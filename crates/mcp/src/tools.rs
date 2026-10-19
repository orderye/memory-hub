//! MCP 工具执行器与双模输出生成
//!
//! 18 个工具全部围绕三件事：**存储**（put/get/update/delete/search/list/stats/history）、
//! **备份**（backup_create/backup_list/restore）、**整合**（integrate_scan/
//! integration_list/integration_resolve），外加命名空间共享（grant/revoke）。
//!
//! 输出同时携带 `structuredContent`（结构化）与文本通道（兼容 JSON 字符串），
//! 错误一律以 `isError = true` + 标准错误码对象返回（MCP 规定工具错误仍在 200 内表达）。

use common::{AgentId, AuthCtx, HubError, Scope};
use mem_core::infra::ExperienceStore;
use mem_core::{
    BackupService, GrantStore, IntegrationService, MemoryService, NamespaceGrantRecord,
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
}

impl Default for ConsolidationParams {
    fn default() -> Self {
        Self {
            sim_threshold: 0.92,
            max_proposals: 100,
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
        }
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

        let mut engine = consolidation::ConsolidationEngine::new(
            Arc::new(scoped),
            req.sim_threshold
                .unwrap_or(self.consolidation.sim_threshold),
            self.consolidation.max_proposals,
        );
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

        let engine = consolidation::ConsolidationEngine::new(
            Arc::new(scoped),
            self.consolidation.sim_threshold,
            self.consolidation.max_proposals,
        );

        match engine.calculate_stats(&req.namespace).await {
            Ok(stats) => {
                ToolCallResult::success(serde_json::to_value(&stats).unwrap_or(Value::Null))
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
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let action = match req.decision.to_lowercase().as_str() {
                    "rejected" => mem_core::ResolveAction::Dismiss,
                    "accepted" | "modified" => mem_core::ResolveAction::Merge,
                    other => {
                        return ToolCallResult::error(&HubError::InvalidArgument(format!(
                            "不支持的提案决策: {}",
                            other
                        )));
                    }
                };
                match self
                    .integration
                    .resolve(
                        auth,
                        mem_core::ResolveRequest {
                            proposal_id: req.proposal_id,
                            action,
                            survivor_id: None,
                            note: req.reason.clone(),
                        },
                    )
                    .await
                {
                    Ok(mut entry) => {
                        if req.decision == "modified" {
                            if let Some(custom) = req.custom_content {
                                let patch = mem_core::MemoryPatch {
                                    content: Some(custom),
                                    kind: None,
                                    tags: None,
                                    metadata: None,
                                    confidence: None,
                                    expires_in_seconds: None,
                                    scene: None,
                                    layer: None,
                                    valid_from: None,
                                    valid_until: None,
                                    occurred_at: None,
                                };
                                match self
                                    .memory
                                    .update(auth, &entry.namespace, entry.id, patch)
                                    .await
                                {
                                    Ok(updated) => {
                                        entry = updated;
                                    }
                                    Err(e) => return ToolCallResult::error(&e),
                                }
                            }
                        }
                        ToolCallResult::success(json!({
                            "status": req.decision,
                            "resulting_memory": strip_embedding(serde_json::to_value(&entry).unwrap_or(Value::Null)),
                        }))
                    }
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 cognitive_proposal_resolve schema: {}",
                e
            ))),
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

                let proposals = match self.integration.list(auth, &req.namespace).await {
                    Ok(list) => {
                        let filtered: Vec<_> = list
                            .into_iter()
                            .filter(|p| {
                                if let Some(ref st) = req.status {
                                    match st.to_lowercase().as_str() {
                                        "open" => p.status == mem_core::ProposalStatus::Open,
                                        "merged" => p.status == mem_core::ProposalStatus::Merged,
                                        "dismissed" => {
                                            p.status == mem_core::ProposalStatus::Dismissed
                                        }
                                        _ => true,
                                    }
                                } else {
                                    true
                                }
                            })
                            .take(limit)
                            .collect();
                        filtered
                    }
                    Err(_) => vec![],
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
        let store = Arc::new(InMemoryHubStore::new());
        let access = Arc::new(GrantChecker::new(store.clone()));
        let embedder = Arc::new(mem_core::TestEmbedder::default());
        let retriever = Arc::new(retrieval::HybridRetriever::new(
            store.clone() as Arc<dyn mem_core::MemoryStore>,
            embedder.clone(),
            100,
            100,
            [0.5, 0.2, 0.1, 0.1, 0.1],
            0.7,
            500,
        ));
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
                    "diff_content": "+const KEY: &str = \"sk-proj-1234567890abcdef1234567890abcdef\";",
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

    // 编译期检查：内存实现满足聚合契约
    #[test]
    fn test_in_memory_store_satisfies_contract() {
        fn assert_impl<T: mem_core::MemoryHubStore>() {}
        assert_impl::<InMemoryHubStore>();
    }
}
