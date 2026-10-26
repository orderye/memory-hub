//! MCP 工具执行器与双模输出生成
//!
//! 28 个工具全部围绕三件事：**存储**（put/get/update/delete/search/list/stats/history）、
//! **备份**（backup_create/backup_list/restore）、**整合**（integrate_scan/
//! integration_list/integration_resolve），外加命名空间共享（grant/revoke）。
//!
//! 输出同时携带 `structuredContent`（结构化）与文本通道（兼容 JSON 字符串），
//! 错误一律以 `isError = true` + 标准错误码对象返回（MCP 规定工具错误仍在 200 内表达）。

use common::{AgentId, AuthCtx, HubError, Scope};
use mem_core::infra::ExperienceStore;
use mem_core::{
    BackupService, CognitiveProposalStore, GrantStore, IntegrationService, IntegrationStore,
    MemoryService, NamespaceGrantRecord,
};
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

/// 整合中枢的运行参数。
///
/// 此前这两个值在工具分发里写死为 `0.92 / 100`，绕过了 `AppConfig` 的
/// `INTEGRATE_SIM_THRESHOLD` / `INTEGRATE_MAX_PROPOSALS`，改了环境变量也不生效。
/// 默认值沿用历史行为，由二进制启动时按配置覆盖。
#[derive(Debug, Clone, Copy)]
pub struct ConsolidationParams {
    pub sim_threshold: f32,
    pub max_proposals: usize,
    /// 候选发现模式（默认纯本地规则整合）
    pub mode: common::ConsolidationMode,
    /// 向量模式不可用时的降级策略
    pub fallback: common::ConsolidationFallback,
    /// 是否允许 MCP 单次请求覆盖服务端模式
    pub allow_mode_override: bool,
    /// 向量模式单次扫描最多新计算的向量条目数
    pub embed_top_k: usize,
    /// 向量模式语义候选的相似度下限；`None` 表示未经真实模型标定
    pub embed_min_similarity: Option<f32>,
    /// 纯规则模式的词法候选相似度下限
    pub lexical_min_similarity: f32,
    /// 单命名空间单次扫描的活跃条目上限（0 = 不限制）
    pub scan_max_entries: usize,
    /// 整合健康度统计的缓存时长（秒，0 = 每次重算）
    ///
    /// `memory_consolidation_stats` 每次都要做一遍全命名空间候选分析；
    /// Agent 连环调用或面板轮询会把它变成最贵的热点。缓存并显式回报 `cached`。
    pub stats_ttl_s: u64,
}

impl Default for ConsolidationParams {
    fn default() -> Self {
        Self {
            sim_threshold: 0.92,
            max_proposals: 100,
            mode: common::ConsolidationMode::Rules,
            fallback: common::ConsolidationFallback::Rules,
            allow_mode_override: false,
            embed_top_k: 20,
            embed_min_similarity: None,
            lexical_min_similarity: 0.80,
            scan_max_entries: 2_000,
            stats_ttl_s: 60,
        }
    }
}

