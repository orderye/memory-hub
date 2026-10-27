use super::*;

impl ToolDispatcher {
    pub(super) async fn put(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn get(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn update(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn delete(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn search(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            #[serde(default)]
            namespace: Option<String>,
            #[serde(default)]
            collection: Option<String>,
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

        let req = match serde_json::from_value::<Req>(args) {
            Ok(req) => req,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_search schema: {}",
                    e
                )));
            }
        };

        // 解析检索范围：namespace 与 collection 必须且只能提供一个
        let (namespaces, filter) = match (req.namespace.clone(), req.collection.clone()) {
            (Some(ns), None) => (vec![ns], None),
            (None, Some(name)) => match self.collections().resolve(auth, &name).await {
                Ok(resolved) => (
                    resolved.resolved_namespaces,
                    Some(resolved.collection.filter),
                ),
                Err(e) => return ToolCallResult::error(&e),
            },
            _ => {
                return ToolCallResult::error(&HubError::InvalidArgument(
                    "namespace 与 collection 必须且只能提供一个".into(),
                ));
            }
        };

        let mut skipped: Vec<String> = Vec::new();
        let mut merged: Vec<(f32, mem_core::MemoryEntry)> = Vec::new();
        let mut experiences: Vec<Value> = Vec::new();
        // 只有集合扇出才把无权限命名空间降级为 skipped；
        // 显式单命名空间请求必须 fail closed（返回错误），否则撤销授权后会静默变成空结果。
        let fan_out = req.collection.is_some();

        for ns in &namespaces {
            let q = mem_core::SearchQuery {
                namespace: ns.clone(),
                query_text: req.query_text.clone(),
                kinds: merge_kind_filter(req.kinds.as_deref(), filter.as_ref()),
                tags: merge_tag_filter(req.tags.as_deref(), filter.as_ref()),
                top_k: req.top_k.clamp(1, 50),
                include_archived: req.include_archived
                    || filter
                        .as_ref()
                        .map(|f| f.statuses.contains(&mem_core::MemoryStatus::Archived))
                        .unwrap_or(false),
                min_score: req.min_score,
                scene: match (req.scene, filter.as_ref().and_then(|f| f.scene)) {
                    (Some(a), Some(b)) if a != b => {
                        // 集合与请求声明了不同场景：交集为空，该命名空间不产出结果
                        continue;
                    }
                    (Some(a), _) => Some(a),
                    (None, b) => b,
                },
                layer: match (req.layer, filter.as_ref().and_then(|f| f.layer)) {
                    (Some(a), Some(b)) if a != b => continue,
                    (Some(a), _) => Some(a),
                    (None, b) => b,
                },
                valid_at: req.valid_at,
                include_superseded: req.include_superseded,
            };
            match self.memory.search(auth, q).await {
                Ok(results) => {
                    for s in results {
                        if let Some(f) = &filter {
                            if !f.statuses.is_empty() && !f.statuses.contains(&s.item.status) {
                                continue;
                            }
                        }
                        merged.push((s.score, s.item));
                    }
                }
                Err(HubError::InsufficientScope) if fan_out => skipped.push(ns.clone()),
                Err(e) => return ToolCallResult::error(&e),
            }

            // 集合显式包含经验时，同一范围内附带经验检索（分数体系不同，单独返回）
            if let Some(f) = &filter {
                if f.include_experiences {
                    let service = self
                        .working_memory
                        .clone()
                        .unwrap_or_else(|| {
                            Arc::new(mem_core::WorkingMemoryService::new(self.store.clone()))
                        })
                        .scoped(auth.account_id);
                    match service
                        .search_experiences_governed(
                            auth,
                            ns,
                            &req.query_text,
                            mem_core::ExperienceSearchOptions {
                                current_env: None,
                                limit: req.top_k.clamp(1, 50),
                                strict_env: false,
                                include_statuses: if f.experience_statuses.is_empty() {
                                    None
                                } else {
                                    Some(f.experience_statuses.as_slice())
                                },
                            },
                        )
                        .await
                    {
                        Ok(rows) => experiences.extend(rows.into_iter().map(|r| {
                            json!({
                                "namespace": ns,
                                "score": r.score,
                                "env_mismatch": r.env_mismatch,
                                "boundary_warning": r.boundary_warning,
                                "experience": r.experience,
                            })
                        })),
                        Err(HubError::InsufficientScope) => {
                            if !skipped.contains(ns) {
                                skipped.push(ns.clone());
                            }
                        }
                        Err(e) => return ToolCallResult::error(&e),
                    }
                }
            }
        }

        merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(req.top_k.clamp(1, 50));
        let items: Vec<Value> = merged
            .into_iter()
            .map(|(score, item)| {
                json!({
                    "score": score,
                    "memory": strip_embedding(
                        serde_json::to_value(&item).unwrap_or(Value::Null)
                    ),
                })
            })
            .collect();

        // 空结果必须**显式**表达"无可靠匹配"，而不是让调用方
        // 把"空数组"误读成"库里没有这条记忆"或"检索坏了"。
        let no_reliable_match = items.is_empty();
        ToolCallResult::success(json!({
            "results": items,
            "experiences": experiences,
            "scope": {
                "namespaces": namespaces,
                "skipped_namespaces": skipped,
                "collection": req.collection,
            },
            // 能力声明（追加字段，不改动既有键）
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

    pub(super) async fn list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn stats(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn history(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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
}

/// 集合过滤器与请求 kinds 取交集（两者都是“硬边界”，请求只能进一步收窄）
fn merge_kind_filter(
    requested: Option<&[mem_core::MemoryKind]>,
    filter: Option<&mem_core::CollectionFilter>,
) -> Option<Vec<mem_core::MemoryKind>> {
    match (requested, filter) {
        (None, None) => None,
        (Some(r), None) => Some(r.to_vec()),
        (None, Some(f)) => {
            if f.kinds.is_empty() {
                None
            } else {
                Some(f.kinds.clone())
            }
        }
        (Some(r), Some(f)) => {
            if f.kinds.is_empty() {
                Some(r.to_vec())
            } else {
                Some(
                    r.iter()
                        .filter(|k| f.kinds.contains(*k))
                        .copied()
                        .collect::<Vec<_>>(),
                )
            }
        }
    }
}

/// 集合标签与请求标签合并：两者都是“必须包含”，取并集去重
fn merge_tag_filter(
    requested: Option<&[String]>,
    filter: Option<&mem_core::CollectionFilter>,
) -> Option<Vec<String>> {
    let mut tags: Vec<String> = requested.map(|t| t.to_vec()).unwrap_or_default();
    if let Some(f) = filter {
        for tag in &f.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    if tags.is_empty() {
        None
    } else {
        Some(tags)
    }
}
