use super::*;

impl ToolDispatcher {
    pub(super) async fn working_task_start(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn working_task_finish(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn working_experience_distill(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
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

    pub(super) async fn working_experience_feedback(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
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

    pub(super) async fn working_experience_search(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
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
}