/// MCP 工具调用调度器
#[derive(Clone)]
pub struct ToolDispatcher {
    pub memory: Arc<dyn MemoryService>,
    pub backup: Arc<dyn BackupService>,
    pub integration: Arc<dyn IntegrationService>,
    /// 底层存储（授权工具用；所有访问经账户作用域包装器）
    pub store: Arc<dyn mem_core::MemoryHubStore>,
    /// 整合中枢参数（来自配置，而非硬编码）
    pub consolidation: ConsolidationParams,
    /// 嵌入器：整合合并后重算幸存者向量（未注入时合并会清空旧向量）
    pub embedder: Option<Arc<dyn mem_core::Embedder>>,
    /// 认知提炼服务（Chat 场景会话提炼与事实演化）
    pub chat_cognitive: Option<Arc<mem_core::ChatCognitiveService>>,
    /// Working 经验记忆服务（任务事实生命周期与经验提炼）
    pub working_memory: Option<Arc<mem_core::WorkingMemoryService>>,
    /// Coding 代码演进记忆服务（主题线程与修订记录）
    pub coding_memory: Option<Arc<mem_core::CodingMemoryService>>,
    /// 整合健康度统计缓存：`(account, namespace) → (写入时刻, 响应)`
    ///
    /// 放在 `Arc<Mutex<..>>` 里是因为 `ToolDispatcher` 会被克隆分发；
    /// 缓存只影响"多久重算一次"，不改变任何数据语义。
    stats_cache:
        Arc<std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Value)>>>,
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
            consolidation: ConsolidationParams::default(),
            embedder: None,
            chat_cognitive: None,
            working_memory: None,
            coding_memory: None,
            stats_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// 本次实例是否启用向量路（嵌入被显式关闭时为 false）
    fn vector_path_enabled(&self) -> bool {
        self.embedder
            .as_ref()
            .map(|e| e.is_enabled())
            .unwrap_or(false)
    }

    /// 本次实例是否具备真实语义能力（伪向量/关闭均为 false）
    fn semantic_ready(&self) -> bool {
        self.embedder
            .as_ref()
            .map(|e| e.provides_semantic_signal())
            .unwrap_or(false)
    }

    /// 以配置注入整合参数
    pub fn with_consolidation(mut self, params: ConsolidationParams) -> Self {
        self.consolidation = params;
        self
    }

    /// 注入嵌入器（合并后重算向量）
    pub fn with_embedder(mut self, embedder: Arc<dyn mem_core::Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// 注入 Chat 认知提炼服务
    pub fn with_chat_cognitive(mut self, service: Arc<mem_core::ChatCognitiveService>) -> Self {
        self.chat_cognitive = Some(service);
        self
    }

    /// 注入 Working 经验记忆服务
    pub fn with_working_memory(mut self, service: Arc<mem_core::WorkingMemoryService>) -> Self {
        self.working_memory = Some(service);
        self
    }

    /// 注入 Coding 代码演进记忆服务
    pub fn with_coding_memory(mut self, service: Arc<mem_core::CodingMemoryService>) -> Self {
        self.coding_memory = Some(service);
        self
    }

    /// 解析请求指定的整合模式
    ///
    /// `HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE=false`（默认）时，调用方**不得**逐次
    /// 切换整合模式：显式请求与服务端默认不一致 → 直接拒绝；一致 → 接受（等价于不覆盖）。
    ///
    /// 这道闸门放在 MCP 层：显式选择 `hybrid` 意味着允许把经脱敏的记忆文本发送到
    /// 已配置的嵌入端点，属于管理员级别的决定，不该由单个请求决定。
    fn resolve_requested_mode(
        &self,
        requested: Option<common::ConsolidationMode>,
    ) -> Result<common::ConsolidationMode, HubError> {
        let Some(mode) = requested else {
            return Ok(self.consolidation.mode);
        };
        if !self.consolidation.allow_mode_override {
            if mode != self.consolidation.mode {
                return Err(HubError::InvalidArgument(format!(
                    "服务端未开启 HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE，整合模式固定为 {}",
                    self.consolidation.mode.as_str()
                )));
            }
            return Ok(self.consolidation.mode);
        }
        Ok(mode)
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
            "memory_consolidate" => self.consolidate(auth, arguments).await,
            "memory_consolidation_stats" => self.consolidation_stats(auth, arguments).await,
            "memory_grant" => self.grant(auth, arguments).await,
            "memory_revoke" => self.revoke(auth, arguments).await,
            // ===== 认知增强工具 (P2 / P3 / P4) =====
            "cognitive_chat_propose" => self.cognitive_chat_propose(auth, arguments).await,
            "cognitive_proposal_resolve" => self.cognitive_proposal_resolve(auth, arguments).await,
            "cognitive_intentions_list" => self.cognitive_intentions_list(auth, arguments).await,
            "working_task_start" => self.working_task_start(auth, arguments).await,
            "working_task_finish" => self.working_task_finish(auth, arguments).await,
            "working_experience_distill" => self.working_experience_distill(auth, arguments).await,
            "working_experience_feedback" => {
                self.working_experience_feedback(auth, arguments).await
            }
            "working_experience_search" => self.working_experience_search(auth, arguments).await,
            "coding_thread_create" => self.coding_thread_create(auth, arguments).await,
            "coding_revision_record" => self.coding_revision_record(auth, arguments).await,
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
            #[serde(default)]
            scene: Option<mem_core::MemoryScene>,
            #[serde(default)]
            layer: Option<mem_core::CognitiveLayer>,
            #[serde(default)]
            valid_from: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(default)]
            valid_until: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(default)]
            occurred_at: Option<chrono::DateTime<chrono::Utc>>,
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
                    scene: req.scene,
                    layer: req.layer,
                    valid_from: req.valid_from,
                    valid_until: req.valid_until,
                    occurred_at: req.occurred_at,
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
            #[serde(default)]
            min_score: Option<f32>,
            #[serde(default)]
            scene: Option<mem_core::domain::MemoryScene>,
            #[serde(default)]
            layer: Option<mem_core::domain::CognitiveLayer>,
            #[serde(default)]
            valid_at: Option<chrono::DateTime<chrono::Utc>>,
            #[serde(default)]
            include_superseded: bool,
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
                    min_score: req.min_score,
                    scene: req.scene,
                    layer: req.layer,
                    valid_at: req.valid_at,
                    include_superseded: req.include_superseded,
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
                        // 空结果必须**显式**表达"无可靠匹配"，而不是让调用方
                        // 把"空数组"误读成"库里没有这条记忆"或"检索坏了"。
                        // 长期记忆里，一个域外查询硬凑一条低分结果，
                        // 比明确说"没有"更容易把错误事实写进 Agent 的上下文。
                        let no_reliable_match = items.is_empty();
                        ToolCallResult::success(json!({
                            "results": items,
                            // 能力声明（追加字段，不改动既有键）：调用方必须能区分
                            // "检索为纯关键词模式" 与 "向量服务临时故障降级"
                            "capabilities": {
                                "semantic_ready": self.semantic_ready(),
                                "vector_path_enabled": self.vector_path_enabled(),
                                "keyword_only": !self.vector_path_enabled(),
                            },
                            "no_reliable_match": no_reliable_match,
                            "reason": if no_reliable_match {
                                "没有达到最小可信分的候选：本次检索未找到可靠匹配，\
                                 请勿据此编造事实（可放宽 min_score 或调整查询后重试）"
                            } else {
                                ""
                            },
                        }))
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
        match serde_json::from_value::<mem_core::ScanRequest>(args.clone()) {
            Ok(mut req) => {
                // 模式覆盖闸门在 MCP 层：未开启覆盖时，调用方不得逐次切换整合模式
                match self.resolve_requested_mode(req.mode) {
                    Ok(mode) => req.mode = Some(mode),
                    Err(e) => return ToolCallResult::error(&e),
                }
                match self.integration.scan(auth, req).await {
                    Ok(report) => ToolCallResult::success(
                        serde_json::to_value(&report).unwrap_or(Value::Null),
                    ),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
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

    async fn consolidate(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            #[serde(default)]
            dry_run: bool,
            #[serde(default)]
            sim_threshold: Option<f32>,
            #[serde(default)]
            auto_merge: bool,
            #[serde(default)]
            mode: Option<common::ConsolidationMode>,
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_consolidate schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Write) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        if !scoped
            .can_write(&req.namespace, &auth.agent_id)
            .await
            .unwrap_or(false)
        {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        // 模式覆盖闸门：未开启覆盖时，调用方不得逐次切换整合模式
        let mode = match self.resolve_requested_mode(req.mode) {
            Ok(m) => m,
            Err(e) => return ToolCallResult::error(&e),
        };

        let mut engine = consolidation::ConsolidationEngine::new(
            Arc::new(scoped),
            req.sim_threshold
                .unwrap_or(self.consolidation.sim_threshold),
            self.consolidation.max_proposals,
        )
        .with_mode(mode, self.consolidation.fallback)
        .with_candidate_limits(consolidation::CandidateLimits {
            max_embed_entries: self.consolidation.embed_top_k,
            ..consolidation::CandidateLimits::default()
        })
        .with_embed_min_similarity(self.consolidation.embed_min_similarity)
        .with_lexical_min_similarity(self.consolidation.lexical_min_similarity)
        // 有界扫描：大命名空间不得一次全量装载（超限时报告标 truncated）
        .with_scan_limit(self.consolidation.scan_max_entries);
        if let Some(embedder) = &self.embedder {
            engine = engine.with_embedder(embedder.clone());
        }
        if req.auto_merge {
            engine = engine.with_policy(consolidation::AutoPolicy::new(
                consolidation::PolicyMode::SemiAuto,
            ));
        }

        match engine
            .consolidate_namespace(
                &req.namespace,
                req.dry_run,
                req.sim_threshold,
                &auth.agent_id,
            )
            .await
        {
            Ok(report) => {
                ToolCallResult::success(serde_json::to_value(&report).unwrap_or(Value::Null))
            }
            Err(e) => ToolCallResult::error(&e),
        }
    }

    async fn consolidation_stats(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_consolidation_stats schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Read) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        if !scoped
            .can_read(&req.namespace, &auth.agent_id)
            .await
            .unwrap_or(false)
        {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        // 统计缓存：`memory_consolidation_stats` 每次都要做一遍全库候选分析，
        // Agent 连环调用 / 面板轮询会把它变成最贵的热点。缓存并**显式**回报命中状态，
        // 不让调用方把"60 秒前的抽样"误当成实时结论。
        let cache_key = format!("{}:{}", auth.account_id, req.namespace);
        let now = std::time::Instant::now();
        if self.consolidation.stats_ttl_s > 0 {
            if let Ok(guard) = self.stats_cache.lock() {
                if let Some((at, value)) = guard.get(&cache_key) {
                    let age = now.saturating_duration_since(*at);
                    if age.as_secs() < self.consolidation.stats_ttl_s {
                        let mut out = value.clone();
                        if let Some(obj) = out.as_object_mut() {
                            obj.insert("cached".to_string(), json!(true));
                            obj.insert("cache_age_s".to_string(), json!(age.as_secs()));
                        }
                        return ToolCallResult::success(out);
                    }
                }
            }
        }

        let engine = consolidation::ConsolidationEngine::new(
            Arc::new(scoped),
            self.consolidation.sim_threshold,
            self.consolidation.max_proposals,
        )
        .with_scan_limit(self.consolidation.scan_max_entries);

        match engine.calculate_stats(&req.namespace).await {
            Ok(stats) => {
                let mut value = serde_json::to_value(&stats).unwrap_or(Value::Null);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("cached".to_string(), json!(false));
                    obj.insert("cache_age_s".to_string(), json!(0));
                }
                if self.consolidation.stats_ttl_s > 0 {
                    if let Ok(mut guard) = self.stats_cache.lock() {
                        guard.insert(cache_key, (now, value.clone()));
                        // 防无界增长：条目过多时只保留仍在有效期内的
                        if guard.len() > 512 {
                            guard.retain(|_, (at, _)| {
                                now.saturating_duration_since(*at).as_secs()
                                    < self.consolidation.stats_ttl_s
                            });
                        }
                    }
                }
                ToolCallResult::success(value)
            }
            Err(e) => ToolCallResult::error(&e),
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

    // ===== 认知增强：Chat / Working / Coding =====

    async fn cognitive_chat_propose(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        if !auth.has_scope(&Scope::Write) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        #[derive(Deserialize)]
        struct Turn {
            role: String,
            content: String,
        }
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            turns: Vec<Turn>,
            #[serde(default)]
            #[allow(dead_code)]
            scene: Option<String>,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            metadata: Option<Value>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = match &self.chat_cognitive {
                    Some(s) => s.clone(),
                    None => {
                        return ToolCallResult::error(&HubError::MethodNotFound(
                            "Chat 认知提炼服务未配置模型抽取器".into(),
                        ));
                    }
                };
                let messages = req
                    .turns
                    .into_iter()
                    .map(|t| mem_core::ChatMessage {
                        role: match t.role.to_lowercase().as_str() {
                            "user" => mem_core::ChatRole::User,
                            "assistant" => mem_core::ChatRole::Assistant,
                            "system" => mem_core::ChatRole::System,
                            _ => mem_core::ChatRole::User,
                        },
                        content: t.content,
                        timestamp: Some(chrono::Utc::now()),
                    })
                    .collect();
                let session_id = req
                    .session_id
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let input = mem_core::ChatCognitiveInput {
                    namespace: req.namespace,
                    session_id,
                    messages,
                    occurred_at: Some(chrono::Utc::now()),
                };
                let scoped_svc = service.scoped(auth.account_id);
                match scoped_svc.process_chat_session(auth, input).await {
                    Ok(result) => {
                        let committed: Vec<_> = result
                            .committed_entries
                            .into_iter()
                            .map(|e| {
                                strip_embedding(serde_json::to_value(&e).unwrap_or(Value::Null))
                            })
                            .collect();
                        ToolCallResult::success(json!({
                            "evidence_id": result.evidence_id,
                            "committed_entries": committed,
                            "proposals": result.proposals,
                        }))
                    }
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 cognitive_chat_propose schema: {}",
                e
            ))),
        }
    }

    async fn cognitive_proposal_resolve(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        if !auth.has_scope(&Scope::Write) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        #[derive(Deserialize)]
        struct Req {
            proposal_id: uuid::Uuid,
            decision: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            custom_content: Option<String>,
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 cognitive_proposal_resolve schema: {}",
                    e
                )))
            }
        };
        let decision = match req.decision.to_lowercase().as_str() {
            "rejected" | "accepted" | "modified" => req.decision.to_lowercase(),
            other => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "不支持的提案决策: {}",
                    other
                )));
            }
        };

        // 路由分发：先查认知提案存储（本工具的语义归属），再回退整合提案。
        // 两套提案是不同的 ID 空间——认知提案来自 cognitive_chat_propose，
        // 整合提案来自 memory_integrate_scan。此前不查认知存储直接路由
        // 整合 resolve，把认知提案 ID 传进来必然 NotFound。
        let scoped = mem_core::AccountScopedStore::<dyn mem_core::MemoryHubStore>::new(
            self.store.clone(),
            auth.account_id,
        );
        let cognitive_hit = match scoped.get_cognitive_proposal(req.proposal_id).await {
            Ok(p) => Some(p),
            Err(HubError::NotFound) => None,
            Err(e) => return ToolCallResult::error(&e),
        };

        // ID 混用防护：本工具名为 cognitive_proposal_resolve，只裁决认知提案。
        // 整合提案 ID 传进来时报明确错误，而不是静默走整合路径或笼统 NotFound。
        let is_integration_id = match cognitive_hit {
            None => scoped.get_proposal(req.proposal_id).await.is_ok(),
            Some(_) => false,
        };
        if cognitive_hit.is_none() {
            if is_integration_id {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "提案 {} 是整合提案（memory_integrate_scan 产出），请使用 memory_integration_resolve 裁决",
                    req.proposal_id
                )));
            }
            return ToolCallResult::error(&HubError::NotFound);
        }

        let chat_svc = match &self.chat_cognitive {
            Some(s) => s.clone(),
            None => {
                return ToolCallResult::error(&HubError::MethodNotFound(
                    "Chat 认知提炼服务未配置模型抽取器".into(),
                ));
            }
        };
        let scoped_svc = chat_svc.scoped(auth.account_id);

        let review_decision = match decision.as_str() {
            "rejected" => mem_core::CognitiveReviewDecision::Reject {
                reason: req.reason.clone(),
            },
            // 采纳与编辑后合并同走 Accept；modified 必须提供修订正文
            "accepted" => mem_core::CognitiveReviewDecision::Accept {
                modified_content: None,
            },
            "modified" => {
                let Some(custom) = req.custom_content.clone() else {
                    return ToolCallResult::error(&HubError::InvalidArgument(
                        "decision 为 modified 时必须提供 custom_content".into(),
                    ));
                };
                mem_core::CognitiveReviewDecision::Accept {
                    modified_content: Some(custom),
                }
            }
            _ => unreachable!("decision 已在上方枚举校验"),
        };

        match scoped_svc
            .review_persisted_proposal(auth, req.proposal_id, review_decision)
            .await
        {
            Ok(entry) => {
                let resulting =
                    entry.map(|e| strip_embedding(serde_json::to_value(&e).unwrap_or(Value::Null)));
                ToolCallResult::success(json!({
                    "status": decision,
                    "proposal_id": req.proposal_id,
                    "resulting_memory": resulting,
                }))
            }
            Err(e) => ToolCallResult::error(&e),
        }
    }

    async fn cognitive_intentions_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        if !auth.has_scope(&Scope::Read) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            #[serde(default)]
            status: Option<String>,
            #[serde(default)]
            limit: Option<usize>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let limit = req.limit.unwrap_or(20).clamp(1, 100);
                let intentions = if let Some(ref chat_svc) = self.chat_cognitive {
                    let scoped = chat_svc.scoped(auth.account_id);
                    scoped
                        .list_active_intentions(auth, &req.namespace)
                        .await
                        .unwrap_or_default()
                } else {
                    let list_req = mem_core::ListRequest {
                        namespace: req.namespace.clone(),
                        kinds: None,
                        statuses: Some(vec![mem_core::MemoryStatus::Active]),
                        tags: None,
                        source_agent: None,
                        text_contains: None,
                        scene: Some(mem_core::MemoryScene::Chat),
                        layer: Some(mem_core::CognitiveLayer::Intention),
                        valid_at: Some(chrono::Utc::now()),
                        cursor: None,
                        limit,
                    };
                    match self.memory.list(auth, list_req).await {
                        Ok(page) => page.items,
                        Err(e) => return ToolCallResult::error(&e),
                    }
                };
                let intentions_val: Vec<_> = intentions
                    .into_iter()
                    .take(limit)
                    .map(|e| strip_embedding(serde_json::to_value(&e).unwrap_or(Value::Null)))
                    .collect();

                // 候选提案列认知提案（cognitive_chat_propose 的产出），
                // 而不是整合提案——后者属于 memory_integration_list 的职责。
                // chat_cognitive 服务未配置时直接读存储，保持工具可用。
                let status_filter = req.status.as_deref().and_then(|st| match st {
                    "pending_review" => Some(mem_core::CognitiveProposalStatus::PendingReview),
                    "accepted" => Some(mem_core::CognitiveProposalStatus::Accepted),
                    "rejected" => Some(mem_core::CognitiveProposalStatus::Rejected),
                    "superseded" => Some(mem_core::CognitiveProposalStatus::Superseded),
                    "candidate" => Some(mem_core::CognitiveProposalStatus::Candidate),
                    _ => None,
                });
                let proposals_result: std::result::Result<
                    Vec<mem_core::CognitiveProposalRecord>,
                    HubError,
                > = if let Some(ref chat_svc) = self.chat_cognitive {
                    chat_svc
                        .scoped(auth.account_id)
                        .list_proposals(auth, &req.namespace, status_filter)
                        .await
                } else {
                    let scoped =
                        mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
                    mem_core::infra::CognitiveProposalStore::list_cognitive_proposals(
                        &scoped,
                        &req.namespace,
                        status_filter,
                    )
                    .await
                };
                let proposals: Vec<_> = match proposals_result {
                    Ok(list) => list,
                    Err(e) => return ToolCallResult::error(&e),
                };

                ToolCallResult::success(json!({
                    "intentions": intentions_val,
                    "proposals": proposals,
                }))
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 cognitive_intentions_list schema: {}",
                e
            ))),
        }
    }

    async fn working_task_start(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            task: String,
            #[serde(default)]
            scene: Option<String>,
            #[serde(default)]
            source_evidence_id: Option<uuid::Uuid>,
            #[serde(default)]
            metadata: Option<Value>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self
                    .working_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                let scene = match req.scene.as_deref() {
                    Some("chat") => Some(mem_core::MemoryScene::Chat),
                    Some("coding") => Some(mem_core::MemoryScene::Coding),
                    _ => Some(mem_core::MemoryScene::Working),
                };
                match service
                    .start_task_with_evidence(
                        auth,
                        &req.namespace,
                        &req.task,
                        scene,
                        req.source_evidence_id,
                        req.metadata,
                    )
                    .await
                {
                    Ok(task) => ToolCallResult::success(json!({ "task": task })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_task_start schema: {}",
                e
            ))),
        }
    }

    async fn working_task_finish(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            task_id: uuid::Uuid,
            status: String,
            #[serde(default)]
            result_summary: Option<String>,
            #[serde(default)]
            failure_reason: Option<String>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let status = match req.status.to_lowercase().as_str() {
                    "completed" => mem_core::TaskStatus::Completed,
                    "failed" => mem_core::TaskStatus::Failed,
                    "cancelled" => mem_core::TaskStatus::Cancelled,
                    other => {
                        return ToolCallResult::error(&HubError::InvalidArgument(format!(
                            "不支持的任务终态: {}",
                            other
                        )));
                    }
                };
                let service = self
                    .working_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                match service
                    .finish_task(
                        auth,
                        req.task_id,
                        &req.namespace,
                        status,
                        req.result_summary,
                        req.failure_reason,
                        None,
                    )
                    .await
                {
                    Ok(task) => ToolCallResult::success(json!({ "task": task })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_task_finish schema: {}",
                e
            ))),
        }
    }

    async fn working_experience_distill(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            task: String,
            solution: String,
            #[serde(default)]
            task_id: Option<uuid::Uuid>,
            #[serde(default)]
            search_keys: Option<Vec<String>>,
            #[serde(default)]
            preconditions: Option<Vec<String>>,
            #[serde(default)]
            boundary_envs: Option<Vec<String>>,
            #[serde(default)]
            failure_modes: Option<Vec<String>>,
            #[serde(default)]
            validation: Option<String>,
            #[serde(default)]
            confidence: Option<f32>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self
                    .working_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                // 若未指定关联任务，则自动生成一个已完成的工作事实任务
                let source_task_id = match req.task_id {
                    Some(tid) => tid,
                    None => {
                        match service
                            .start_task(
                                auth,
                                &req.namespace,
                                &req.task,
                                Some(mem_core::MemoryScene::Working),
                                None,
                            )
                            .await
                        {
                            Ok(t) => {
                                let _ = service
                                    .finish_task(
                                        auth,
                                        t.id,
                                        &req.namespace,
                                        mem_core::TaskStatus::Completed,
                                        Some("由工作经验提炼工具自动创建的关联任务事实".into()),
                                        None,
                                        None,
                                    )
                                    .await;
                                t.id
                            }
                            Err(e) => return ToolCallResult::error(&e),
                        }
                    }
                };
                let candidate = mem_core::ExperienceCandidate {
                    source_task_id,
                    task_pattern: req.task,
                    search_keys: req.search_keys.unwrap_or_default(),
                    solution: req.solution,
                    preconditions: serde_json::to_value(req.preconditions.unwrap_or_default())
                        .unwrap_or_else(|_| json!([])),
                    boundary_envs: req.boundary_envs.unwrap_or_default(),
                    failure_modes: req.failure_modes.unwrap_or_default(),
                    validation: match req.validation {
                        Some(ref v) if !v.trim().is_empty() => json!(v),
                        _ => json!("通过自动化测试与回归验证"),
                    },
                    confidence: req.confidence.unwrap_or(1.0),
                };
                match service
                    .distill_experience(auth, &req.namespace, candidate)
                    .await
                {
                    Ok(exp) => ToolCallResult::success(json!({ "experience": exp })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_distill schema: {}",
                e
            ))),
        }
    }

    async fn working_experience_feedback(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            experience_id: uuid::Uuid,
            score: f32,
            #[serde(default)]
            status: Option<String>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self
                    .working_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                let is_success = req.score > 0.0;
                match service
                    .record_feedback(auth, &req.namespace, req.experience_id, is_success, None)
                    .await
                {
                    Ok(mut exp) => {
                        if let Some(st) = req.status {
                            let parsed = match st.to_lowercase().as_str() {
                                "candidate" => mem_core::ExperienceStatus::Candidate,
                                "active" => mem_core::ExperienceStatus::Active,
                                "deprecated" => mem_core::ExperienceStatus::Deprecated,
                                "archived" => mem_core::ExperienceStatus::Archived,
                                other => {
                                    return ToolCallResult::error(&HubError::InvalidArgument(
                                        format!("不支持的经验状态: {}", other),
                                    ));
                                }
                            };
                            exp.status = parsed;
                            let scoped_store = mem_core::AccountScopedStore::new(
                                self.store.clone(),
                                auth.account_id,
                            );
                            if let Err(e) = scoped_store.update_experience(&exp).await {
                                return ToolCallResult::error(&e);
                            }
                        }
                        ToolCallResult::success(json!({ "experience": exp }))
                    }
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_feedback schema: {}",
                e
            ))),
        }
    }

    async fn working_experience_search(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            query: String,
            #[serde(default)]
            env: Option<String>,
            /// 严格环境边界：环境不匹配的经验直接不返回（默认 false = 降权 + 告警）
            #[serde(default)]
            strict_env: bool,
            #[serde(default = "default_experience_top_k")]
            top_k: usize,
        }
        fn default_experience_top_k() -> usize {
            10
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self
                    .working_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                match service
                    .search_experiences_with(
                        auth,
                        &req.namespace,
                        &req.query,
                        req.env.as_deref(),
                        req.top_k.clamp(1, 50),
                        req.strict_env,
                    )
                    .await
                {
                    Ok(results) => {
                        let items: Vec<Value> = results
                            .into_iter()
                            .map(|r| {
                                json!({
                                    "score": r.score,
                                    "env_mismatch": r.env_mismatch,
                                    "boundary_warning": r.boundary_warning,
                                    "experience": r.experience,
                                })
                            })
                            .collect();
                        ToolCallResult::success(json!({ "results": items }))
                    }
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_search schema: {}",
                e
            ))),
        }
    }

    async fn coding_thread_create(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            repository: String,
            #[serde(default)]
            branch: Option<String>,
            title: String,
            #[serde(default)]
            description: Option<String>,
            #[serde(default)]
            task_id: Option<uuid::Uuid>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self
                    .coding_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::CodingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                let thread = mem_core::NewCodingThread {
                    id: None,
                    namespace: req.namespace,
                    repository: req.repository,
                    branch: req.branch,
                    title: req.title,
                    description: req.description,
                    task_id: req.task_id,
                };
                match service.start_thread(auth, thread).await {
                    Ok(t) => ToolCallResult::success(json!({ "thread": t })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 coding_thread_create schema: {}",
                e
            ))),
        }
    }

    async fn coding_revision_record(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            repository: String,
            file_path: String,
            diff_content: String,
            change_kind: String,
            #[serde(default)]
            thread_id: Option<uuid::Uuid>,
            #[serde(default)]
            branch: Option<String>,
            #[serde(default)]
            commit_sha: Option<String>,
            #[serde(default)]
            parent_sha: Option<String>,
            #[serde(default)]
            symbol_fingerprint: Option<String>,
            #[serde(default)]
            is_committed: Option<bool>,
            #[serde(default)]
            author: Option<String>,
            #[serde(default)]
            source_evidence_id: Option<uuid::Uuid>,
            #[serde(default)]
            source_task_id: Option<uuid::Uuid>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let kind = match req.change_kind.to_lowercase().as_str() {
                    "add" => mem_core::CodeChangeKind::Add,
                    "modify" => mem_core::CodeChangeKind::Modify,
                    "delete" => mem_core::CodeChangeKind::Delete,
                    "rename" => mem_core::CodeChangeKind::Rename,
                    other => {
                        return ToolCallResult::error(&HubError::InvalidArgument(format!(
                            "不支持的代码变更类型: {}",
                            other
                        )));
                    }
                };
                let service = self
                    .coding_memory
                    .clone()
                    .unwrap_or_else(|| {
                        Arc::new(mem_core::CodingMemoryService::new(self.store.clone()))
                    })
                    .scoped(auth.account_id);
                let revision = mem_core::NewCodingRevision {
                    id: None,
                    namespace: req.namespace,
                    repository: req.repository,
                    thread_id: req.thread_id,
                    branch: req.branch,
                    commit_sha: req.commit_sha,
                    parent_sha: req.parent_sha,
                    change_kind: kind,
                    file_path: req.file_path,
                    diff_content: req.diff_content,
                    symbol_fingerprint: req.symbol_fingerprint,
                    is_committed: req.is_committed.unwrap_or(false),
                    author: req.author,
                    occurred_at: Some(chrono::Utc::now()),
                    source_evidence_id: req.source_evidence_id,
                    source_task_id: req.source_task_id,
                };
                match service.record_revision(auth, revision).await {
                    Ok(rev) => ToolCallResult::success(json!({ "revision": rev })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 coding_revision_record schema: {}",
                e
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use consolidation::DefaultIntegrationService;
    use mem_core::{DefaultBackupService, GrantChecker};
    use store::InMemoryHubStore;
    use uuid::Uuid;

    fn harness() -> (ToolDispatcher, AuthCtx) {
        harness_with_min_score(None)
    }

    /// 构造带默认拒绝阈值的分发器（与生产默认一致）
    fn harness_with_min_score(min_score: Option<f32>) -> (ToolDispatcher, AuthCtx) {
        let store = Arc::new(InMemoryHubStore::new());
        let access = Arc::new(GrantChecker::new(store.clone()));
        let embedder = Arc::new(mem_core::TestEmbedder::default());
        let retriever = Arc::new(
            retrieval::HybridRetriever::new(
                store.clone() as Arc<dyn mem_core::MemoryStore>,
                embedder.clone(),
                100,
                100,
                [0.5, 0.2, 0.1, 0.1, 0.1],
                0.7,
                500,
            )
            .with_default_min_score(min_score),
        );
        let dispatcher = ToolDispatcher::new(
            Arc::new(
                mem_core::DefaultMemoryService::new(store.clone(), embedder, access.clone())
                    .with_retriever(retriever),
            ),
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

    /// 检索无结果时必须**显式**表达"无可靠匹配"
    ///
    /// 回归：此前只返回空数组，调用方会把"没找到可靠的"误读成
    /// "库里没有这条记忆"或"检索坏了"，进而自行编造事实。
    #[tokio::test]
    async fn test_search_reports_no_reliable_match() {
        // 生产默认：调用方不传 min_score 时由服务端阈值把关
        let (d, auth) = harness_with_min_score(Some(0.65));
        // 先写一条注册命名空间（命名空间不存在时检索是 fail-closed 的）
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "team-x",
                    "kind": "fact",
                    "content": "服务端口 3000",
                })),
            )
            .await;
        assert!(!put.is_error);

        // 查询一个完全无关的主题
        let res = d
            .dispatch(
                &auth,
                "memory_search",
                arg(json!({
                    "namespace": "team-x",
                    "query_text": "完全不相关的主题：火星基地氧气循环",
                })),
            )
            .await;
        assert!(!res.is_error, "检索不应报错：{:?}", res.content);
        let body = res.structured_content.unwrap();
        assert_eq!(body["results"], json!([]));
        assert_eq!(
            body["no_reliable_match"],
            json!(true),
            "空结果必须显式声明无可靠匹配：{body}"
        );
        assert!(
            body["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("可靠匹配"),
            "应给出可操作的说明：{body}"
        );
    }

    /// 检索有结果时不得声明"无可靠匹配"
    #[tokio::test]
    async fn test_search_with_hit_does_not_claim_no_reliable_match() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "team-x",
                    "kind": "fact",
                    "content": "服务端口 3000",
                })),
            )
            .await;
        assert!(!put.is_error);

        let res = d
            .dispatch(
                &auth,
                "memory_search",
                arg(json!({ "namespace": "team-x", "query_text": "服务端口" })),
            )
            .await;
        assert!(!res.is_error);
        let body = res.structured_content.unwrap();
        assert_eq!(body["no_reliable_match"], json!(false));
        assert!(!body["results"].as_array().unwrap().is_empty());
    }

    /// 检索响应必须自报能力状态：没有向量能力时 keyword_only=true。
    /// 调用方据此区分"这就是纯关键词模式"与"向量服务临时故障降级"。
    #[tokio::test]
    async fn test_search_reports_capability_state() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "team-x",
                    "kind": "fact",
                    "content": "服务端口 3000",
                })),
            )
            .await;
        assert!(!put.is_error);

        let res = d
            .dispatch(
                &auth,
                "memory_search",
                arg(json!({ "namespace": "team-x", "query_text": "服务端口" })),
            )
            .await;
        assert!(!res.is_error);
        let body = res.structured_content.unwrap();
        assert_eq!(
            body["capabilities"]["vector_path_enabled"],
            json!(false),
            "未注入嵌入器时向量路必须声明为关闭：{body}"
        );
        assert_eq!(body["capabilities"]["keyword_only"], json!(true));
        assert_eq!(body["capabilities"]["semantic_ready"], json!(false));
    }

    /// 整合统计缓存：第二次调用必须命中缓存并如实标注 cached=true
    #[tokio::test]
    async fn test_consolidation_stats_are_cached_and_marked() {
        let (d, auth) = harness();
        let put = d
            .dispatch(
                &auth,
                "memory_put",
                arg(json!({
                    "namespace": "team-x",
                    "kind": "fact",
                    "content": "服务端口 3000",
                })),
            )
            .await;
        assert!(!put.is_error);

        let first = d
            .dispatch(
                &auth,
                "memory_consolidation_stats",
                arg(json!({ "namespace": "team-x" })),
            )
            .await;
        assert!(!first.is_error);
        let first_body = first.structured_content.unwrap();
        assert_eq!(first_body["cached"], json!(false), "首次必须实际计算");

        let second = d
            .dispatch(
                &auth,
                "memory_consolidation_stats",
                arg(json!({ "namespace": "team-x" })),
            )
            .await;
        let second_body = second.structured_content.unwrap();
        assert_eq!(second_body["cached"], json!(true), "TTL 内必须命中缓存");
        assert_eq!(
            second_body["total_active"], first_body["total_active"],
            "缓存命中不得改变统计内容"
        );
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
    async fn test_consolidation_hub_tools() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "team-hub", "kind": "fact", "content": "集群配置：节点规格 8C16G" })),
        )
        .await;
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "team-hub", "kind": "fact", "content": "集群配置：节点规格 8C16G" })),
        )
        .await;
        let stats = d
            .dispatch(
                &auth,
                "memory_consolidation_stats",
                arg(json!({ "namespace": "team-hub" })),
            )
            .await;
        assert!(!stats.is_error, "统计应成功: {:?}", stats);
        let sc = stats.structured_content.unwrap();
        assert_eq!(sc["total_active"], 2);

        let consolidate = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "team-hub", "dry_run": false, "auto_merge": true })),
            )
            .await;
        assert!(!consolidate.is_error, "整合中枢应成功: {:?}", consolidate);
        let report = consolidate.structured_content.unwrap();
        assert_eq!(report["auto_merged"], 1);
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

        // 撤销后读与搜索同样必须被拒绝
        let get_b = d
            .dispatch(
                &b,
                "memory_get",
                arg(json!({ "namespace": "shared", "key": "any" })),
            )
            .await;
        assert!(get_b.is_error, "撤销后应不可读");

        let search_b = d
            .dispatch(
                &b,
                "memory_search",
                arg(json!({ "namespace": "shared", "query_text": "笔记" })),
            )
            .await;
        assert!(search_b.is_error, "撤销后应不可搜");
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

    #[tokio::test]
    async fn test_working_memory_tools_lifecycle_and_scope() {
        let (d, auth) = harness();

        // 1. 无写权限时 working_task_start 应失败
        let read_only_auth = AuthCtx {
            agent_id: "agent-ro".to_string(),
            account_id: common::BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read],
            acl: Default::default(),
        };
        let denied = d
            .dispatch(
                &read_only_auth,
                "working_task_start",
                arg(json!({
                    "namespace": "work-ns",
                    "task": "排查内存泄漏",
                })),
            )
            .await;
        assert!(denied.is_error, "缺少 write scope 应拒绝");

        // 2. 正常启动任务
        let start_res = d
            .dispatch(
                &auth,
                "working_task_start",
                arg(json!({
                    "namespace": "work-ns",
                    "task": "排查内存泄漏",
                })),
            )
            .await;
        assert!(!start_res.is_error, "任务启动失败: {:?}", start_res);
        let task_id = start_res.structured_content.unwrap()["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 3. 正常结束任务
        let finish_res = d
            .dispatch(
                &auth,
                "working_task_finish",
                arg(json!({
                    "namespace": "work-ns",
                    "task_id": task_id,
                    "status": "completed",
                    "result_summary": "定位并修复循环引用",
                })),
            )
            .await;
        assert!(!finish_res.is_error, "任务结束失败: {:?}", finish_res);

        // 4. 经验提炼
        let distill_res = d
            .dispatch(
                &auth,
                "working_experience_distill",
                arg(json!({
                    "namespace": "work-ns",
                    "task": "排查内存泄漏",
                    "solution": "使用 valgrind 或 heaptrack 抓取堆栈分析泄漏点",
                    "task_id": task_id,
                    "search_keys": ["memory_leak", "heaptrack"],
                    "preconditions": ["linux环境"],
                    "boundary_envs": ["os:linux"],
                    "failure_modes": ["未安装调试符号"],
                    "confidence": 0.95,
                })),
            )
            .await;
        assert!(!distill_res.is_error, "经验提炼失败: {:?}", distill_res);
        let exp_id = distill_res.structured_content.unwrap()["experience"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 5. 经验效用反馈
        let fb_res = d
            .dispatch(
                &auth,
                "working_experience_feedback",
                arg(json!({
                    "namespace": "work-ns",
                    "experience_id": exp_id,
                    "score": 1.0,
                    "status": "active",
                })),
            )
            .await;
        assert!(!fb_res.is_error, "反馈失败: {:?}", fb_res);

        // 6. 经验检索：分词部分匹配 + 环境边界匹配
        //    （查询不是正文的整段子串，旧实现会整条漏召回）
        let search_res = d
            .dispatch(
                &auth,
                "working_experience_search",
                arg(json!({
                    "namespace": "work-ns",
                    "query": "内存泄漏排查",
                    "env": "os:linux",
                    "top_k": 5,
                })),
            )
            .await;
        assert!(!search_res.is_error, "经验检索失败: {:?}", search_res);
        let results = search_res.structured_content.unwrap();
        let results = results["results"].as_array().unwrap();
        assert!(
            results.iter().any(|r| r["experience"]["id"] == exp_id),
            "经验检索必须能召回刚提炼的经验：{results:?}"
        );
        assert_eq!(results[0]["env_mismatch"], json!(false));

        // 7. 环境不匹配：显式告警且降权保留
        let mismatch_res = d
            .dispatch(
                &auth,
                "working_experience_search",
                arg(json!({
                    "namespace": "work-ns",
                    "query": "内存泄漏排查",
                    "env": "os:windows",
                })),
            )
            .await;
        let mismatched = mismatch_res.structured_content.unwrap();
        let item = mismatched["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["experience"]["id"] == exp_id)
            .expect("环境不匹配时仍应保留经验");
        assert_eq!(item["env_mismatch"], json!(true));
        assert!(item["boundary_warning"]
            .as_str()
            .unwrap_or_default()
            .contains("os:windows"));
    }

    #[tokio::test]
    async fn test_coding_memory_tools_and_security_checks() {
        let (d, auth) = harness();

        // 1. 创建 coding thread
        let thread_res = d
            .dispatch(
                &auth,
                "coding_thread_create",
                arg(json!({
                    "namespace": "code-ns",
                    "repository": "company/backend",
                    "branch": "feature/cognitive-v2",
                    "title": "重构 MCP 认知调度器",
                })),
            )
            .await;
        assert!(!thread_res.is_error, "线程创建失败: {:?}", thread_res);
        let thread_id = thread_res.structured_content.unwrap()["thread"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        // 2. 尝试记录含路径穿越与敏感凭证的代码变更 -> 必须被拒绝
        let bad_path = d
            .dispatch(
                &auth,
                "coding_revision_record",
                arg(json!({
                    "namespace": "code-ns",
                    "repository": "company/backend",
                    "thread_id": thread_id,
                    "file_path": "../../../etc/passwd",
                    "diff_content": "+root:x:0:0:root:/root:/bin/bash",
                    "change_kind": "modify",
                })),
            )
            .await;
        assert!(bad_path.is_error, "路径穿越必须被拒绝");

        let leak = d
            .dispatch(
                &auth,
                "coding_revision_record",
                arg(json!({
                    "namespace": "code-ns",
                    "repository": "company/backend",
                    "thread_id": thread_id,
                    "file_path": "config/key.rs",
                    "diff_content": format!(
                        "+const KEY: &str = \"{}\";",
                        concat!("sk-proj-", "1234567890abcdef1234567890abcdef")
                    ),
                    "change_kind": "add",
                })),
            )
            .await;
        assert!(leak.is_error, "敏感凭证泄漏必须被拒绝");

        // 3. 正常记录代码变更
        let ok_rev = d
            .dispatch(
                &auth,
                "coding_revision_record",
                arg(json!({
                    "namespace": "code-ns",
                    "repository": "company/backend",
                    "thread_id": thread_id,
                    "file_path": "src/tools.rs",
                    "diff_content": "--- a/src/tools.rs\n+++ b/src/tools.rs\n@@ -1,1 +1,2 @@\n+// cognitive tool\n",
                    "change_kind": "modify",
                })),
            )
            .await;
        assert!(!ok_rev.is_error, "正常代码修订应成功: {:?}", ok_rev);
    }

    #[tokio::test]
    async fn test_chat_cognitive_tools_propose_resolve_and_intentions() {
        let (mut d, auth) = harness();
        let mock_extractor = Arc::new(
            mem_core::testing::MockChatCognitiveExtractor::new().with_candidates(vec![
                mem_core::CognitiveCandidate {
                    content: "用户偏好使用 Rust 编写后端服务".into(),
                    layer: mem_core::CognitiveLayer::Intention,
                    scene: mem_core::MemoryScene::Chat,
                    confidence: 0.95,
                    occurred_at: Some(chrono::Utc::now()),
                    valid_from: None,
                    valid_until: Some(chrono::Utc::now() + chrono::Duration::days(30)),
                    tags: vec!["preference".into(), "rust".into()],
                    target_memory_id: None,
                    relation_kind: None,
                    metadata: serde_json::json!({}),
                },
            ]),
        );
        let chat_svc = Arc::new(mem_core::ChatCognitiveService::new(
            d.store.clone(),
            mock_extractor,
            mem_core::CognitivePolicy {
                mode: mem_core::CognitiveGovernanceMode::Manual,
                ..Default::default()
            },
        ));
        d = d.with_chat_cognitive(chat_svc);

        // 1. 提交对话提炼
        let prop_res = d
            .dispatch(
                &auth,
                "cognitive_chat_propose",
                arg(json!({
                    "namespace": "chat-ns",
                    "turns": [
                        { "role": "user", "content": "以后项目后端全部使用 Rust" },
                        { "role": "assistant", "content": "好的，已记录偏好" }
                    ],
                })),
            )
            .await;
        assert!(!prop_res.is_error, "对话提炼应成功: {:?}", prop_res);
        let proposals = prop_res.structured_content.unwrap()["proposals"]
            .as_array()
            .unwrap()
            .clone();
        assert!(!proposals.is_empty(), "在 Manual 模式下应生成候选提案");

        // 2. 列出意图与提案
        let list_res = d
            .dispatch(
                &auth,
                "cognitive_intentions_list",
                arg(json!({
                    "namespace": "chat-ns",
                })),
            )
            .await;
        assert!(!list_res.is_error, "列出意图应成功: {:?}", list_res);
    }

    // ===== 整合模式：MCP 兼容性与覆盖闸门 =====

    /// 旧客户端（不传 `mode`）必须照常工作
    #[tokio::test]
    async fn test_consolidate_without_mode_field_keeps_working() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "旧客户端也能整合" })),
        )
        .await;
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "旧客户端也能整合" })),
        )
        .await;

        let res = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "ns", "dry_run": true })),
            )
            .await;
        assert!(!res.is_error, "不带 mode 的旧请求必须可用: {:?}", res);
        let sc = res.structured_content.unwrap();
        // 响应新增模式状态段，既有字段保持不变
        assert_eq!(sc["mode"]["effective_mode"], "rules");
        assert_eq!(sc["mode"]["requested_mode"], "rules");
        assert_eq!(sc["mode"]["embedding_used"], false);
        assert_eq!(sc["scanned"], 2);
    }

    /// 默认禁止逐请求覆盖模式：显式请求 hybrid 必须被拒绝
    #[tokio::test]
    async fn test_consolidate_rejects_mode_override_when_disabled() {
        let (d, auth) = harness();
        // 先建命名空间（首次写入即所有者）
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
        )
        .await;
        let res = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "ns", "dry_run": true, "mode": "hybrid" })),
            )
            .await;
        assert!(
            res.is_error,
            "未开启覆盖时必须拒绝逐请求切换模式: {:?}",
            res
        );
        let text = res
            .structured_content
            .as_ref()
            .and_then(|v| v["message_zh"].as_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            text.contains("HUB_CONSOLIDATION_ALLOW_MODE_OVERRIDE"),
            "错误信息须说明如何开启：{text}"
        );
    }

    /// 显式请求与服务端默认一致时允许（等价于不覆盖）
    #[tokio::test]
    async fn test_consolidate_accepts_mode_equal_to_server_default() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
        )
        .await;
        let res = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "ns", "dry_run": true, "mode": "rules" })),
            )
            .await;
        assert!(
            !res.is_error,
            "与服务端默认一致的显式模式应被接受: {:?}",
            res
        );
    }

    /// 开启覆盖后，显式模式被采纳
    #[tokio::test]
    async fn test_consolidate_accepts_mode_override_when_enabled() {
        let (d, auth) = harness();
        let d = d.with_consolidation(ConsolidationParams {
            allow_mode_override: true,
            ..ConsolidationParams::default()
        });
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
        )
        .await;
        let res = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "ns", "dry_run": true, "mode": "hybrid" })),
            )
            .await;
        assert!(!res.is_error, "开启覆盖后应接受显式模式: {:?}", res);
        let sc = res.structured_content.unwrap();
        assert_eq!(sc["mode"]["requested_mode"], "hybrid");
    }

    /// 未知模式字符串必须被拒绝，不得静默猜测
    #[tokio::test]
    async fn test_consolidate_rejects_unknown_mode_value() {
        let (d, auth) = harness();
        d.dispatch(
            &auth,
            "memory_put",
            arg(json!({ "namespace": "ns", "kind": "fact", "content": "用于建命名空间" })),
        )
        .await;
        let res = d
            .dispatch(
                &auth,
                "memory_consolidate",
                arg(json!({ "namespace": "ns", "dry_run": true, "mode": "semantic" })),
            )
            .await;
        assert!(res.is_error, "未知模式必须被拒绝: {:?}", res);
    }

    // 编译期检查：内存实现满足聚合契约
    #[test]
    fn test_in_memory_store_satisfies_contract() {
        fn assert_impl<T: mem_core::MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }
}
