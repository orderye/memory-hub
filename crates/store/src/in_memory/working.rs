use super::*;

#[async_trait]
impl mem_core::infra::EvidenceStore for InMemoryHubStore {
    async fn append_evidence(&self, evidence: &NewEvidence) -> Result<MemoryEvidence> {
        let id = evidence.id.unwrap_or_else(Uuid::new_v4);
        let created_at = Utc::now();
        let ev = MemoryEvidence {
            id,
            namespace: evidence.namespace.clone(),
            source_kind: evidence.source_kind,
            source_ref: evidence.source_ref.clone(),
            content_hash: evidence.content_hash.clone(),
            excerpt: evidence.excerpt.clone(),
            occurred_at: evidence.occurred_at,
            created_at,
            expires_at: evidence.expires_at,
        };
        let mut w = self.evidences.write().unwrap();
        w.insert((evidence.namespace.clone(), id), ev.clone());
        Ok(ev)
    }

    async fn get_evidence(&self, namespace: &str, id: Uuid) -> Result<MemoryEvidence> {
        let r = self.evidences.read().unwrap();
        r.get(&(namespace.to_string(), id))
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_evidence(
        &self,
        namespace: &str,
        source_kind: Option<EvidenceSourceKind>,
        source_ref: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEvidence>> {
        let r = self.evidences.read().unwrap();
        let mut list: Vec<MemoryEvidence> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, ev)| ev.clone())
            .filter(|ev| {
                if let Some(kind) = source_kind {
                    if ev.source_kind != kind {
                        return false;
                    }
                }
                if let Some(r_ref) = source_ref {
                    if ev.source_ref.as_deref() != Some(r_ref) {
                        return false;
                    }
                }
                true
            })
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        list.truncate(limit.max(1));
        Ok(list)
    }

    /// 按内容哈希点查：与列表实现同源的过滤条件，但不截断窗口，
    /// 覆盖该会话全部历史证据
    async fn find_evidence_by_hash(
        &self,
        namespace: &str,
        source_kind: EvidenceSourceKind,
        source_ref: &str,
        content_hash: &str,
    ) -> Result<Option<MemoryEvidence>> {
        let r = self.evidences.read().unwrap();
        Ok(r.values()
            .filter(|ev| {
                ev.namespace == namespace
                    && ev.source_kind == source_kind
                    && ev.source_ref.as_deref() == Some(source_ref)
                    && ev.content_hash == content_hash
            })
            .max_by_key(|ev| ev.created_at)
            .cloned())
    }

    async fn all_evidence_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryEvidence>> {
        let r = self.evidences.read().unwrap();
        let mut list: Vec<MemoryEvidence> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, ev)| ev.clone())
            .collect();
        list.sort_by_key(|e| (e.created_at, e.id));
        Ok(list)
    }

    async fn delete_evidence(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.evidences.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_none() {
            Err(HubError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn purge_expired_evidence(&self, namespace: &str, now: DateTime<Utc>) -> Result<usize> {
        let mut w = self.evidences.write().unwrap();
        let before = w.len();
        w.retain(|(ns, _), ev| !(ns == namespace && ev.expires_at.is_some_and(|exp| exp <= now)));
        Ok(before - w.len())
    }
}

#[async_trait]
impl mem_core::infra::RelationStore for InMemoryHubStore {
    async fn add_relation(
        &self,
        namespace: &str,
        relation: &NewMemoryRelation,
    ) -> Result<MemoryRelation> {
        if relation.from_memory_id == relation.to_memory_id {
            return Err(HubError::InvalidArgument("自环关系不合法".to_string()));
        }
        let items = self.items.read().unwrap();
        let from_exists = items.contains_key(&(namespace.to_string(), relation.from_memory_id));
        let to_exists = items.contains_key(&(namespace.to_string(), relation.to_memory_id));
        if !from_exists || !to_exists {
            return Err(HubError::NotFound);
        }
        if let Some(ev_id) = relation.source_evidence_id {
            let evs = self.evidences.read().unwrap();
            if !evs.contains_key(&(namespace.to_string(), ev_id)) {
                return Err(HubError::NotFound);
            }
        }
        let created_at = Utc::now();
        let rel = MemoryRelation {
            namespace: Some(namespace.to_string()),
            from_memory_id: relation.from_memory_id,
            to_memory_id: relation.to_memory_id,
            relation_kind: relation.relation_kind,
            confidence: relation.confidence,
            source_evidence_id: relation.source_evidence_id,
            created_at,
        };
        let mut w = self.relations.write().unwrap();
        w.insert(
            (
                relation.from_memory_id,
                relation.to_memory_id,
                relation.relation_kind,
            ),
            (namespace.to_string(), rel.clone()),
        );
        Ok(rel)
    }

    async fn list_relations_from(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, rel)| ns == namespace && rel.from_memory_id == from_memory_id)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list)
    }

    async fn list_relations_to(
        &self,
        namespace: &str,
        to_memory_id: Uuid,
    ) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, rel)| ns == namespace && rel.to_memory_id == to_memory_id)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        Ok(list)
    }

    async fn delete_relation(
        &self,
        namespace: &str,
        from_memory_id: Uuid,
        to_memory_id: Uuid,
        relation_kind: MemoryRelationKind,
    ) -> Result<()> {
        let mut w = self.relations.write().unwrap();
        let key = (from_memory_id, to_memory_id, relation_kind);
        if let Some((ns, _)) = w.get(&key) {
            if ns == namespace {
                w.remove(&key);
                return Ok(());
            }
        }
        Err(HubError::NotFound)
    }

    async fn all_relations_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryRelation>> {
        let r = self.relations.read().unwrap();
        let mut list: Vec<MemoryRelation> = r
            .values()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, rel)| rel.clone())
            .collect();
        list.sort_by_key(|rel| (rel.created_at, rel.from_memory_id, rel.to_memory_id));
        Ok(list)
    }
}

