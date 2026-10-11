//! 记忆存取服务：put / get / update / delete / search / list / stats / history
//!
//! 所有写操作都经过同一套纪律：
//! 1. scope 校验（token 层面）；
//! 2. 命名空间访问校验（GrantStore 层面，owner / 显式授权 / 通配）；
//! 3. 首次写入自动注册命名空间并授予 owner 权限；
//! 4. 内容变更必然产生一条版本历史（备份与审计的事实来源）。

use crate::access::{validate_key, validate_namespace};
use crate::account_scope::{scope_name, unscope_name, AccountScopedStore};
use crate::algorithm::{
    calculate_item_score, cosine_similarity, keyword_score, recency_score, ItemScoreParams,
    SearchQuery,
};
use crate::domain::{
    MemoryEntry, MemoryFilter, MemoryKind, MemoryStats, MemoryStatus, MemoryVersion, Page, Scored,
    VersionChange,
};
use crate::infra::{AccessChecker, Clock, Embedder, MemoryStore, Retriever, SystemClock};
use async_trait::async_trait;
use common::{AgentId, AuthCtx, HubError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// 单条内容长度上限（字符）
pub const MAX_CONTENT_CHARS: usize = 8_192;
/// 单条标签数上限
pub const MAX_TAGS: usize = 32;
/// 单标签长度上限
pub const MAX_TAG_CHARS: usize = 64;
/// metadata 序列化后体积上限（字节）
pub const MAX_METADATA_BYTES: usize = 16_384;
/// 游标编码分隔符（内容不会包含该控制字符）
const CURSOR_SEP: char = '\u{1}';

/// 默认近期性半衰期（天）
pub const DEFAULT_RECENCY_TAU_DAYS: f32 = 30.0;

/// put 请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutRequest {
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub kind: MemoryKind,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// 生存秒数（None = 不过期；0 = 立即过期由清理任务归档）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<i64>,
}

fn default_confidence() -> f32 {
    1.0
}

