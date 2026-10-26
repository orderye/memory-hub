use super::*;

impl ToolDispatcher {
    pub(super) async fn grant(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            agent_id: AgentId,
            #[serde(default = "default_true")]
            can_read: bool,
            #[serde(default)]
            can_write: bool,
        }
        fn default_true() -> bool {
            true
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_grant schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Admin) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        // 沙箱语义：通配授权已废除，授权必须落在具体命名空间
        if req.namespace == "*" {
            return ToolCallResult::error(&HubError::InvalidArgument(
                "通配命名空间授权已废除（账户沙箱）：请对具体命名空间授权".to_string(),
            ));
        }
        if let Err(e) = mem_core::validate_namespace(&req.namespace) {
            return ToolCallResult::error(&e);
        }
        // 目标 Agent 必须与调用方同属一个账户
        match self.store.get_agent(&req.agent_id).await {
            Ok(Some(target)) if target.owner_account_id == auth.account_id => {}
            _ => {
                return ToolCallResult::error(&HubError::InvalidArgument(
                    "目标 Agent 不存在或不属于本账户".to_string(),
                ))
            }
        }
        // 仅命名空间 owner 可转授
        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        let is_owner = scoped
            .get_namespace(&req.namespace)
            .await
            .ok()
            .flatten()
            .map(|info| info.owner_agent == auth.agent_id)
            .unwrap_or(false);
        if !is_owner {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        let record = NamespaceGrantRecord {
            namespace: req.namespace.clone(),
            agent_id: req.agent_id.clone(),
            can_read: req.can_read,
            can_write: req.can_write,
            granted_by: auth.agent_id.clone(),
            created_at: chrono::Utc::now(),
        };
        match scoped.grant(&record).await {
            Ok(()) => ToolCallResult::success(json!({
                "granted": true,
                "namespace": req.namespace,
                "agent_id": req.agent_id,
                "can_read": req.can_read,
                "can_write": req.can_write,
            })),
            Err(e) => ToolCallResult::error(&e),
        }
    }

    pub(super) async fn revoke(&self, auth: &AuthCtx, args: Value) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            namespace: String,
            agent_id: AgentId,
        }
        let req = match serde_json::from_value::<Req>(args) {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(&HubError::InvalidArgument(format!(
                    "参数不符合 memory_revoke schema: {}",
                    e
                )))
            }
        };

        if !auth.has_scope(&Scope::Admin) {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }
        if req.namespace == "*" {
            return ToolCallResult::error(&HubError::InvalidArgument(
                "通配命名空间授权已废除（账户沙箱）".to_string(),
            ));
        }
        if let Err(e) = mem_core::validate_namespace(&req.namespace) {
            return ToolCallResult::error(&e);
        }
        match self.store.get_agent(&req.agent_id).await {
            Ok(Some(target)) if target.owner_account_id == auth.account_id => {}
            _ => {
                return ToolCallResult::error(&HubError::InvalidArgument(
                    "目标 Agent 不存在或不属于本账户".to_string(),
                ))
            }
        }
        let scoped = mem_core::AccountScopedStore::new(self.store.clone(), auth.account_id);
        let is_owner = scoped
            .get_namespace(&req.namespace)
            .await
            .ok()
            .flatten()
            .map(|info| info.owner_agent == auth.agent_id)
            .unwrap_or(false);
        if !is_owner {
            return ToolCallResult::error(&HubError::InsufficientScope);
        }

        match scoped.revoke(&req.namespace, &req.agent_id).await {
            Ok(()) => ToolCallResult::success(json!({
                "revoked": true,
                "namespace": req.namespace,
                "agent_id": req.agent_id,
            })),
            Err(e) => ToolCallResult::error(&e),
        }
    }

    // ===== 认知增强：Chat / Working / Coding =====

}