#[async_trait]
impl mem_core::infra::TaskStore for InMemoryHubStore {
    async fn create_task(&self, task: &NewMemoryTask) -> Result<MemoryTask> {
        let id = task.id.unwrap_or_else(Uuid::new_v4);
        let record = MemoryTask {
            id,
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
        };
        let mut w = self.tasks.write().unwrap();
        w.insert((task.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn update_task(&self, task: &MemoryTask) -> Result<MemoryTask> {
        let mut w = self.tasks.write().unwrap();
        let key = (task.namespace.clone(), task.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        w.insert(key, task.clone());
        Ok(task.clone())
    }

    async fn get_task(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_tasks(
        &self,
        namespace: &str,
        status: Option<TaskStatus>,
        limit: usize,
    ) -> Result<Vec<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        let mut list: Vec<MemoryTask> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, t)| t.clone())
            .filter(|t| {
                if let Some(s) = status {
                    t.status == s
                } else {
                    true
                }
            })
            .collect();
        list.sort_by_key(|t| std::cmp::Reverse(t.started_at));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn all_tasks_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryTask>> {
        let r = self.tasks.read().unwrap();
        let mut list: Vec<MemoryTask> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, t)| t.clone())
            .collect();
        list.sort_by_key(|t| (t.started_at, t.id));
        Ok(list)
    }

    async fn delete_task(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.tasks.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut te = self.task_experiences.write().unwrap();
            te.retain(|(_, l)| l.task_id != id);
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}

#[async_trait]
impl mem_core::infra::ExperienceStore for InMemoryHubStore {
    async fn save_experience(&self, exp: &NewMemoryExperience) -> Result<MemoryExperience> {
        let id = exp.id.unwrap_or_else(Uuid::new_v4);
        let now = Utc::now();
        let record = MemoryExperience {
            id,
            namespace: exp.namespace.clone(),
            task: exp.task.clone(),
            search_keys: exp.search_keys.clone(),
            solution: exp.solution.clone(),
            preconditions: exp.preconditions.clone(),
            boundary_envs: exp.boundary_envs.clone(),
            failure_modes: exp.failure_modes.clone(),
            validation: exp.validation.clone(),
            confidence: exp.confidence,
            status: exp.status,
            feedback_score: exp.feedback_score,
            version: 1,
            superseded_by: None,
            verified_at: None,
            verified_by: None,
            verification_note: None,
            status_reason: None,
            source_count: 0,
            created_at: now,
            updated_at: now,
        };
        let mut w = self.experiences.write().unwrap();
        w.insert((exp.namespace.clone(), id), record.clone());
        Ok(record)
    }

    async fn update_experience(&self, exp: &MemoryExperience) -> Result<MemoryExperience> {
        let mut w = self.experiences.write().unwrap();
        let key = (exp.namespace.clone(), exp.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        let mut updated = exp.clone();
        updated.updated_at = Utc::now();
        w.insert(key, updated.clone());
        Ok(updated)
    }

    async fn update_experience_if_version(
        &self,
        exp: &MemoryExperience,
        expected_version: i64,
    ) -> Result<MemoryExperience> {
        let mut w = self.experiences.write().unwrap();
        let key = (exp.namespace.clone(), exp.id);
        match w.get(&key) {
            Some(current) if current.version == expected_version => {
                let mut updated = exp.clone();
                updated.updated_at = Utc::now();
                w.insert(key, updated.clone());
                Ok(updated)
            }
            Some(_) => Err(HubError::Conflict),
            None => Err(HubError::NotFound),
        }
    }

    async fn append_experience_version(
        &self,
        version: &NewExperienceVersion,
    ) -> Result<ExperienceVersion> {
        let mut w = self.experience_versions.write().unwrap();
        let key = (version.experience_id, version.version);
        if let Some(existing) = w.get(&key) {
            return Ok(existing.clone());
        }
        let record = ExperienceVersion {
            id: version.id.unwrap_or_else(Uuid::new_v4),
            experience_id: version.experience_id,
            namespace: version.namespace.clone(),
            version: version.version,
            snapshot: version.snapshot.clone(),
            change_kind: version.change_kind,
            changed_by: version.changed_by.clone(),
            status: version.status,
            note: version.note.clone(),
            changed_at: version.changed_at,
        };
        w.insert(key, record.clone());
        Ok(record)
    }

    async fn list_experience_versions(
        &self,
        namespace: &str,
        experience_id: Uuid,
        limit: usize,
    ) -> Result<Vec<ExperienceVersion>> {
        let r = self.experience_versions.read().unwrap();
        let mut list: Vec<ExperienceVersion> = r
            .values()
            .filter(|v| v.namespace == namespace && v.experience_id == experience_id)
            .cloned()
            .collect();
        list.sort_by_key(|v| std::cmp::Reverse(v.version));
        list.truncate(limit.max(1));
        Ok(list)
    }

    async fn all_experience_versions_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<ExperienceVersion>> {
        let r = self.experience_versions.read().unwrap();
        let mut list: Vec<ExperienceVersion> = r
            .values()
            .filter(|v| v.namespace == namespace)
            .cloned()
            .collect();
        list.sort_by_key(|v| (v.experience_id, v.version));
        Ok(list)
    }

    async fn get_experience(&self, namespace: &str, id: Uuid) -> Result<Option<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        Ok(r.get(&(namespace.to_string(), id)).cloned())
    }

    async fn list_experiences(
        &self,
        namespace: &str,
        status: Option<ExperienceStatus>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        let mut list: Vec<MemoryExperience> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, e)| e.clone())
            .filter(|e| {
                if let Some(s) = status {
                    e.status == s
                } else {
                    true
                }
            })
            .collect();
        list.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
        Ok(list.into_iter().skip(offset).take(limit.max(1)).collect())
    }

