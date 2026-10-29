use super::*;
use mem_core::infra::{EvidenceStore, ExperienceStore, MemoryStore, RelationStore, TaskStore};

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

    async fn replace_namespaces_from_snapshots(
        &self,
        snapshots: &[(Namespace, mem_core::RestoreSnapshot)],
    ) -> Result<()> {
        let items = self.items.read().unwrap().clone();
        let versions = self.versions.read().unwrap().clone();
        let evidences = self.evidences.read().unwrap().clone();
        let relations = self.relations.read().unwrap().clone();
        let tasks = self.tasks.read().unwrap().clone();
        let experiences = self.experiences.read().unwrap().clone();
        let experience_versions = self.experience_versions.read().unwrap().clone();
        let task_experiences = self.task_experiences.read().unwrap().clone();
        let coding_threads = self.coding_threads.read().unwrap().clone();
        let coding_revisions = self.coding_revisions.read().unwrap().clone();

        let restore = async {
            for (namespace, snapshot) in snapshots {
                self.items
                    .write()
                    .unwrap()
                    .retain(|(ns, _), _| ns != namespace);
                let ids: std::collections::HashSet<Uuid> = items
                    .keys()
                    .filter(|(ns, _)| ns == namespace)
                    .map(|(_, id)| *id)
                    .collect();
                self.versions
                    .write()
                    .unwrap()
                    .retain(|_, v| !ids.contains(&v.memory_id));
                self.evidences
                    .write()
                    .unwrap()
                    .retain(|_, e| e.namespace != *namespace);
                self.relations
                    .write()
                    .unwrap()
                    .retain(|_, (_, relation)| relation.namespace.as_deref() != Some(namespace));
                self.tasks
                    .write()
                    .unwrap()
                    .retain(|(ns, _), _| ns != namespace);
                self.experiences
                    .write()
                    .unwrap()
                    .retain(|(ns, _), _| ns != namespace);
                self.experience_versions
                    .write()
                    .unwrap()
                    .retain(|_, v| v.namespace != *namespace);
                self.task_experiences
                    .write()
                    .unwrap()
                    .retain(|(ns, _)| ns != namespace);
                self.coding_threads
                    .write()
                    .unwrap()
                    .retain(|(ns, _), _| ns != namespace);
                self.coding_revisions
                    .write()
                    .unwrap()
                    .retain(|(ns, _), _| ns != namespace);

                for entry in &snapshot.entries {
                    if entry.namespace != *namespace {
                        return Err(HubError::InvalidArgument(
                            "snapshot namespace mismatch".into(),
                        ));
                    }
                    self.insert_initial(entry).await?;
                }
                for evidence in &snapshot.evidences {
                    self.append_evidence(&NewEvidence {
                        id: Some(evidence.id),
                        namespace: evidence.namespace.clone(),
                        source_kind: evidence.source_kind,
                        source_ref: evidence.source_ref.clone(),
                        content_hash: evidence.content_hash.clone(),
                        excerpt: evidence.excerpt.clone(),
                        occurred_at: evidence.occurred_at,
                        expires_at: evidence.expires_at,
                    })
                    .await?;
                }
                for task in &snapshot.tasks {
                    self.create_task(&NewMemoryTask {
                        id: Some(task.id),
                        namespace: task.namespace.clone(),
                        scene: task.scene,
                        task: task.task.clone(),
                        status: task.status,
                        started_at: task.started_at,
                        finished_at: task.finished_at,
                        result_summary: task.result_summary.clone(),
                        failure_reason: task.failure_reason.clone(),
                        source_evidence_id: task.source_evidence_id,
                        metadata: task.metadata.clone(),
                    })
                    .await?;
                }
                for experience in &snapshot.experiences {
                    self.save_experience(&NewMemoryExperience {
                        id: Some(experience.id),
                        namespace: experience.namespace.clone(),
                        task: experience.task.clone(),
                        search_keys: experience.search_keys.clone(),
                        solution: experience.solution.clone(),
                        preconditions: experience.preconditions.clone(),
                        boundary_envs: experience.boundary_envs.clone(),
                        failure_modes: experience.failure_modes.clone(),
                        validation: experience.validation.clone(),
                        confidence: experience.confidence,
                        status: experience.status,
                        feedback_score: experience.feedback_score,
                    })
                    .await?;
                    self.update_experience(experience).await?;
                }
                for version in &snapshot.experience_versions {
                    self.append_experience_version(&NewExperienceVersion {
                        id: Some(version.id),
                        experience_id: version.experience_id,
                        namespace: version.namespace.clone(),
                        version: version.version,
                        snapshot: version.snapshot.clone(),
                        change_kind: version.change_kind,
                        changed_by: version.changed_by.clone(),
                        status: version.status,
                        note: version.note.clone(),
                        changed_at: version.changed_at,
                    })
                    .await?;
                }
                for link in &snapshot.task_experiences {
                    self.link_task_experience(link).await?;
                }
                for relation in &snapshot.relations {
                    self.add_relation(
                        namespace,
                        &NewMemoryRelation {
                            from_memory_id: relation.from_memory_id,
                            to_memory_id: relation.to_memory_id,
                            relation_kind: relation.relation_kind,
                            confidence: relation.confidence,
                            source_evidence_id: relation.source_evidence_id,
                        },
                    )
                    .await?;
                }
                for thread in &snapshot.coding_threads {
                    self.create_thread(&NewCodingThread {
                        id: Some(thread.id),
                        namespace: thread.namespace.clone(),
                        repository: thread.repository.clone(),
                        branch: thread.branch.clone(),
                        task_id: thread.task_id,
                        title: thread.title.clone(),
                        description: thread.description.clone(),
                    })
                    .await?;
                }
                for revision in &snapshot.coding_revisions {
                    self.save_revision(
                        &NewCodingRevision {
                            id: Some(revision.id),
                            thread_id: revision.thread_id,
                            namespace: revision.namespace.clone(),
                            repository: revision.repository.clone(),
                            branch: revision.branch.clone(),
                            commit_sha: revision.commit_sha.clone(),
                            parent_sha: revision.parent_sha.clone(),
                            file_path: revision.file_path.clone(),
                            change_kind: revision.change_kind,
                            diff_content: revision.diff_content.clone(),
                            symbol_fingerprint: revision.symbol_fingerprint.clone(),
                            is_committed: revision.is_committed,
                            author: revision.author.clone(),
                            occurred_at: Some(revision.occurred_at),
                            source_evidence_id: revision.source_evidence_id,
                            source_task_id: revision.source_task_id,
                        },
                        &revision.diff_hash,
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = restore {
            *self.items.write().unwrap() = items;
            *self.versions.write().unwrap() = versions;
            *self.evidences.write().unwrap() = evidences;
            *self.relations.write().unwrap() = relations;
            *self.tasks.write().unwrap() = tasks;
            *self.experiences.write().unwrap() = experiences;
            *self.experience_versions.write().unwrap() = experience_versions;
            *self.task_experiences.write().unwrap() = task_experiences;
            *self.coding_threads.write().unwrap() = coding_threads;
            *self.coding_revisions.write().unwrap() = coding_revisions;
            return Err(error);
        }
        Ok(())
    }
}
