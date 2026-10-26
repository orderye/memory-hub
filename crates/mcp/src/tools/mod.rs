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
}

mod admin;
mod backup;
mod cognitive;
mod coding;
mod integration;
mod memory;
mod working;

#[cfg(test)]
mod tests;
