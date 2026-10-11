//! MCP 工具执行器与双模输出生成
//!
//! 16 个工具全部围绕三件事：**存储**（put/get/update/delete/search/list/stats/history）、
//! **备份**（backup_create/backup_list/restore）、**整合**（integrate_scan/
//! integration_list/integration_resolve），外加命名空间共享（grant/revoke）。
//!
//! 输出同时携带 `structuredContent`（结构化）与文本通道（兼容 JSON 字符串），
//! 错误一律以 `isError = true` + 标准错误码对象返回（MCP 规定工具错误仍在 200 内表达）。

use common::{AgentId, AuthCtx, HubError, Scope};
use mem_core::{BackupService, GrantStore, IntegrationService, MemoryService, NamespaceGrantRecord};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// MCP 内容项（兼容旧客户端的文本通道）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpContentItem {
    Text { text: String },
}

/// 标准 MCP 工具调用结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    #[serde(rename = "structuredContent", skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    pub content: Vec<McpContentItem>,
}

impl ToolCallResult {
    /// 成功响应（同时返回结构化对象与兼容 JSON 字符串）
    pub fn success(val: Value) -> Self {
        let json_text = serde_json::to_string(&val).unwrap_or_default();
        Self {
            is_error: false,
            structured_content: Some(val),
            content: vec![McpContentItem::Text { text: json_text }],
        }
    }

    /// 错误响应（isError = true，返回标准错误码对象）
    pub fn error(err: &HubError) -> Self {
        let detail = err.to_error_detail();
        let val = serde_json::to_value(&detail).unwrap_or_default();
        let json_text = serde_json::to_string(&val).unwrap_or_default();
        Self {
            is_error: true,
            structured_content: Some(val),
            content: vec![McpContentItem::Text { text: json_text }],
        }
    }
}

/// 从序列化值中剔除 embedding（向量体积大且无调用价值）
fn strip_embedding(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("embedding");
    }
    v
}

/// MCP 工具调用调度器
#[derive(Clone)]
pub struct ToolDispatcher {
    pub memory: Arc<dyn MemoryService>,
    pub backup: Arc<dyn BackupService>,
    pub integration: Arc<dyn IntegrationService>,
    /// 底层存储（授权工具用；所有访问经账户作用域包装器）
    pub store: Arc<dyn mem_core::MemoryHubStore>,
}

impl ToolDispatcher {
    pub fn new(
        memory: Arc<dyn MemoryService>,
        backup: Arc<dyn BackupService>,
        integration: Arc<dyn IntegrationService>,
        store: Arc<dyn mem_core::MemoryHubStore>,
    ) -> Self {
        Self {
            memory,
            backup,
            integration,
            store,
        }
    }

    /// 分发执行
    pub async fn dispatch(&self, auth: &AuthCtx, name: &str, arguments: Value) -> ToolCallResult {
        match name {
            "memory_put" => self.put(auth, arguments).await,
            "memory_get" => self.get(auth, arguments).await,
            "memory_update" => self.update(auth, arguments).await,
            "memory_delete" => self.delete(auth, arguments).await,
            "memory_search" => self.search(auth, arguments).await,
            "memory_list" => self.list(auth, arguments).await,
            "memory_stats" => self.stats(auth, arguments).await,
            "memory_history" => self.history(auth, arguments).await,
            "memory_backup_create" => self.backup_create(auth, arguments).await,
            "memory_backup_list" => self.backup_list(auth, arguments).await,
            "memory_restore" => self.restore(auth, arguments).await,
            "memory_integrate_scan" => self.integrate_scan(auth, arguments).await,
            "memory_integration_list" => self.integration_list(auth, arguments).await,
            "memory_integration_resolve" => self.integration_resolve(auth, arguments).await,
            "memory_grant" => self.grant(auth, arguments).await,
            "memory_revoke" => self.revoke(auth, arguments).await,
            other => {
                ToolCallResult::error(&HubError::MethodNotFound(format!("未知工具: {}", other)))
            }
        }
    }

    // ===== 存储 =====

