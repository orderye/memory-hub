use super::*;

impl ToolDispatcher {
    pub(super) async fn integrate_scan(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn integration_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn integration_resolve(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn consolidate(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn consolidation_stats(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

}
