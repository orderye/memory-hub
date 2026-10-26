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

    pub(super) async fn coding_revision_record(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
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

