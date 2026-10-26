use super::*;

#[async_trait]
impl mem_core::BackupStore for InMemoryHubStore {
    async fn save_backup(&self, backup: &BackupRecord) -> Result<()> {
        self.backups
            .write()
            .unwrap()
            .insert(backup.id, backup.clone());
        Ok(())
    }

    async fn get_backup(&self, id: Uuid) -> Result<BackupRecord> {
        self.backups
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    /// 列举快照（摘要视图）：与 PostgreSQL 实现保持同一语义——不返回 `payload`，
    /// 需要负载的调用方必须走 `get_backup(id)`。两套实现若在这一点上分叉，
    /// 内存测试就会验不出生产行为。
    async fn list_backups(&self, namespace: Option<&str>) -> Result<Vec<BackupRecord>> {
        let mut out: Vec<BackupRecord> = self
            .backups
            .read()
            .unwrap()
            .values()
            .filter(|b| namespace.is_none_or(|n| b.namespace == n))
            .cloned()
            .map(|mut b| {
                b.payload = serde_json::Value::Null;
                b
            })
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn delete_backup(&self, id: Uuid) -> Result<()> {
        self.backups.write().unwrap().remove(&id);
        Ok(())
    }
}
