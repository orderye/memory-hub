use super::*;

/// Agent 记忆集合的内存实现（语义与 PostgreSQL 对齐）
#[async_trait]
impl mem_core::infra::CollectionStore for InMemoryHubStore {
    async fn upsert_collection(&self, collection: &MemoryCollection) -> Result<MemoryCollection> {
        let mut w = self.collections.write().unwrap();
        let mut record = collection.clone();
        if let Some(existing) = w
            .iter_mut()
            .find(|c| c.account_id == record.account_id && c.name == record.name)
        {
            // 同名更新：保留既有 id 与创建者，刷新过滤条件与时间
            existing.description = record.description;
            existing.filter = record.filter;
            existing.updated_at = Utc::now();
            return Ok(existing.clone());
        }
        record.updated_at = Utc::now();
        w.push(record.clone());
        Ok(record)
    }

    async fn get_collection(
        &self,
        account_id: AccountId,
        id: Uuid,
    ) -> Result<Option<MemoryCollection>> {
        let r = self.collections.read().unwrap();
        Ok(r.iter()
            .find(|c| c.account_id == account_id && c.id == id)
            .cloned())
    }

    async fn get_collection_by_name(
        &self,
        account_id: AccountId,
        name: &str,
    ) -> Result<Option<MemoryCollection>> {
        let r = self.collections.read().unwrap();
        Ok(r.iter()
            .find(|c| c.account_id == account_id && c.name == name)
            .cloned())
    }

    async fn list_collections(&self, account_id: AccountId) -> Result<Vec<MemoryCollection>> {
        let r = self.collections.read().unwrap();
        let mut list: Vec<MemoryCollection> = r
            .iter()
            .filter(|c| c.account_id == account_id)
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(list)
    }

    async fn delete_collection(&self, account_id: AccountId, id: Uuid) -> Result<()> {
        let mut w = self.collections.write().unwrap();
        let before = w.len();
        w.retain(|c| !(c.account_id == account_id && c.id == id));
        if w.len() == before {
            return Err(HubError::NotFound);
        }
        // 级联删除绑定；不触碰任何记忆
        let mut bindings = self.collection_bindings.write().unwrap();
        bindings.retain(|b| b.collection_id != id);
        Ok(())
    }

    async fn bind_collection(&self, binding: &CollectionBinding) -> Result<()> {
        let mut w = self.collection_bindings.write().unwrap();
        if !w
            .iter()
            .any(|b| b.collection_id == binding.collection_id && b.agent_id == binding.agent_id)
        {
            w.push(binding.clone());
        }
        Ok(())
    }

    async fn unbind_collection(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<()> {
        let mut w = self.collection_bindings.write().unwrap();
        let before = w.len();
        w.retain(|b| !(b.collection_id == collection_id && b.agent_id == *agent_id));
        if w.len() == before {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn list_bindings(&self, collection_id: Uuid) -> Result<Vec<CollectionBinding>> {
        let r = self.collection_bindings.read().unwrap();
        let mut list: Vec<CollectionBinding> = r
            .iter()
            .filter(|b| b.collection_id == collection_id)
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
        });
        Ok(list)
    }

    async fn is_bound(&self, collection_id: Uuid, agent_id: &AgentId) -> Result<bool> {
        let r = self.collection_bindings.read().unwrap();
        Ok(r.iter()
            .any(|b| b.collection_id == collection_id && b.agent_id == *agent_id))
    }
}
