use super::*;

impl ToolDispatcher {
    pub(super) async fn cognitive_chat_propose(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn cognitive_proposal_resolve(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn cognitive_intentions_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

}