    async fn link_task_experience(&self, link: &MemoryTaskExperience) -> Result<()> {
        let tasks = self.tasks.read().unwrap();
        let experiences = self.experiences.read().unwrap();

        let task_ns = tasks
            .iter()
            .find(|((_, tid), _)| *tid == link.task_id)
            .map(|((ns, _), _)| ns.clone())
            .ok_or(HubError::NotFound)?;

        let exp_ns = experiences
            .iter()
            .find(|((_, eid), _)| *eid == link.experience_id)
            .map(|((ns, _), _)| ns.clone())
            .ok_or(HubError::NotFound)?;

        if task_ns != exp_ns {
            return Err(HubError::InvalidArgument(
                "任务与经验不属于同一命名空间".into(),
            ));
        }

        if let Some(ref link_ns) = link.namespace {
            if *link_ns != task_ns {
                return Err(HubError::InvalidArgument(
                    "指定命名空间与任务所属命名空间不一致".into(),
                ));
            }
        }

        let mut w = self.task_experiences.write().unwrap();
        if !w.iter().any(|(_, l)| {
            l.task_id == link.task_id
                && l.experience_id == link.experience_id
                && l.relation_kind == link.relation_kind
        }) {
            let mut stored = link.clone();
            stored.namespace = Some(task_ns.clone());
            w.push((task_ns, stored));
        }
        Ok(())
    }

    async fn list_experiences_for_task(&self, task_id: Uuid) -> Result<Vec<MemoryTaskExperience>> {
        let r = self.task_experiences.read().unwrap();
        let list: Vec<MemoryTaskExperience> = r
            .iter()
            .filter(|(_, l)| l.task_id == task_id)
            .map(|(_, l)| l.clone())
            .collect();
        Ok(list)
    }

    async fn all_experiences_in_namespace(&self, namespace: &str) -> Result<Vec<MemoryExperience>> {
        let r = self.experiences.read().unwrap();
        let mut list: Vec<MemoryExperience> = r
            .iter()
            .filter(|((ns, _), _)| ns == namespace)
            .map(|(_, e)| e.clone())
            .collect();
        list.sort_by_key(|e| (e.created_at, e.id));
        Ok(list)
    }

    async fn all_task_experiences_in_namespace(
        &self,
        namespace: &str,
    ) -> Result<Vec<MemoryTaskExperience>> {
        let r = self.task_experiences.read().unwrap();
        let list: Vec<MemoryTaskExperience> = r
            .iter()
            .filter(|(ns, _)| ns == namespace)
            .map(|(_, l)| l.clone())
            .collect();
        Ok(list)
    }

    async fn delete_experience(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.experiences.write().unwrap();
        if w.remove(&(namespace.to_string(), id)).is_some() {
            let mut te = self.task_experiences.write().unwrap();
            te.retain(|(_, l)| l.experience_id != id);
            let mut versions = self.experience_versions.write().unwrap();
            versions.retain(|(exp_id, _), _| *exp_id != id);
            Ok(())
        } else {
            Err(HubError::NotFound)
        }
    }
}
