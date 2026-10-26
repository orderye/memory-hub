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