    async fn put(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::PutRequest>(args) {
            Ok(req) => match self.memory.put(auth, req).await {
                Ok(entry) => ToolCallResult::success(json!({
                    "memory": strip_embedding(serde_json::to_value(&entry).unwrap_or(Value::Null)),
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_put schema: {}",
                e
            ))),
        }
    }

    async fn get(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::GetRequest>(args) {
            Ok(req) => match self.memory.get(auth, req).await {
                Ok(entry) => ToolCallResult::success(json!({
                    "memory": strip_embedding(serde_json::to_value(&entry).unwrap_or(Value::Null)),
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_get schema: {}",
                e
            ))),
        }
    }

    async fn update(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            id: uuid::Uuid,
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            kind: Option<mem_core::MemoryKind>,
            #[serde(default)]
            tags: Option<Vec<String>>,
            #[serde(default)]
            metadata: Option<Value>,
            #[serde(default)]
            confidence: Option<f32>,
            #[serde(default)]
            expires_in_seconds: Option<i64>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let patch = mem_core::MemoryPatch {
                    content: req.content,
                    kind: req.kind,
                    tags: req.tags,
                    metadata: req.metadata,
                    confidence: req.confidence,
                    expires_in_seconds: req.expires_in_seconds,
                };
                match self
                    .memory
                    .update(auth, &req.namespace, req.id, patch)
                    .await
                {
                    Ok(entry) => ToolCallResult::success(json!({
                        "memory": strip_embedding(serde_json::to_value(&entry).unwrap_or(Value::Null)),
                    })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_update schema: {}",
                e
            ))),
        }
    }

    async fn delete(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            id: uuid::Uuid,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.memory.delete(auth, &req.namespace, req.id).await {
                Ok(()) => ToolCallResult::success(json!({
                    "deleted": true,
                    "id": req.id,
                    "namespace": req.namespace,
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_delete schema: {}",
                e
            ))),
        }
    }

