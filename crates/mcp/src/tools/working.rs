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
                // 若未指定关联任务，则自动生成一个**仍在进行中**的任务事实。
                // 不得自动标记为 completed：那会把“工具调用”伪造成“任务成功”，
                // 而完成事实是经验验证的必要证据（任务书 3.4 / 4.3）。
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
                            Ok(t) => t.id,
                            Err(e) => return ToolCallResult::error(&e),
                        }
                    }
                };
                let validation = match req.validation {
                    Some(ref v) if !v.trim().is_empty() => json!(v),
                    _ => {
                        return ToolCallResult::error(&HubError::InvalidArgument(
                            "validation 必填：经验必须给出可验证结果或验证方法，不得由服务端代填"
                                .into(),
                        ));
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
                    validation,
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
                // 旧的 `status` 直写通道会绕过状态机与验证门槛（任意状态随意改写），
                // 已关闭。状态流转必须走 `working_experience_transition`。
                if req.status.is_some() {
                    return ToolCallResult::error(&HubError::InvalidArgument(
                        "working_experience_feedback 不再接受 status 参数；状态流转请调用 \
                         working_experience_transition（verify / reject / deprecate / restore）"
                            .into(),
                    ));
                }
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
                    Ok(exp) => ToolCallResult::success(json!({ "experience": exp })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_feedback schema: {}",
                e
            ))),
        }
    }

    /// 经验资产状态流转（任务书 4.3）
    pub(super) async fn working_experience_transition(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            experience_id: uuid::Uuid,
            action: String,
            #[serde(default)]
            reason: Option<String>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let action = match mem_core::ExperienceTransition::parse(&req.action) {
                    Some(a) => a,
                    None => {
                        return ToolCallResult::error(&HubError::InvalidArgument(format!(
                            "不支持的状态流转动作: {}（可选 verify / reject / deprecate / restore）",
                            req.action
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
                    .transition(
                        auth,
                        &req.namespace,
                        req.experience_id,
                        action,
                        req.reason.as_deref(),
                        self.experience_verify_min_sources,
                    )
                    .await
                {
                    Ok(exp) => ToolCallResult::success(json!({ "experience": exp })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_transition schema: {}",
                e
            ))),
        }
    }

    /// 经验版本历史（任务书 4.6：版本变化不丢历史）
    pub(super) async fn working_experience_history(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            experience_id: uuid::Uuid,
            #[serde(default = "default_experience_history_limit")]
            limit: usize,
        }
        fn default_experience_history_limit() -> usize {
            20
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
                    .list_versions(
                        auth,
                        &req.namespace,
                        req.experience_id,
                        req.limit.clamp(1, 100),
                    )
                    .await
                {
                    Ok(versions) => ToolCallResult::success(json!({ "versions": versions })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 working_experience_history schema: {}",
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
            #[serde(default)]
            namespace: Option<String>,
            #[serde(default)]
            collection: Option<String>,
            query: String,
            #[serde(default)]
            env: Option<String>,
            /// 严格环境边界：环境不匹配的经验直接不返回（默认 false = 降权 + 告警）
            #[serde(default)]
            strict_env: bool,
            /// 显式限定状态集合（审计用）；缺省 = 排除 rejected/archived
            #[serde(default)]
            include_statuses: Option<Vec<String>>,
            #[serde(default = "default_experience_top_k")]
            top_k: usize,
        }
        fn default_experience_top_k() -> usize {
            10
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(req) => req,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 working_experience_search schema: {}",
                    e
                )));
            }
        };

        let mut statuses: Vec<mem_core::ExperienceStatus> = Vec::new();
        for raw in req.include_statuses.clone().unwrap_or_default() {
            match mem_core::ExperienceStatus::parse(&raw) {
                Some(s) => statuses.push(s),
                None => {
                    return ToolCallResult::error(&HubError::InvalidArgument(format!(
                        "不支持的经验状态: {}",
                        raw
                    )));
                }
            }
        }

        let (namespaces, collection_statuses) =
            match (req.namespace.clone(), req.collection.clone()) {
                (Some(ns), None) => (vec![ns], Vec::new()),
                (None, Some(name)) => match self.collections().resolve(auth, &name).await {
                    Ok(resolved) => (
                        resolved.resolved_namespaces,
                        resolved.collection.filter.experience_statuses,
                    ),
                    Err(e) => return ToolCallResult::error(&e),
                },
                _ => {
                    return ToolCallResult::error(&HubError::InvalidArgument(
                        "namespace 与 collection 必须且只能提供一个".into(),
                    ));
                }
            };

        if statuses.is_empty() && !collection_statuses.is_empty() {
            statuses = collection_statuses;
        }
        let include = if statuses.is_empty() {
            None
        } else {
            Some(statuses.as_slice())
        };

        let service = self
            .working_memory
            .clone()
            .unwrap_or_else(|| Arc::new(mem_core::WorkingMemoryService::new(self.store.clone())))
            .scoped(auth.account_id);

        let mut skipped: Vec<String> = Vec::new();
        let mut all: Vec<(f32, Value)> = Vec::new();
        // 单命名空间请求必须 fail closed；只有集合扇出才降级为 skipped
        let fan_out = req.collection.is_some();
        for ns in &namespaces {
            match service
                .search_experiences_governed(
                    auth,
                    ns,
                    &req.query,
                    mem_core::ExperienceSearchOptions {
                        current_env: req.env.as_deref(),
                        limit: req.top_k.clamp(1, 50),
                        strict_env: req.strict_env,
                        include_statuses: include,
                    },
                )
                .await
            {
                Ok(results) => {
                    for r in results {
                        all.push((
                            r.score,
                            json!({
                                "namespace": ns,
                                "score": r.score,
                                "env_mismatch": r.env_mismatch,
                                "boundary_warning": r.boundary_warning,
                                "experience": r.experience,
                            }),
                        ));
                    }
                }
                Err(HubError::InsufficientScope) if fan_out => skipped.push(ns.clone()),
                Err(e) => return ToolCallResult::error(&e),
            }
        }

        all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        all.truncate(req.top_k.clamp(1, 50));
        ToolCallResult::success(json!({
            "results": all.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
            "scope": {
                "namespaces": namespaces,
                "skipped_namespaces": skipped,
                "collection": req.collection,
            }
        }))
    }
}
