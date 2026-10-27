use super::*;

impl ToolDispatcher {
    // ===== 备份 =====

    pub(super) async fn backup_create(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::BackupCreateRequest>(args) {
            Ok(req) => match self.backup.create(auth, req).await {
                Ok(record) => {
                    let summary = serde_json::to_value(mem_core::BackupSummary::from(&record))
                        .unwrap_or(Value::Null);
                    ToolCallResult::success(json!({ "backup": summary }))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_backup_create schema: {}",
                e
            ))),
        }
    }

    pub(super) async fn backup_list(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            #[serde(default)]
            namespace: Option<String>,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.backup.list(auth, req.namespace.as_deref()).await {
                Ok(list) => ToolCallResult::success(json!({ "backups": list })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_backup_list schema: {}",
                e
            ))),
        }
    }

    pub(super) async fn restore(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        match serde_json::from_value::<mem_core::RestoreRequest>(args) {
            Ok(req) => match self.backup.restore(auth, req).await {
                Ok(report) => {
                    ToolCallResult::success(serde_json::to_value(&report).unwrap_or(Value::Null))
                }
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_restore schema: {}",
                e
            ))),
        }
    }

    // ===== 整合 =====
}
