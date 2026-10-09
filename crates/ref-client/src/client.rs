//! 参照 MCP 客户端（基于设计规范 12 节与 18.2 WP-19）
//!
//! 验证：
//! - 问答前调用 `hub_context`（调用率 100% 验收）
//! - 步骤指令执行与自评回传 `hub_check`
//! - 意图假设与循环一致性回传 `hub_observe`

use common::{AuthCtx, Result};
use hub_mcp::ToolDispatcher;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// 客户端端到端轮次评测指标统计
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientMetrics {
    pub total_turns: usize,
    pub context_calls: usize,
    pub check_calls: usize,
    pub observe_calls: usize,
}

impl ClientMetrics {
    /// 调用率（必须达到 100%）
    pub fn calling_rate(&self) -> f32 {
        if self.total_turns == 0 {
            0.0
        } else {
            (self.context_calls as f32) / (self.total_turns as f32)
        }
    }
}

/// 参照 MCP 客户端
pub struct ReferenceMcpClient {
    dispatcher: ToolDispatcher,
}

impl ReferenceMcpClient {
    pub fn new(dispatcher: ToolDispatcher) -> Self {
        Self { dispatcher }
    }

    /// 模拟运行单轮对话（完全遵循设计规范第 4 节在线流程）
    pub async fn run_turn(
        &self,
        auth: &AuthCtx,
        turn_id: &str,
        scene_tags: &[String],
        metrics: &mut ClientMetrics,
    ) -> Result<String> {
        metrics.total_turns += 1;

        // 步骤 1：生成回答之前必须先调用 hub_context
        let ctx_res = self
            .dispatcher
            .dispatch(
                auth,
                "hub_context",
                json!({
                    "turn_id": turn_id,
                    "scene_tags": scene_tags,
                }),
            )
            .await;

        assert!(!ctx_res.is_error, "hub_context must succeed");
        metrics.context_calls += 1;

        // 步骤 2：模拟助理 LLM 执行轻量推理
        // 依次执行：推断假设 -> 按规范精修 -> 生成回答 -> 按量表自检
        let mock_answer = "收到，已按您的偏好以直接简洁的方式回复。".to_string();

        // 步骤 3：回传自评与规则校验 hub_check
        let chk_res = self
            .dispatcher
            .dispatch(
                auth,
                "hub_check",
                json!({
                    "turn_id": turn_id,
                    "self_eval": {
                        "gates": { "G1": true, "G2": true },
                        "scores": { "coherence": 0.95, "empathy": 0.85, "memory_fit": 0.90 },
                        "revisions": 0
                    },
                    "rule_hits": []
                }),
            )
            .await;

        assert!(!chk_res.is_error, "hub_check must succeed");
        metrics.check_calls += 1;

        // 步骤 4：回传心智假设与事件 hub_observe
        let obs_res = self
            .dispatcher
            .dispatch(
                auth,
                "hub_observe",
                json!({
                    "turn_id": turn_id,
                    "hypotheses": [{
                        "kind": "intention",
                        "content": "用户倾向于直接了当处理事务",
                        "confidence": 0.85,
                        "evidence": ["快速采纳"]
                    }],
                    "predicted_reaction": { "kind": "adopt", "confidence": 0.9 },
                    "events": [{ "type": "adopt" }]
                }),
            )
            .await;

        assert!(!obs_res.is_error, "hub_observe must succeed");
        metrics.observe_calls += 1;

        Ok(mock_answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{Acl, Scope};
    use hub_core::{
        Assembler, Layer, MemoryStore, NewMemory, PackSet, Provenance, Sensitivity, UsePolicy,
    };
    use hub_embed::MockEmbedder;
    use hub_packs::AtomicPackRegistry;
    use hub_retrieval::{HybridRetriever, HybridRetrieverConfig};
    use hub_rules::DefaultRuleEngine;
    use hub_store::InMemoryMemoryStore;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_reference_client_end_to_end_conformance() {
        // 1. 装配完整系统核心组件
        let store = Arc::new(InMemoryMemoryStore::new());
        let embedder = Arc::new(MockEmbedder::default());
        let retriever = Arc::new(HybridRetriever::new(
            store.clone(),
            embedder,
            HybridRetrieverConfig::default(),
        ));

        let initial_pack = PackSet {
            pack_version: "2026.10.3".to_string(),
            core_hash: "test_core_hash".to_string(),
            raw_content: json!({}),
        };
        let pack_registry =
            Arc::new(AtomicPackRegistry::new(initial_pack, "test_core_hash").unwrap());
        let rule_engine = Arc::new(DefaultRuleEngine::new());

        let assembler = Arc::new(Assembler::new(retriever, pack_registry, rule_engine, 300));

        let dispatcher = ToolDispatcher::new(assembler);
        let client = ReferenceMcpClient::new(dispatcher);

        let user_id = Uuid::new_v4();
        let auth = AuthCtx {
            user_id,
            client_id: "ref_assistant_v1".to_string(),
            scopes: vec![Scope::Context, Scope::Write],
            acl: Acl::default(),
        };

        // 预置一条用户记忆
        store
            .upsert(NewMemory {
                user_id,
                layer: Layer::Preference,
                text: "回复请保持精简".to_string(),
                provenance: Provenance::Stated,
                confidence: 1.0,
                sensitivity: Sensitivity::Normal,
                use_policy: UsePolicy::Free,
                source_client: "ref_assistant_v1".to_string(),
                ttl_seconds: None,
                attrs: json!({}),
            })
            .await
            .unwrap();

        // 2. 模拟 10 轮端到端会话
        let mut metrics = ClientMetrics::default();
        for i in 1..=10 {
            let turn_id = format!("turn_{}", i);
            let tags = vec!["daily_chat".to_string()];
            let answer = client
                .run_turn(&auth, &turn_id, &tags, &mut metrics)
                .await
                .unwrap();
            assert!(!answer.is_empty());
        }

        // 3. 验收指标
        assert_eq!(metrics.total_turns, 10);
        assert_eq!(metrics.context_calls, 10);
        assert_eq!(metrics.check_calls, 10);
        assert_eq!(metrics.observe_calls, 10);
        assert_eq!(
            metrics.calling_rate(),
            1.0,
            "Calling rate must be strictly 100%"
        );
    }
}
