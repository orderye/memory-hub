use super::*;

#[async_trait]
impl CodingStore for InMemoryHubStore {
    async fn create_thread(&self, thread: &NewCodingThread) -> Result<CodingThread> {
        if let Some(task_id) = thread.task_id {
            let tasks = self.tasks.read().unwrap();
            if !tasks.contains_key(&(thread.namespace.clone(), task_id)) {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }

        let id = thread.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = CodingThread {
            id,
            namespace: thread.namespace.clone(),
            repository: thread.repository.clone(),
            branch: thread.branch.clone(),
            task_id: thread.task_id,
            title: thread.title.clone(),
            description: thread.description.clone(),
            created_at: now,
            updated_at: now,
        };
        self.coding_threads
            .write()
            .unwrap()
            .insert((thread.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn get_thread(&self, namespace: &str, id: Uuid) -> Result<Option<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_threads(
        &self,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|t| {
                t.namespace == namespace && repository.is_none_or(|repo| t.repository == repo)
            })
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn save_revision(
        &self,
        revision: &NewCodingRevision,
        diff_hash: &str,
    ) -> Result<CodingRevision> {
        if let Some(tid) = revision.thread_id {
            let threads = self.coding_threads.read().unwrap();
            if !threads.contains_key(&(revision.namespace.clone(), tid)) {
                return Err(HubError::InvalidArgument(
                    "关联线程不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(task_id) = revision.source_task_id {
            let tasks = self.tasks.read().unwrap();
            if !tasks.contains_key(&(revision.namespace.clone(), task_id)) {
                return Err(HubError::InvalidArgument(
                    "关联任务不存在于该命名空间".into(),
                ));
            }
        }
        if let Some(ev_id) = revision.source_evidence_id {
            let evs = self.evidences.read().unwrap();
            if !evs.contains_key(&(revision.namespace.clone(), ev_id)) {
                return Err(HubError::InvalidArgument(
                    "关联证据不存在于该命名空间".into(),
                ));
            }
        }

        let id = revision.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = CodingRevision {
            id,
            thread_id: revision.thread_id,
            namespace: revision.namespace.clone(),
            repository: revision.repository.clone(),
            branch: revision.branch.clone(),
            commit_sha: revision.commit_sha.clone(),
            parent_sha: revision.parent_sha.clone(),
            file_path: revision.file_path.clone(),
            change_kind: revision.change_kind,
            diff_content: revision.diff_content.clone(),
            diff_hash: diff_hash.to_string(),
            symbol_fingerprint: revision.symbol_fingerprint.clone(),
            is_committed: revision.is_committed,
            author: revision.author.clone(),
            occurred_at: revision.occurred_at.unwrap_or(now),
            created_at: now,
            source_evidence_id: revision.source_evidence_id,
            source_task_id: revision.source_task_id,
        };
        self.coding_revisions
            .write()
            .unwrap()
            .insert((revision.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn get_revision(&self, namespace: &str, id: Uuid) -> Result<Option<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn get_revision_by_hash(
        &self,
        namespace: &str,
        repository: &str,
        diff_hash: &str,
    ) -> Result<Option<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.values()
            .find(|rev| {
                rev.namespace == namespace
                    && rev.repository == repository
                    && rev.diff_hash == diff_hash
            })
            .cloned())
    }

    async fn list_revisions_for_thread(
        &self,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|rev| rev.namespace == namespace && rev.thread_id == Some(thread_id))
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.occurred_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn list_revisions_for_file(
        &self,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        let mut list: Vec<_> = r
            .values()
            .filter(|rev| {
                rev.namespace == namespace
                    && rev.repository == repository
                    && rev.file_path == file_path
            })
            .cloned()
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.occurred_at));
        list.truncate(limit);
        Ok(list)
    }

    async fn all_threads_in_namespace(&self, namespace: &str) -> Result<Vec<CodingThread>> {
        let r = self.coding_threads.read().unwrap();
        Ok(r.values()
            .filter(|t| t.namespace == namespace)
            .cloned()
            .collect())
    }

    async fn all_revisions_in_namespace(&self, namespace: &str) -> Result<Vec<CodingRevision>> {
        let r = self.coding_revisions.read().unwrap();
        Ok(r.values()
            .filter(|rev| rev.namespace == namespace)
            .cloned()
            .collect())
    }

    async fn delete_thread(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.coding_threads.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut revs = self.coding_revisions.write().unwrap();
            revs.retain(|_, r| r.thread_id != Some(id));
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }

    async fn delete_revision(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.coding_revisions.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}