    async fn search(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            #[serde(default)]
            query_text: String,
            #[serde(default)]
            kinds: Option<Vec<mem_core::MemoryKind>>,
            #[serde(default)]
            tags: Option<Vec<String>>,
            #[serde(default = "default_top_k")]
            top_k: usize,
            #[serde(default)]
            include_archived: bool,
        }
        fn default_top_k() -> usize {
            10
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let q = mem_core::SearchQuery {
                    namespace: req.namespace,
                    query_text: req.query_text,
                    kinds: req.kinds,
                    tags: req.tags,
                    top_k: req.top_k,
                    include_archived: req.include_archived,
                };
                match self.memory.search(auth, q).await {
                    Ok(results) => {
                        let items: Vec<Value> = results
                            .into_iter()
                            .map(|s| {
                                json!({
                                    "score": s.score,
                                    "memory": strip_embedding(
                                        serde_json::to_value(&s.item).unwrap_or(Value::Null)
                                    ),
                                })
                            })
                            .collect();
                        ToolCallResult::success(json!({ "results": items }))
                    }
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_search schema: {}",
                e
            ))),
        }
    }

    async fn list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::ListRequest>(args) {
            Ok(req) => match self.memory.list(auth, req).await {
                Ok(page) => {
                    let items: Vec<Value> = page
                        .items
                        .into_iter()
                        .map(|e| strip_embedding(serde_json::to_value(&e).unwrap_or(Value::Null)))
                        .collect();
                    ToolCallResult::success(json!({
                        "items": items,
                        "nextCursor": page.next_cursor,
                    }))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_list schema: {}",
                e
            ))),
        }
    }

    async fn stats(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.memory.stats(auth, &req.namespace).await {
                Ok(stats) => {
                    ToolCallResult::success(serde_json::to_value(&stats).unwrap_or(Value::Null))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_stats schema: {}",
                e
            ))),
        }
    }

    async fn history(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            id: uuid::Uuid,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.memory.history(auth, &req.namespace, req.id).await {
                Ok(versions) => ToolCallResult::success(json!({
                    "versions": versions,
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_history schema: {}",
                e
            ))),
        }
    }

    // ===== 备份 =====

    async fn backup_create(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::BackupCreateRequest>(args) {
            Ok(req) => match self.backup.create(auth, req).await {
                Ok(record) => {
                    let summary = serde_json::to_value(mem_core::BackupSummary::from(&record))
                        .unwrap_or(Value::Null);
                    ToolCallResult::success(json!({ "backup": summary }))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_backup_create schema: {}",
                e
            ))),
        }
    }

    async fn backup_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            #[serde(default)]
            namespace: Option<String>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.backup.list(auth, req.namespace.as_deref()).await {
                Ok(list) => ToolCallResult::success(json!({ "backups": list })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_backup_list schema: {}",
                e
            ))),
        }
    }

    async fn restore(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::RestoreRequest>(args) {
            Ok(req) => match self.backup.restore(auth, req).await {
                Ok(report) => {
                    ToolCallResult::success(serde_json::to_value(&report).unwrap_or(Value::Null))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_restore schema: {}",
                e
            ))),
        }
    }

    // ===== 整合 =====

    async fn integrate_scan(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::ScanRequest>(args) {
            Ok(req) => match self.integration.scan(auth, req).await {
                Ok(report) => {
                    ToolCallResult::success(serde_json::to_value(&report).unwrap_or(Value::Null))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_integrate_scan schema: {}",
                e
            ))),
        }
    }

    async fn integration_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.integration.list(auth, &req.namespace).await {
                Ok(list) => ToolCallResult::success(json!({ "proposals": list })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_integration_list schema: {}",
                e
            ))),
        }
    }

    async fn integration_resolve(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::ResolveRequest>(args) {
            Ok(req) => match self.integration.resolve(auth, req).await {
                Ok(entry) => ToolCallResult::success(json!({
                    "survivor": strip_embedding(serde_json::to_value(&entry).unwrap_or(Value::Null)),
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_integration_resolve schema: {}",
                e
            ))),
        }
    }

    // ===== 命名空间共享 =====

    async fn grant(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            agent_id: AgentId,
            #[serde(default = "default_true")]
            can_read: bool,
            #[serde(default)]
            can_write: bool,
        }
        fn default_true() -> bool {
            true
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_grant schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Admin) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        // 沙箱语义：通配授权已废除，授权必须落在具体命名空间
        if req.namespace == "*" {
            return ToolCallResult::error(&HubError::InvalidArgument(
                "通配命名空间授权已废除（账户沙箱）：请对具体命名空间授权".to_string(),
            ));
        }
        if let Err(e) = mem_core::validate_namespace(&req.namespace) {
            return ToolCallResult::error(&e);
        }
        // 目标 Agent 必须与调用方同属一个账户
        match self.store.get_agent(&req.agent_id).await {
            Ok(Some(target)) if target.owner_account_id == auth.account_id => {}
            _ => {
                return ToolCallResult::error(&HubError::InvalidArgument(
                    "目标 Agent 不存在或不属于本账户".to_string(),
                ))
            }
        }
        // 仅命名空间 owner 可转授
        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        let is_owner = scoped
            .get_namespace(&req.namespace)
            .await
            .ok()
            .flatten()
            .map(|info| info.owner_agent == auth.agent_id)
            .unwrap_or(false);
        if !is_owner {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        let record = NamespaceGrantRecord {
            namespace: req.namespace.clone(),
            agent_id: req.agent_id.clone(),
            can_read: req.can_read,
            can_write: req.can_write,
            granted_by: auth.agent_id.clone(),
            created_at: chrono::Utc::now(),
        };
        match scoped.grant(&record).await {
            Ok(()) => ToolCallResult::success(json!({
                "granted": true,
                "namespace": req.namespace,
                "agent_id": req.agent_id,
                "can_read": req.can_read,
                "can_write": req.can_write,
            })),
            Err(e) => ToolCallResult::error(&e),
        }
    }

    async fn revoke(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            agent_id: AgentId,
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_revoke schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Admin) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        if req.namespace == "*" {
            return ToolCallResult::error(&HubError::InvalidArgument(
                "通配命名空间授权已废除（账户沙箱）".to_string(),
            ));
        }
        if let Err(e) = mem_core::validate_namespace(&req.namespace) {
            return ToolCallResult::error(&e);
        }
        match self.store.get_agent(&req.agent_id).await {
            Ok(Some(target)) if target.owner_account_id == auth.account_id => {}
            _ => {
                return ToolCallResult::error(&HubError::InvalidArgument(
                    "目标 Agent 不存在或不属于本账户".to_string(),
                ))
            }
        }
        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        let is_owner = scoped
            .get_namespace(&req.namespace)
            .await
            .ok()
            .flatten()
            .map(|info| info.owner_agent == auth.agent_id)
            .unwrap_or(false);
        if !is_owner {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        match scoped.revoke(&req.namespace, &req.agent_id).await {
            Ok(()) => ToolCallResult::success(json!({
                "revoked": true,
                "namespace": req.namespace,
                "agent_id": req.agent_id,
            })),
            Err(e) => ToolCallResult::error(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mem_core::{DefaultBackupService, DefaultIntegrationService, GrantChecker};
    use store::InMemoryHubStore;
    use uuid::Uuid;

    fn harness() -> (ToolDispatcher, AuthCtx) {
        let store = Arc::new(InMemoryHubStore::new());
        let access = Arc::new(GrantChecker::new(store.clone()));
        let dispatcher = ToolDispatcher::new(
            Arc::new(mem_core::DefaultMemoryService::new(
                store.clone(),
                Arc::new(mem_core::TestEmbedder::default()),
                access.clone(),
                [0.5, 0.2, 0.1, 0.1, 0.1],
            )),
            Arc::new(DefaultBackupService::new(store.clone(), access.clone())),
            Arc::new(DefaultIntegrationService::new(
                store.clone(),
                access,
                0.99,
                100,
            )),
            store.clone(),
        );
        let auth = AuthCtx {
            agent_id: "agent-a".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        };
        (dispatcher, auth)
    }

    fn arg(v: Value) -> Value {
        v
    }

    #[tokio::test]
    async fn test_put_get_list_roundtrip() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "team-x",
                    "kind": "fact",
                    "content": "服务端口 3000",
                    "tags": ["ops"],
                })),
            )
            .await;
        assert!(!put.is_error, "put 应成功: {:?}", put);
        let id = put.structured_content.unwrap()["memory"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let get = d
            .dispatch(
                &auth,
                "memory_get",
                arg(json!({ "namespace": "team-x", "id": id })),
            )
            .await;
        assert!(!get.is_error);
        assert_eq!(
            get.structured_content.unwrap()["memory"]["content"],
            json!("服务端口 3000")
        );

        let list = d
            .dispatch(&auth, "memory_list", arg(json!({ "namespace": "team-x" })))
            .await;
        assert!(!list.is_error);
        assert_eq!(
            list.structured_content.unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_put_by_key_upserts() {
        let (d, auth) = harness();
        let r1 = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "ns", "key": "db:engine", "kind": "fact", "content": "MySQL",
                })),
            )
            .await;
        let id1 = r1.structured_content.unwrap()["memory"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let r2 = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "ns", "key": "db:engine", "kind": "fact", "content": "PostgreSQL",
                })),
            )
            .await;
        let payload = r2.structured_content.unwrap();
        assert_eq!(payload["memory"]["id"], json!(id1), "同 key 必须 upsert");
        assert_eq!(payload["memory"]["version"], json!(2));
    }

    #[tokio::test]
    async fn test_search_and_stats() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "用户喜欢美式咖啡" })),
        )
        .await;
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "preference", "content": "偏好深色模式" })),
        )
        .await;

        let search = d
            .dispatch(
                &auth,
                "memory_search",
                arg(json!({ "namespace": "ns", "query_text": "咖啡", "top_k": 5 })),
            )
            .await;
        assert!(!search.is_error);
        let results = search.structured_content.unwrap()["results"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!results.is_empty());
        // 关键词命中的条目必须排在首位（向量路可能补充语义近邻）
        assert!(results[0]["memory"]["content"]
            .as_str()
            .unwrap()
            .contains("咖啡"));

        let stats = d
            .dispatch(&auth, "memory_stats", arg(json!({ "namespace": "ns" })))
            .await;
        let stats = stats.structured_content.unwrap();
        assert_eq!(stats["total"], json!(2));
        assert_eq!(stats["by_kind"]["fact"], json!(1));
        assert_eq!(stats["by_kind"]["preference"], json!(1));
    }

    #[tokio::test]
    async fn test_update_and_delete_flow() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({ "namespace": "ns", "kind": "note", "content": "初版" })),
            )
            .await;
        let id = put.structured_content.unwrap()["memory"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let upd = d
            .dispatch(
                &auth,
                "memory_update",
                arg(json!({ "namespace": "ns", "id": id, "content": "修订版", "confidence": 0.9 })),
            )
            .await;
        assert!(!upd.is_error);
        assert_eq!(
            upd.structured_content.unwrap()["memory"]["version"],
            json!(2)
        );

        let del = d
            .dispatch(
                &auth,
                "memory_delete",
                arg(json!({ "namespace": "ns", "id": id })),
            )
            .await;
        assert!(!del.is_error);

        // 已归档条目再次删除应报错
        let del2 = d
            .dispatch(
                &auth,
                "memory_delete",
                arg(json!({ "namespace": "ns", "id": id })),
            )
            .await;
        assert!(del2.is_error);
    }

    #[tokio::test]
    async fn test_backup_and_restore_via_tools() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "关键配置 A" })),
        )
        .await;

        let backup = d
            .dispatch(
                &auth,
                "memory_backup_create",
                arg(json!({ "namespace": "ns", "note": "变更前" })),
            )
            .await;
        assert!(!backup.is_error);
        let backup_id = backup.structured_content.unwrap()["backup"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let list = d
            .dispatch(
                &auth,
                "memory_backup_list",
                arg(json!({ "namespace": "ns" })),
            )
            .await;
        assert_eq!(
            list.structured_content.unwrap()["backups"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let dry = d
            .dispatch(
                &auth,
                "memory_restore",
                arg(json!({ "backup_id": backup_id, "mode": "dry_run" })),
            )
            .await;
        assert!(!dry.is_error);
        assert_eq!(
            dry.structured_content.unwrap()["unchanged"],
            json!(1),
            "dry_run 应识别 1 条未变化"
        );
    }

    #[tokio::test]
    async fn test_integrate_scan_and_resolve_via_tools() {
        let (d, auth) = harness();
        // 两个不同 key 的相同内容 → 嵌入完全相同 → 重复提案
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "key": "k:1", "kind": "fact", "content": "服务部署在东京" })),
        )
        .await;
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "key": "k:2", "kind": "fact", "content": "服务部署在东京" })),
        )
        .await;

        let scan = d
            .dispatch(
                &auth,
                "memory_integrate_scan",
                arg(json!({ "namespace": "ns", "dry_run": false })),
            )
            .await;
        assert!(!scan.is_error, "扫描应成功: {:?}", scan);
        let proposals = scan.structured_content.unwrap()["proposals"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(proposals.len(), 1, "应识别出 1 个重复提案");
        let proposal_id = proposals[0]["id"].as_str().unwrap().to_string();
        let survivor = proposals[0]["member_ids"][0].as_str().unwrap().to_string();

        let resolve = d
            .dispatch(
                &auth,
                "memory_integration_resolve",
                arg(json!({
                    "proposal_id": proposal_id,
                    "action": "merge",
                    "survivor_id": survivor,
                })),
            )
            .await;
        assert!(!resolve.is_error, "合并应成功: {:?}", resolve);

        let list = d
            .dispatch(
                &auth,
                "memory_integration_list",
                arg(json!({ "namespace": "ns" })),
            )
            .await;
        assert!(!list.is_error);
        let after = list.structured_content.unwrap()["proposals"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["status"], json!("merged"));
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let (d, auth) = harness();
        let r = d.dispatch(&auth, "no_such_tool", json!({})).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn test_bad_arguments_return_error() {
        let (d, auth) = harness();
        let r = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({ "namespace": "ns", "kind": "not-a-kind", "content": "x" })),
            )
            .await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn test_embedding_stripped_from_output() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({ "namespace": "ns", "kind": "fact", "content": "有向量的条目" })),
            )
            .await;
        let payload = put.structured_content.unwrap();
        assert!(
            payload["memory"].get("embedding").is_none(),
            "输出不得包含 embedding 向量"
        );
    }

    #[tokio::test]
    async fn test_grant_and_revoke_via_tools() {
        let (d, auth) = harness();
        // 沙箱语义：授权双方都必须已注册且同属一个账户
        for id in ["agent-a", "agent-b"] {
            d.store
                .register_agent(&mem_core::AgentRegistration {
                    agent_id: id.to_string(),
                    name: id.to_string(),
                    kind: "assistant".to_string(),
                    owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
                    client_secret: None,
                    redirect_uris: vec![],
                    max_scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
                })
                .await
                .unwrap();
        }
        // 先建命名空间
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "shared", "kind": "note", "content": "共享笔记" })),
        )
        .await;

        let g = d
            .dispatch(
                &auth,
                "memory_grant",
                arg(json!({
                    "namespace": "shared",
                    "agent_id": "agent-b",
                    "can_read": true,
                    "can_write": true,
                })),
            )
            .await;
        assert!(!g.is_error, "owner 应可授权: {:?}", g);

        // agent-b 现在可以读写 shared
        let b = AuthCtx {
            agent_id: "agent-b".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write],
            acl: Default::default(),
        };
        let put_b = d
            .dispatch(
                &b,
                "memory_put",
                arg(json!({ "namespace": "shared", "kind": "note", "content": "B 的写入" })),
            )
            .await;
        assert!(!put_b.is_error, "被授权 Agent 应可写");

        // 撤销后不可写
        let rev = d
            .dispatch(
                &auth,
                "memory_revoke",
                arg(json!({ "namespace": "shared", "agent_id": "agent-b" })),
            )
            .await;
        assert!(!rev.is_error);
        let put_b2 = d
            .dispatch(
                &b,
                "memory_put",
                arg(json!({ "namespace": "shared", "kind": "note", "content": "再次写入" })),
            )
            .await;
        assert!(put_b2.is_error, "撤销后应不可写");
    }

    #[tokio::test]
    async fn test_grant_rejects_cross_account_and_wildcard() {
        let (d, auth) = harness();
        d.store
            .register_agent(&mem_core::AgentRegistration {
                agent_id: "agent-a".to_string(),
                name: "A".to_string(),
                kind: "assistant".to_string(),
                owner_account_id: common::BOOTSTRAP_ACCOUNT_ID,
                client_secret: None,
                redirect_uris: vec![],
                max_scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            })
            .await
            .unwrap();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "shared", "kind": "note", "content": "笔记" })),
        )
        .await;

        // 跨账户目标：agent-other 属于另一个账户 → 拒绝
        d.store
            .register_agent(&mem_core::AgentRegistration {
                agent_id: "agent-other".to_string(),
                name: "Other".to_string(),
                kind: "assistant".to_string(),
                owner_account_id: Uuid::from_u128(999),
                client_secret: None,
                redirect_uris: vec![],
                max_scopes: vec![Scope::Read],
            })
            .await
            .unwrap();
        let cross = d
            .dispatch(
                &auth,
                "memory_grant",
                arg(json!({
                    "namespace": "shared",
                    "agent_id": "agent-other",
                    "can_read": true,
                })),
            )
            .await;
        assert!(cross.is_error, "跨账户授权必须被拒绝: {:?}", cross);

        // 通配命名空间授权 → 拒绝（沙箱废除 `*`）
        let wildcard = d
            .dispatch(
                &auth,
                "memory_grant",
                arg(json!({
                    "namespace": "*",
                    "agent_id": "agent-a",
                    "can_read": true,
                })),
            )
            .await;
        assert!(wildcard.is_error, "通配授权必须被拒绝");
    }

    #[tokio::test]
    async fn test_grant_rejected_for_non_owner() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "owned-by-a", "kind": "note", "content": "A 的" })),
        )
        .await;

        // agent-c 虽然持有 admin scope，但不是 owner 也不能通配授权 → 拒绝
        let c = AuthCtx {
            agent_id: "agent-c".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        };
        let g = d
            .dispatch(
                &c,
                "memory_grant",
                arg(json!({
                    "namespace": "owned-by-a",
                    "agent_id": "agent-c",
                    "can_read": true,
                })),
            )
            .await;
        assert!(g.is_error, "非 owner 不得转授");
    }

    #[tokio::test]
    async fn test_history_tool_returns_versions() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({ "namespace": "ns", "kind": "note", "content": "v1" })),
            )
            .await;
        let id = put.structured_content.unwrap()["memory"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        d.dispatch(
            &auth,
            "memory_update",
            arg(json!({ "namespace": "ns", "id": id, "content": "v2" })),
        )
        .await;

        let h = d
            .dispatch(
                &auth,
                "memory_history",
                arg(json!({ "namespace": "ns", "id": id })),
            )
            .await;
        assert!(!h.is_error);
        let versions = h.structured_content.unwrap()["versions"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(versions.len(), 2);
    }

    // 编译期检查：内存实现满足聚合契约
    #[test]
    fn test_in_memory_store_satisfies_contract() {
        fn assert_impl<T: mem_core::MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }
}
