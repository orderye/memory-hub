use super::*;

impl ToolDispatcher {
    pub(super) fn collections(&self) -> mem_core::CollectionService {
        mem_core::CollectionService::new(self.store.clone())
    }

    pub(super) async fn memory_collection_upsert(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        match serde_json::from_value::<mem_core::UpsertCollectionRequest>(args) {
            Ok(req) => match self.collections().upsert(auth, req).await {
                Ok(collection) => ToolCallResult::success(json!({ "collection": collection })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_collection_upsert schema: {}",
                e
            ))),
        }
    }

    pub(super) async fn memory_collection_list(
        &self,
        auth: &AuthCtx,
        _args: Value,
    ) -> ToolCallResult {
        match self.collections().list(auth).await {
            Ok(collections) => ToolCallResult::success(json!({ "collections": collections })),
            Err(e) => ToolCallResult::error(&e),
        }
    }

    pub(super) async fn memory_collection_delete(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            collection_id: uuid::Uuid,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self.collections().delete(auth, req.collection_id).await {
                Ok(()) => ToolCallResult::success(json!({
                    "deleted": true,
                    "collection_id": req.collection_id,
                    "note": "仅删除集合与绑定，底层记忆未被删除"
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_collection_delete schema: {}",
                e
            ))),
        }
    }

    pub(super) async fn memory_collection_bind(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            collection_id: uuid::Uuid,
            agent_id: String,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self
                .collections()
                .bind(auth, req.collection_id, &req.agent_id)
                .await
            {
                Ok(()) => ToolCallResult::success(json!({
                    "bound": true,
                    "collection_id": req.collection_id,
                    "agent_id": req.agent_id
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_collection_bind schema: {}",
                e
            ))),
        }
    }

    pub(super) async fn memory_collection_unbind(
        &self,
        auth: &AuthCtx,
        args: Value,
    ) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Req {
            collection_id: uuid::Uuid,
            agent_id: String,
        }
        match serde_json::from_value::<Req>(args) {
            Ok(req) => match self
                .collections()
                .unbind(auth, req.collection_id, &req.agent_id)
                .await
            {
                Ok(()) => ToolCallResult::success(json!({
                    "unbound": true,
                    "collection_id": req.collection_id,
                    "agent_id": req.agent_id,
                    "note": "解绑立即生效；底层记忆未被删除"
                })),
                Err(e) => ToolCallResult::error(&e),
            },
            Err(e) => ToolCallResult::error(&HubError::InvalidArgument(format!(
                "参数不符合 memory_collection_unbind schema: {}",
                e
            ))),
        }
    }
}
