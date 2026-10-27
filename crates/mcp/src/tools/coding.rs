use super::*;

impl ToolDispatcher {
    pub(super) async fn coding_thread_create(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

    pub(super) async fn coding_revision_record(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
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

    /// 列出代码演进线程（只读）
    pub(super) async fn coding_thread_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            #[serde(default)]
            repository: Option<String>,
            #[serde(default = "default_coding_limit")]
            limit: usize,
        }
        fn default_coding_limit() -> usize {
            50
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self.coding_service(auth);
                match service
                    .list_threads(
                        auth,
                        &req.namespace,
                        req.repository.as_deref(),
                        req.limit.clamp(1, 100),
                    )
                    .await
                {
                    Ok(threads) => ToolCallResult::success(json!({ "threads": threads })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 coding_thread_list schema: {}",
                e
            ))),
        }
    }

    /// 列出代码修订（按线程或按文件；只读）
    pub(super) async fn coding_revision_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            #[serde(default)]
            thread_id: Option<uuid::Uuid>,
            #[serde(default)]
            repository: Option<String>,
            #[serde(default)]
            file_path: Option<String>,
            #[serde(default = "default_coding_limit")]
            limit: usize,
        }
        fn default_coding_limit() -> usize {
            50
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self.coding_service(auth);
                let limit = req.limit.clamp(1, 100);
                let result = if let Some(thread_id) = req.thread_id {
                    service
                        .list_thread_revisions(auth, &req.namespace, thread_id, limit)
                        .await
                } else if let (Some(repo), Some(file)) = (&req.repository, &req.file_path) {
                    service
                        .list_file_revisions(auth, &req.namespace, repo, file, limit)
                        .await
                } else {
                    return ToolCallResult::error(&HubError::InvalidArgument(
                        "必须提供 thread_id，或同时提供 repository 与 file_path".into(),
                    ));
                };
                match result {
                    Ok(revisions) => ToolCallResult::success(json!({ "revisions": revisions })),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 coding_revision_list schema: {}",
                e
            ))),
        }
    }

    /// 读取单条代码修订（只读）
    pub(super) async fn coding_revision_get(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            revision_id: uuid::Uuid,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => {
                let service = self.coding_service(auth);
                match service
                    .get_revision(auth, &req.namespace, req.revision_id)
                    .await
                {
                    Ok(Some(revision)) => ToolCallResult::success(json!({ "revision": revision })),
                    Ok(None) => ToolCallResult::error(&HubError::NotFound),
                    Err(e) => ToolCallResult::error(&e),
                }
            }
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 coding_revision_get schema: {}",
                e
            ))),
        }
    }

    fn coding_service(&self, auth: &AuthCtx) -> mem_core::CodingMemoryService {
        self.coding_memory
            .clone()
            .unwrap_or_else(|| Arc::new(mem_core::CodingMemoryService::new(self.store.clone())))
            .scoped(auth.account_id)
    }
}