/// get 请求：按 ID 或按 key
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRequest {
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// 列表请求
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRequest {
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<MemoryKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statuses: Option<Vec<MemoryStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

fn default_list_limit() -> usize {
    50
}

/// 记忆服务
#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn put(&self, auth: &AuthCtx, req: PutRequest) -> Result<MemoryEntry>;
    async fn get(&self, auth: &AuthCtx, req: GetRequest) -> Result<MemoryEntry>;
    async fn update(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
        patch: crate::domain::MemoryPatch,
    ) -> Result<MemoryEntry>;
    async fn delete(&self, auth: &AuthCtx, namespace: &str, id: Uuid) -> Result<()>;
    async fn search(&self, auth: &AuthCtx, q: SearchQuery) -> Result<Vec<Scored<MemoryEntry>>>;
    async fn list(&self, auth: &AuthCtx, req: ListRequest) -> Result<Page<MemoryEntry>>;
    async fn stats(&self, auth: &AuthCtx, namespace: &str) -> Result<MemoryStats>;
    async fn history(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
    ) -> Result<Vec<MemoryVersion>>;
}

/// 核心记忆服务实现
pub struct DefaultMemoryService {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    access: Arc<dyn AccessChecker>,
    clock: Arc<dyn Clock>,
    retriever: Option<Arc<dyn Retriever>>,
    score_weights: [f32; 5],
}

impl DefaultMemoryService {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<dyn Embedder>,
        access: Arc<dyn AccessChecker>,
        score_weights: [f32; 5],
    ) -> Self {
        Self {
            store,
            embedder,
            access,
            clock: Arc::new(SystemClock),
            retriever: None,
            score_weights,
        }
    }

    /// 注入混合检索器（不注入时退化为内置的纯向量检索）
    pub fn with_retriever(mut self, retriever: Arc<dyn Retriever>) -> Self {
        self.retriever = Some(retriever);
        self
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 本请求的账户作用域存储：一切数据访问都经它代理
    fn scoped(&self, auth: &AuthCtx) -> AccountScopedStore<dyn MemoryStore> {
        AccountScopedStore::new(self.store.clone(), auth.account_id)
    }

    /// 读前置校验
    async fn require_read(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&common::Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_read(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 写前置校验
    async fn require_write(&self, auth: &AuthCtx, namespace: &str) -> Result<()> {
        if !auth.has_scope(&common::Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        if !self.access.can_write(auth, namespace).await? {
            return Err(HubError::InsufficientScope);
        }
        Ok(())
    }

    /// 内容字段统一校验
    fn validate_new(req: &PutRequest) -> Result<()> {
        validate_namespace(&req.namespace)?;
        if let Some(k) = &req.key {
            validate_key(k)?;
        }
        if req.content.trim().is_empty() {
            return Err(HubError::InvalidArgument("content 不得为空".to_string()));
        }
        if req.content.chars().count() > MAX_CONTENT_CHARS {
            return Err(HubError::InvalidArgument(format!(
                "content 长度不得超过 {} 字符",
                MAX_CONTENT_CHARS
            )));
        }
        if req.tags.len() > MAX_TAGS {
            return Err(HubError::InvalidArgument(format!(
                "tags 数量不得超过 {}",
                MAX_TAGS
            )));
        }
        for t in &req.tags {
            if t.chars().count() > MAX_TAG_CHARS {
                return Err(HubError::InvalidArgument(format!(
                    "单个 tag 长度不得超过 {} 字符",
                    MAX_TAG_CHARS
                )));
            }
        }
        if !(0.0..=1.0).contains(&req.confidence) {
            return Err(HubError::InvalidArgument(
                "confidence 必须落在 [0,1]".to_string(),
            ));
        }
        let meta_bytes = serde_json::to_vec(&req.metadata).unwrap_or_default().len();
        if meta_bytes > MAX_METADATA_BYTES {
            return Err(HubError::InvalidArgument(format!(
                "metadata 序列化后不得超过 {} 字节",
                MAX_METADATA_BYTES
            )));
        }
        Ok(())
    }

    /// 计算嵌入（失败不阻塞写入：向量检索降级为关键词）
    async fn embed_content(&self, content: &str) -> (Option<Vec<f32>>, Option<String>) {
        match self.embedder.embed(&[content.to_string()]).await {
            Ok(mut vs) if !vs.is_empty() => {
                let mut v = vs.remove(0);
                crate::algorithm::l2_normalize(&mut v);
                (Some(v), Some(self.embedder.model_id().to_string()))
            }
            _ => (None, None),
        }
    }

    /// 追加版本历史
    async fn record_version(&self, entry: &MemoryEntry, change: VersionChange) -> Result<()> {
        self.store
            .record_version(&MemoryVersion {
                memory_id: entry.id,
                version: entry.version,
                content: entry.content.clone(),
                tags: entry.tags.clone(),
                metadata: entry.metadata.clone(),
                confidence: entry.confidence,
                changed_by: entry.source_agent.clone(),
                change_type: change,
                changed_at: entry.updated_at,
            })
            .await
    }
}

#[async_trait]
impl MemoryService for DefaultMemoryService {
    async fn put(&self, auth: &AuthCtx, req: PutRequest) -> Result<MemoryEntry> {
        Self::validate_new(&req)?;
        // 先注册命名空间（首次写入方成为 owner 并获得授权），再做写权限校验
        self.access.ensure_namespace(auth, &req.namespace).await?;
        self.require_write(auth, &req.namespace).await?;
        let store = self.scoped(auth);

        let now = self.clock.now();

        // upsert 语义：同 namespace + key 已存在则走修订
        if let Some(key) = &req.key {
            if let Some(existing) = store.get_by_key(&req.namespace, key).await? {
                if existing.status == MemoryStatus::Merged {
                    // 已被合并的 key 允许重新启用为全新条目
                } else {
                    let mut updated = existing.clone();
                    updated.content = req.content.clone();
                    updated.tags = req.tags.clone();
                    updated.metadata = req.metadata.clone();
                    updated.confidence = req.confidence;
                    updated.kind = req.kind;
                    if let Some(ttl) = req.ttl_seconds {
                        updated.expires_at = if ttl > 0 {
                            Some(now + chrono::Duration::seconds(ttl))
                        } else {
                            None
                        };
                    }
                    let (emb, model) = self.embed_content(&req.content).await;
                    updated.embedding = emb;
                    updated.embedding_model = model;
                    updated.source_agent = auth.agent_id.clone();
                    updated.version += 1;
                    updated.updated_at = now;
                    if updated.status == MemoryStatus::Archived {
                        updated.status = MemoryStatus::Active;
                    }
                    let saved = store.save(&updated).await?;
                    self.record_version(&saved, VersionChange::Updated).await?;
                    return Ok(saved);
                }
            }
        }

        let (embedding, embedding_model) = self.embed_content(&req.content).await;
        let entry = MemoryEntry {
            id: Uuid::new_v4(),
            namespace: req.namespace.clone(),
            key: req.key.clone(),
            kind: req.kind,
            content: req.content,
            tags: req.tags,
            metadata: req.metadata,
            embedding,
            embedding_model,
            source_agent: auth.agent_id.clone(),
            confidence: req.confidence,
            status: MemoryStatus::Active,
            version: 1,
            expires_at: req
                .ttl_seconds
                .filter(|t| *t > 0)
                .map(|t| now + chrono::Duration::seconds(t)),
            merged_into: None,
            created_at: now,
            updated_at: now,
        };
        let saved = store.insert(&entry).await?;
        self.record_version(&saved, VersionChange::Created).await?;
        Ok(saved)
    }

    async fn get(&self, auth: &AuthCtx, req: GetRequest) -> Result<MemoryEntry> {
        self.require_read(auth, &req.namespace).await?;
        let store = self.scoped(auth);
        if let Some(id) = req.id {
            return store.get(&req.namespace, id).await;
        }
        if let Some(key) = &req.key {
            return store
                .get_by_key(&req.namespace, key)
                .await?
                .ok_or(HubError::NotFound);
        }
        Err(HubError::InvalidArgument(
            "必须提供 id 或 key 之一".to_string(),
        ))
    }

    async fn update(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
        patch: crate::domain::MemoryPatch,
    ) -> Result<MemoryEntry> {
        self.require_write(auth, namespace).await?;
        let store = self.scoped(auth);
        let mut entry = store.get(namespace, id).await?;

        let mut content_changed = false;
        if let Some(content) = patch.content {
            if content.trim().is_empty() {
                return Err(HubError::InvalidArgument("content 不得为空".to_string()));
            }
            if content.chars().count() > MAX_CONTENT_CHARS {
                return Err(HubError::InvalidArgument(format!(
                    "content 长度不得超过 {} 字符",
                    MAX_CONTENT_CHARS
                )));
            }
            entry.content = content;
            content_changed = true;
        }
        if let Some(kind) = patch.kind {
            entry.kind = kind;
        }
        if let Some(tags) = patch.tags {
            if tags.len() > MAX_TAGS {
                return Err(HubError::InvalidArgument(format!(
                    "tags 数量不得超过 {}",
                    MAX_TAGS
                )));
            }
            entry.tags = tags;
        }
        if let Some(metadata) = patch.metadata {
            let bytes = serde_json::to_vec(&metadata).unwrap_or_default().len();
            if bytes > MAX_METADATA_BYTES {
                return Err(HubError::InvalidArgument(format!(
                    "metadata 序列化后不得超过 {} 字节",
                    MAX_METADATA_BYTES
                )));
            }
            entry.metadata = metadata;
        }
        if let Some(conf) = patch.confidence {
            if !(0.0..=1.0).contains(&conf) {
                return Err(HubError::InvalidArgument(
                    "confidence 必须落在 [0,1]".to_string(),
                ));
            }
            entry.confidence = conf;
        }
        match patch.expires_in_seconds {
            Some(n) if n > 0 => {
                entry.expires_at = Some(self.clock.now() + chrono::Duration::seconds(n))
            }
            Some(_) => entry.expires_at = None,
            None => {}
        }

        let now = self.clock.now();
        entry.source_agent = auth.agent_id.clone();
        entry.version += 1;
        entry.updated_at = now;
        if content_changed {
            let (emb, model) = self.embed_content(&entry.content).await;
            entry.embedding = emb;
            entry.embedding_model = model;
        }
        let saved = store.save(&entry).await?;
        self.record_version(&saved, VersionChange::Updated).await?;
        Ok(saved)
    }

    async fn delete(&self, auth: &AuthCtx, namespace: &str, id: Uuid) -> Result<()> {
        self.require_write(auth, namespace).await?;
        let store = self.scoped(auth);
        let mut entry = store.get(namespace, id).await?;
        if entry.status != MemoryStatus::Active {
            return Err(HubError::InvalidArgument(format!(
                "条目已处于 {} 状态",
                entry.status.as_str()
            )));
        }
        entry.status = MemoryStatus::Archived;
        entry.version += 1;
        entry.updated_at = self.clock.now();
        entry.source_agent = auth.agent_id.clone();
        let saved = store.save(&entry).await?;
        self.record_version(&saved, VersionChange::Archived).await?;
        Ok(())
    }

    async fn search(&self, auth: &AuthCtx, mut q: SearchQuery) -> Result<Vec<Scored<MemoryEntry>>> {
        self.require_read(auth, &q.namespace).await?;
        if let Some(retriever) = &self.retriever {
            // 检索器直接在存储上按命名空间键工作：查询前作用域化、返回后剥离
            q.namespace = scope_name(&auth.account_id, &q.namespace);
            let mut rows = retriever.search(auth, &q).await?;
            for s in &mut rows {
                s.item.namespace = unscope_name(&auth.account_id, &s.item.namespace);
            }
            return Ok(rows);
        }
        // 退化路径：纯向量近邻 + 本地加权重排
        let mut out = Vec::new();
        if q.query_text.trim().is_empty() {
            return Ok(out);
        }
        let query_vec = self
            .embedder
            .embed(std::slice::from_ref(&q.query_text))
            .await?
            .into_iter()
            .next();
        let Some(query_vec) = query_vec else {
            return Ok(out);
        };
        let mut qv = query_vec;
        crate::algorithm::l2_normalize(&mut qv);
        let now = self.clock.now();
        let store = self.scoped(auth);
        let mut scored: Vec<Scored<MemoryEntry>> = store
            .similar(&q.namespace, &qv, q.top_k.max(1), q.kinds.as_deref())
            .await?
            .into_iter()
            .map(|s| {
                let kw = keyword_score(&s.item.content, &q.query_text, &s.item.tags);
                let rec = recency_score(now, s.item.updated_at, DEFAULT_RECENCY_TAU_DAYS);
                let score = calculate_item_score(
                    &ItemScoreParams {
                        sim: s.score.max(0.0),
                        kw,
                        conf: s.item.confidence,
                        rec,
                        fresh: if s.item.version > 1 { 1.0 } else { 0.5 },
                    },
                    &self.score_weights,
                );
                Scored {
                    item: s.item,
                    score,
                }
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(q.top_k.max(1));
        out.extend(scored);
        Ok(out)
    }

    async fn list(&self, auth: &AuthCtx, req: ListRequest) -> Result<Page<MemoryEntry>> {
        self.require_read(auth, &req.namespace).await?;
        let limit = req.limit.clamp(1, 200);
        let before = match &req.cursor {
            Some(c) => Some(decode_cursor(c)?),
            None => None,
        };
        let filter = MemoryFilter {
            namespace: Some(req.namespace.clone()),
            kinds: req.kinds.clone(),
            statuses: req.statuses.clone(),
            tags: req.tags.clone(),
            source_agent: req.source_agent.clone(),
            key: None,
            text_contains: req.text_contains.clone(),
            before,
            limit: Some(limit + 1),
        };
        let mut items = self.scoped(auth).list(&filter).await?;
        let has_more = items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let next_cursor = if has_more {
            items.last().map(|e| encode_cursor(e.updated_at, e.id))
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn stats(&self, auth: &AuthCtx, namespace: &str) -> Result<MemoryStats> {
        self.require_read(auth, namespace).await?;
        let all = self.scoped(auth).all_in_namespace(namespace, true).await?;
        let mut stats = MemoryStats {
            namespace: namespace.to_string(),
            total: all.len(),
            ..Default::default()
        };
        let now = self.clock.now();
        let soon = now + chrono::Duration::hours(24);
        for e in &all {
            match e.status {
                MemoryStatus::Active => stats.active += 1,
                MemoryStatus::Archived => stats.archived += 1,
                MemoryStatus::Merged => stats.merged += 1,
            }
            *stats
                .by_kind
                .entry(e.kind.as_str().to_string())
                .or_insert(0) += 1;
            *stats.by_agent.entry(e.source_agent.clone()).or_insert(0) += 1;
            if e.status == MemoryStatus::Active {
                if let Some(exp) = e.expires_at {
                    if exp <= soon {
                        stats.expiring_24h += 1;
                    }
                }
            }
        }
        Ok(stats)
    }

    async fn history(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
    ) -> Result<Vec<MemoryVersion>> {
        self.require_read(auth, namespace).await?;
        // 先确认条目存在且归属该命名空间（账户作用域内）
        let store = self.scoped(auth);
        store.get(namespace, id).await?;
        store.versions(id).await
    }
}

/// 内置纯向量简易检索器（未注入混合检索器时的退化实现）
pub struct SimpleRetriever {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    score_weights: [f32; 5],
}

impl SimpleRetriever {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<dyn Embedder>,
        score_weights: [f32; 5],
    ) -> Self {
        Self {
            store,
            embedder,
            score_weights,
        }
    }
}

#[async_trait]
impl Retriever for SimpleRetriever {
    async fn search(&self, _auth: &AuthCtx, q: &SearchQuery) -> Result<Vec<Scored<MemoryEntry>>> {
        // 命名空间访问权由 MemoryService 统一把关，检索器内部不再重复校验
        let query_vec = if q.query_text.trim().is_empty() {
            None
        } else {
            self.embedder
                .embed(std::slice::from_ref(&q.query_text))
                .await?
                .into_iter()
                .next()
        };

        let candidates: Vec<MemoryEntry> = match &query_vec {
            Some(qv) => self
                .store
                .similar(&q.namespace, qv, q.top_k.max(1) * 2, q.kinds.as_deref())
                .await?
                .into_iter()
                .map(|s| s.item)
                .collect(),
            None => {
                let filter = MemoryFilter {
                    namespace: Some(q.namespace.clone()),
                    kinds: q.kinds.clone(),
                    statuses: Some(if q.include_archived {
                        vec![MemoryStatus::Active, MemoryStatus::Archived]
                    } else {
                        vec![MemoryStatus::Active]
                    }),
                    limit: Some(q.top_k.max(1)),
                    ..Default::default()
                };
                self.store.list(&filter).await?
            }
        };

        let now = chrono::Utc::now();
        let mut scored: Vec<Scored<MemoryEntry>> = candidates
            .into_iter()
            .map(|item| {
                let sim = query_vec
                    .as_ref()
                    .and_then(|qv| item.embedding.as_ref().map(|iv| cosine_similarity(qv, iv)))
                    .unwrap_or(0.0);
                let kw = keyword_score(&item.content, &q.query_text, &item.tags);
                let rec = recency_score(now, item.updated_at, DEFAULT_RECENCY_TAU_DAYS);
                let fresh = if item.version > 1 { 1.0 } else { 0.5 };
                let score = calculate_item_score(
                    &ItemScoreParams {
                        sim: sim.max(0.0),
                        kw,
                        conf: item.confidence,
                        rec,
                        fresh,
                    },
                    &self.score_weights,
                );
                Scored { item, score }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(q.top_k.max(1));
        Ok(scored)
    }
}

/// 游标编码：`updated_at_rfc3339 \x01 id`
pub fn encode_cursor(updated_at: chrono::DateTime<chrono::Utc>, id: Uuid) -> String {
    use base64::Engine as _;
    let raw = format!("{}{}{}", updated_at.to_rfc3339(), CURSOR_SEP, id);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes())
}

/// 游标解码
pub fn decode_cursor(cursor: &str) -> Result<(chrono::DateTime<chrono::Utc>, Uuid)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .map_err(|_| HubError::InvalidArgument("cursor 不是合法 base64url".to_string()))?;
    let raw = String::from_utf8(bytes)
        .map_err(|_| HubError::InvalidArgument("cursor 解码失败".to_string()))?;
    let (ts, id) = raw
        .split_once(CURSOR_SEP)
        .ok_or_else(|| HubError::InvalidArgument("cursor 格式不正确".to_string()))?;
    let updated_at = chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|_| HubError::InvalidArgument("cursor 时间部分不正确".to_string()))?;
    let id = Uuid::parse_str(id)
        .map_err(|_| HubError::InvalidArgument("cursor ID 部分不正确".to_string()))?;
    Ok((updated_at, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// 测试账户作用域名：与 `auth()`/`writer()` 辅助中的账户一致
    fn scoped_ns(ns: &str) -> String {
        crate::account_scope::scope_name(&common::BOOTSTRAP_ACCOUNT_ID, ns)
    }
    use crate::infra::{GrantStore, MemoryHubStore};
    use common::Scope;
    use std::collections::HashMap as StdHashMap;
    use std::sync::RwLock;

    /// 内存 Grant 存储（测试用）
    #[derive(Default)]
    struct TestGrants {
        namespaces: RwLock<StdHashMap<String, crate::domain::NamespaceInfo>>,
        grants: RwLock<Vec<crate::domain::NamespaceGrantRecord>>,
    }

    #[async_trait]
    impl GrantStore for TestGrants {
        async fn ensure_namespace(
            &self,
            ns: &str,
            owner: &AgentId,
        ) -> Result<(crate::domain::NamespaceInfo, bool)> {
            let mut g = self.namespaces.write().unwrap();
            let mut created = false;
            let info = g
                .entry(ns.to_string())
                .or_insert_with(|| {
                    created = true;
                    crate::domain::NamespaceInfo {
                        namespace: ns.to_string(),
                        owner_agent: owner.clone(),
                        created_at: chrono::Utc::now(),
                    }
                })
                .clone();
            Ok((info, created))
        }
        async fn get_namespace(&self, ns: &str) -> Result<Option<crate::domain::NamespaceInfo>> {
            Ok(self.namespaces.read().unwrap().get(ns).cloned())
        }
        async fn list_namespace_infos(&self) -> Result<Vec<crate::domain::NamespaceInfo>> {
            Ok(self.namespaces.read().unwrap().values().cloned().collect())
        }
        async fn grant(&self, record: &crate::domain::NamespaceGrantRecord) -> Result<()> {
            let mut g = self.grants.write().unwrap();
            g.retain(|x| !(x.namespace == record.namespace && x.agent_id == record.agent_id));
            g.push(record.clone());
            Ok(())
        }
        async fn revoke(&self, ns: &str, agent: &AgentId) -> Result<()> {
            let mut g = self.grants.write().unwrap();
            g.retain(|x| !(x.namespace == ns && *agent == x.agent_id));
            Ok(())
        }
        async fn grants_for(
            &self,
            agent: &AgentId,
        ) -> Result<Vec<crate::domain::NamespaceGrantRecord>> {
            Ok(self
                .grants
                .read()
                .unwrap()
                .iter()
                .filter(|x| &x.agent_id == agent)
                .cloned()
                .collect())
        }
        async fn can_read(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            Ok(self.grants.read().unwrap().iter().any(|x| {
                (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_read
            }))
        }
        async fn can_write(&self, ns: &str, agent: &AgentId) -> Result<bool> {
            Ok(self.grants.read().unwrap().iter().any(|x| {
                (x.namespace == ns || x.namespace == "*") && x.agent_id == *agent && x.can_write
            }))
        }
        async fn owned_namespaces(&self, agent: &AgentId) -> Result<Vec<String>> {
            Ok(self
                .namespaces
                .read()
                .unwrap()
                .values()
                .filter(|n| &n.owner_agent == agent)
                .map(|n| n.namespace.clone())
                .collect())
        }
    }

    type TestStore = RwLock<StdHashMap<(String, Uuid), MemoryEntry>>;

    /// 内存 MemoryStore（测试用，仅覆盖本测试所需路径）
    #[derive(Default)]
    struct TestMemoryStore {
        items: TestStore,
        versions: RwLock<Vec<MemoryVersion>>,
    }

    #[async_trait]
    impl MemoryStore for TestMemoryStore {
        async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            Ok(entry.clone())
        }
        async fn get(&self, ns: &str, id: Uuid) -> Result<MemoryEntry> {
            self.items
                .read()
                .unwrap()
                .get(&(ns.to_string(), id))
                .cloned()
                .ok_or(HubError::NotFound)
        }
        async fn get_by_key(&self, ns: &str, key: &str) -> Result<Option<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .find(|e| e.namespace == ns && e.key.as_deref() == Some(key))
                .cloned())
        }
        async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
            self.items
                .write()
                .unwrap()
                .insert((entry.namespace.clone(), entry.id), entry.clone());
            Ok(entry.clone())
        }
        async fn delete(&self, ns: &str, id: Uuid) -> Result<()> {
            self.items.write().unwrap().remove(&(ns.to_string(), id));
            Ok(())
        }
        async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
            let mut out: Vec<MemoryEntry> = self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| filter.namespace.as_deref().is_none_or(|ns| e.namespace == ns))
                .filter(|e| filter.kinds.as_ref().is_none_or(|ks| ks.contains(&e.kind)))
                .filter(|e| filter.statuses.as_ref().is_none_or(|ss| ss.contains(&e.status)))
                .filter(|e| {
                    filter
                        .source_agent
                        .as_ref()
                        .is_none_or(|a| &e.source_agent == a)
                })
                .filter(|e| {
                    filter.text_contains.as_ref().is_none_or(|t| {
                        e.content.to_lowercase().contains(&t.to_lowercase())
                    })
                })
                // 游标分页：严格位于 (updated_at, id) 复合键之前
                .filter(|e| match &filter.before {
                    Some((ts, id)) => {
                        (e.updated_at, e.id) < (*ts, *id)
                    }
                    None => true,
                })
                .cloned()
                .collect();
            out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
            if let Some(limit) = filter.limit {
                out.truncate(limit);
            }
            Ok(out)
        }
        async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
            Ok(self.list(filter).await?.len())
        }
        async fn similar(
            &self,
            ns: &str,
            qv: &[f32],
            k: usize,
            _kinds: Option<&[MemoryKind]>,
        ) -> Result<Vec<Scored<MemoryEntry>>> {
            let mut out: Vec<Scored<MemoryEntry>> = self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns && e.status == MemoryStatus::Active)
                .filter_map(|e| {
                    e.embedding.as_ref().map(|iv| Scored {
                        item: e.clone(),
                        score: cosine_similarity(qv, iv),
                    })
                })
                .collect();
            out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
            out.truncate(k);
            Ok(out)
        }
        async fn all_in_namespace(
            &self,
            ns: &str,
            include_merged: bool,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(self
                .items
                .read()
                .unwrap()
                .values()
                .filter(|e| e.namespace == ns)
                .filter(|e| include_merged || e.status != MemoryStatus::Merged)
                .cloned()
                .collect())
        }
        async fn list_namespaces(&self) -> Result<Vec<String>> {
            let mut ns: Vec<String> = self
                .items
                .read()
                .unwrap()
                .values()
                .map(|e| e.namespace.clone())
                .collect();
            ns.sort();
            ns.dedup();
            Ok(ns)
        }
        async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
            self.versions.write().unwrap().push(v.clone());
            Ok(())
        }
        async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
            Ok(self
                .versions
                .read()
                .unwrap()
                .iter()
                .filter(|v| v.memory_id == id)
                .cloned()
                .collect())
        }
        async fn archive_expired(&self, _now: chrono::DateTime<chrono::Utc>) -> Result<usize> {
            Ok(0)
        }
    }

    use crate::access::GrantChecker;

    fn test_embedder() -> Arc<dyn Embedder> {
        Arc::new(crate::testing::TestEmbedder::default())
    }

    fn auth(agent: &str, scopes: Vec<Scope>) -> AuthCtx {
        AuthCtx {
            agent_id: agent.to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes,
            acl: Default::default(),
        }
    }

    async fn service() -> (DefaultMemoryService, Arc<TestMemoryStore>, Arc<TestGrants>) {
        let grants = Arc::new(TestGrants::default());
        let mem = Arc::new(TestMemoryStore::default());
        let svc = DefaultMemoryService::new(
            mem.clone(),
            test_embedder(),
            Arc::new(GrantChecker::new(grants.clone())),
            [0.5, 0.2, 0.1, 0.1, 0.1],
        );
        (svc, mem, grants)
    }

    fn put_req(ns: &str, content: &str) -> PutRequest {
        PutRequest {
            namespace: ns.to_string(),
            key: None,
            kind: MemoryKind::Fact,
            content: content.to_string(),
            tags: vec![],
            metadata: serde_json::json!({}),
            confidence: 1.0,
            ttl_seconds: None,
        }
    }

    #[tokio::test]
    async fn test_put_get_roundtrip_and_version_history() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);

        let entry = svc
            .put(&a, put_req("team-x", "项目使用 PostgreSQL 15"))
            .await
            .unwrap();
        assert_eq!(entry.version, 1);
        assert_eq!(entry.source_agent, "agent-a");

        let got = svc
            .get(
                &a,
                GetRequest {
                    namespace: "team-x".to_string(),
                    id: Some(entry.id),
                    key: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(got.content, "项目使用 PostgreSQL 15");

        let history = svc.history(&a, "team-x", entry.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].change_type, VersionChange::Created);
    }

    #[tokio::test]
    async fn test_put_upsert_by_key_bumps_version() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);

        let first = svc
            .put(
                &a,
                PutRequest {
                    key: Some("db:engine".to_string()),
                    ..put_req("team-x", "使用 MySQL")
                },
            )
            .await
            .unwrap();

        let second = svc
            .put(
                &a,
                PutRequest {
                    key: Some("db:engine".to_string()),
                    ..put_req("team-x", "使用 PostgreSQL 15")
                },
            )
            .await
            .unwrap();

        assert_eq!(first.id, second.id, "同 key 必须 upsert 到同一条目");
        assert_eq!(second.version, 2);
        assert_eq!(second.content, "使用 PostgreSQL 15");
    }

    #[tokio::test]
    async fn test_namespace_isolation_between_agents() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);
        let b = auth("agent-b", vec![Scope::Read, Scope::Write, Scope::Admin]);

        svc.put(&a, put_req("ns-a", "A 的私有记忆")).await.unwrap();

        // agent-b 无权读 ns-a
        let err = svc
            .get(
                &b,
                GetRequest {
                    namespace: "ns-a".to_string(),
                    id: Some(uuid::Uuid::new_v4()),
                    key: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));

        // agent-b 也无权写
        let err = svc.put(&b, put_req("ns-a", "B 的写入")).await.unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    #[tokio::test]
    async fn test_scope_without_write_rejected() {
        let (svc, _mem, _grants) = service().await;
        let reader = auth("agent-a", vec![Scope::Read]);
        let err = svc.put(&reader, put_req("ns", "x")).await.unwrap_err();
        assert!(matches!(err, HubError::InsufficientScope));
    }

    #[tokio::test]
    async fn test_update_and_delete() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);
        let entry = svc.put(&a, put_req("ns", "初始内容")).await.unwrap();

        let patched = svc
            .update(
                &a,
                "ns",
                entry.id,
                crate::domain::MemoryPatch {
                    content: Some("更新后的内容".to_string()),
                    confidence: Some(0.8),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.version, 2);
        assert_eq!(patched.confidence, 0.8);

        svc.delete(&a, "ns", entry.id).await.unwrap();
        let got = svc
            .get(
                &a,
                GetRequest {
                    namespace: "ns".to_string(),
                    id: Some(entry.id),
                    key: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(got.status, MemoryStatus::Archived);
    }

    #[tokio::test]
    async fn test_validation_rejects_bad_input() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);

        assert!(svc.put(&a, put_req("Bad NS", "x")).await.is_err());
        assert!(svc.put(&a, put_req("ns", "   ")).await.is_err());
        let mut bad_conf = put_req("ns", "x");
        bad_conf.confidence = 1.5;
        assert!(svc.put(&a, bad_conf).await.is_err());
    }

    #[tokio::test]
    async fn test_stats_counts_by_status_and_kind() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);
        let e1 = svc.put(&a, put_req("ns", "事实一")).await.unwrap();
        svc.put(
            &a,
            PutRequest {
                kind: MemoryKind::Preference,
                ..put_req("ns", "偏好一")
            },
        )
        .await
        .unwrap();
        svc.delete(&a, "ns", e1.id).await.unwrap();

        let stats = svc.stats(&a, "ns").await.unwrap();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.active, 1);
        assert_eq!(stats.archived, 1);
        assert_eq!(stats.by_kind.get("fact"), Some(&1));
        assert_eq!(stats.by_kind.get("preference"), Some(&1));
    }

    #[tokio::test]
    async fn test_list_pagination_cursor() {
        let (svc, _mem, _grants) = service().await;
        let a = auth("agent-a", vec![Scope::Read, Scope::Write, Scope::Admin]);
        for i in 0..5 {
            svc.put(&a, put_req("ns", &format!("条目 {}", i)))
                .await
                .unwrap();
        }

        let page1 = svc
            .list(
                &a,
                ListRequest {
                    namespace: "ns".to_string(),
                    limit: 2,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page1.items.len(), 2);
        assert!(page1.next_cursor.is_some());

        let page2 = svc
            .list(
                &a,
                ListRequest {
                    namespace: "ns".to_string(),
                    limit: 2,
                    cursor: page1.next_cursor.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page2.items.len(), 2);

        // 两页不得重叠
        let ids1: Vec<Uuid> = page1.items.iter().map(|e| e.id).collect();
        assert!(page2.items.iter().all(|e| !ids1.contains(&e.id)));
    }

    #[test]
    fn test_cursor_roundtrip() {
        let now = chrono::Utc::now();
        let id = Uuid::new_v4();
        let cursor = encode_cursor(now, id);
        let (ts, got_id) = decode_cursor(&cursor).unwrap();
        assert_eq!(id, got_id);
        assert_eq!(ts.timestamp(), now.timestamp());
        assert!(decode_cursor("!!!not-base64!!!").is_err());
    }

    #[tokio::test]
    async fn test_owner_auto_registered_on_first_write() {
        let (svc, _mem, grants) = service().await;
        let a = auth("agent-a", vec![Scope::Write]);
        svc.put(&a, put_req("fresh-ns", "第一条")).await.unwrap();

        let info = grants.get_namespace(&scoped_ns("fresh-ns")).await.unwrap().unwrap();
        assert_eq!(info.owner_agent, "agent-a");
        assert!(grants
            .can_read(&scoped_ns("fresh-ns"), &"agent-a".to_string())
            .await
            .unwrap());
        assert!(grants
            .can_write(&scoped_ns("fresh-ns"), &"agent-a".to_string())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_simple_retriever_ranks_relevant_first() {
        let grants = Arc::new(TestGrants::default());
        let mem = Arc::new(TestMemoryStore::default());
        let svc = DefaultMemoryService::new(
            mem.clone(),
            test_embedder(),
            Arc::new(GrantChecker::new(grants.clone())),
            [0.9, 0.1, 0.0, 0.0, 0.0],
        )
        .with_retriever(Arc::new(SimpleRetriever::new(
            mem,
            test_embedder(),
            [0.9, 0.1, 0.0, 0.0, 0.0],
        )));
        let a = auth("agent-a", vec![Scope::Read, Scope::Write]);

        svc.put(&a, put_req("ns", "用户喜欢喝美式咖啡"))
            .await
            .unwrap();
        svc.put(&a, put_req("ns", "服务器部署在东京区域"))
            .await
            .unwrap();

        let res = svc
            .search(
                &a,
                SearchQuery {
                    namespace: "ns".to_string(),
                    query_text: "咖啡".to_string(),
                    kinds: None,
                    tags: None,
                    top_k: 2,
                    include_archived: false,
                },
            )
            .await
            .unwrap();
        assert_eq!(res.len(), 2);
        assert_eq!(res[0].item.content, "用户喜欢喝美式咖啡");
    }

    // 保证 MemoryHubStore 聚合 trait 可被满足（编译期检查）
    #[allow(dead_code)]
    fn assert_hub_store<T: MemoryHubStore>() {}
}
