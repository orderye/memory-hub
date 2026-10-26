use super::*;

#[async_trait]
impl mem_core::MemoryStore for InMemoryHubStore {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if w.contains_key(&key) {
            return Err(HubError::Conflict);
        }
        w.insert(key, entry.clone());
        Ok(entry.clone())
    }

    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if w.contains_key(&key) {
            return Err(HubError::Conflict);
        }
        w.insert(key, entry.clone());
        let version = MemoryVersion {
            memory_id: entry.id,
            version: entry.version,
            content: entry.content.clone(),
            tags: entry.tags.clone(),
            metadata: entry.metadata.clone(),
            confidence: entry.confidence,
            changed_by: entry.source_agent.clone(),
            change_type: VersionChange::Created,
            changed_at: entry.created_at,
            scene: entry.scene,
            layer: entry.layer,
            valid_from: entry.valid_from,
            valid_until: entry.valid_until,
            occurred_at: entry.occurred_at,
        };
        self.versions
            .write()
            .unwrap()
            .insert((entry.id, entry.version), version);
        Ok(entry.clone())
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        self.items
            .read()
            .unwrap()
            .get(&(namespace.to_string(), id))
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let r = self.items.read().unwrap();
        let mut candidates: Vec<_> = r
            .values()
            .filter(|e| e.namespace == namespace && e.key.as_deref() == Some(key))
            .cloned()
            .collect();
        // 状态优先级：active (0) > archived (1) > merged (2)，同状态按 updated_at 倒序
        candidates.sort_by(|a, b| {
            let status_priority = |s: MemoryStatus| match s {
                MemoryStatus::Active => 0,
                MemoryStatus::Archived => 1,
                MemoryStatus::Merged => 2,
            };
            status_priority(a.status)
                .cmp(&status_priority(b.status))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(candidates.into_iter().next())
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        // 契约：save 只保存已存在条目，不存在返回 NotFound（与 PostgreSQL
        // 实现对齐——update 0 行受影响同样报 NotFound）。无条件 upsert 会
        // 掩盖"调用方以为在更新、实则凭空新建"的分叉，内存测试验不出生产行为。
        let mut w = self.items.write().unwrap();
        let key = (entry.namespace.clone(), entry.id);
        if !w.contains_key(&key) {
            return Err(HubError::NotFound);
        }
        w.insert(key, entry.clone());
        Ok(entry.clone())
    }

    /// 与 PostgreSQL 实现同一语义的乐观并发修订：库中当前版本不是
    /// `expected_version` 就返回 Conflict，而不是静默覆盖。
    /// 两套实现在这一点上必须一致，否则内存测试验不出生产的丢更新行为。
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        Self::apply_revision(self, entry, Some(expected_version)).await
    }

    /// 原子修订：版本校验 + 写回条目 + 追加历史。
    /// 条目锁在追加历史之前释放（`RwLock` 守卫不是 `Send`，不得跨 `await` 持有）。
    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: mem_core::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let saved = Self::apply_revision(self, entry, Some(expected_version)).await?;
        self.record_version(&MemoryVersion {
            memory_id: saved.id,
            version: saved.version,
            content: saved.content.clone(),
            tags: saved.tags.clone(),
            metadata: saved.metadata.clone(),
            confidence: saved.confidence,
            changed_by: saved.source_agent.clone(),
            change_type: change,
            changed_at: saved.updated_at,
            scene: saved.scene,
            layer: saved.layer,
            valid_from: saved.valid_from,
            valid_until: saved.valid_until,
            occurred_at: saved.occurred_at,
        })
        .await?;
        Ok(saved)
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        let mut w = self.items.write().unwrap();
        w.remove(&(namespace.to_string(), id));
        let mut rels = self.relations.write().unwrap();
        rels.retain(|(from_id, to_id, _), _| *from_id != id && *to_id != id);
        Ok(())
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        // 查询词元只切一次；每条文档的词元也只算一次（此前 filter 与排序各算一遍，
        // 且每次都重新切查询词，大语料下是主要开销）
        let query_tokens: Option<std::collections::HashSet<String>> = filter
            .text_contains
            .as_deref()
            .map(|t| mem_core::fts_query_tokens(t).into_iter().collect());

        let mut out: Vec<(usize, MemoryEntry)> = Vec::new();
        for e in self.items.read().unwrap().values() {
            if !Self::matches_filter_without_text(e, filter) {
                continue;
            }
            let hits = match &query_tokens {
                Some(qt) => {
                    let doc_tokens = mem_core::document_tokens(&e.content);
                    let hits = qt.intersection(&doc_tokens).count();
                    if hits == 0 {
                        continue;
                    }
                    hits
                }
                None => 0,
            };
            out.push((hits, e.clone()));
        }

        // 与 PostgreSQL 侧一致：带查询词时按词法相关度（词元命中数）排序，
        // 不做"按更新时间截断"——否则更相关但更旧的文档会在打分前被丢弃
        if query_tokens.is_some() {
            out.sort_by(|(ha, a), (hb, b)| {
                hb.cmp(ha)
                    .then(b.updated_at.cmp(&a.updated_at))
                    .then(b.id.cmp(&a.id))
            });
        } else {
            out.sort_by(|(_, a), (_, b)| b.updated_at.cmp(&a.updated_at).then(b.id.cmp(&a.id)));
        }

        let mut out: Vec<MemoryEntry> = out.into_iter().map(|(_, e)| e).collect();
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        let no_limit = MemoryFilter {
            limit: None,
            ..filter.clone()
        };
        Ok(self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, &no_limit))
            .count())
    }

    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        // 状态缺省时按 active 处理（与历史行为一致，避免默认召回已归档条目）
        let mut effective = filter.clone();
        if effective.statuses.is_none() {
            effective.statuses = Some(vec![MemoryStatus::Active]);
        }
        let mut scored: Vec<Scored<MemoryEntry>> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| Self::matches_filter(e, &effective))
            .filter(|e| e.embedding.is_some())
            .filter_map(|e| {
                let iv = e.embedding.as_ref()?;
                if iv.len() != embedding.len() {
                    return None;
                }
                let dot: f32 = iv.iter().zip(embedding.iter()).map(|(x, y)| x * y).sum();
                Some(Scored {
                    item: e.clone(),
                    score: dot.clamp(0.0, 1.0),
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let mut out: Vec<MemoryEntry> = self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.namespace == namespace)
            .filter(|e| include_merged || e.status != MemoryStatus::Merged)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let mut ns: Vec<Namespace> = self
            .items
            .read()
            .unwrap()
            .values()
            .map(|e| e.namespace.clone())
            .collect();
        ns.sort();
        ns.dedup();
        Ok(ns)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        let mut w = self.versions.write().unwrap();
        // 同 (memory_id, version) 只保留首条（幂等重放保护）；BTreeMap 天然保证唯一性
        w.entry((v.memory_id, v.version))
            .or_insert_with(|| v.clone());
        Ok(())
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        let out: Vec<MemoryVersion> = self
            .versions
            .read()
            .unwrap()
            .range((id, i64::MIN)..(id, i64::MAX))
            .map(|(_, v)| v.clone())
            .collect();
        Ok(out)
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 第一遍：收集待归档条目快照（避免持锁写版本表）
        let expired: Vec<MemoryEntry> = {
            let r = self.items.read().unwrap();
            r.values()
                .filter(|e| {
                    e.status == MemoryStatus::Active && e.expires_at.is_some_and(|exp| exp <= now)
                })
                .cloned()
                .collect()
        };

        // 第二遍：逐条归档并记录版本历史
        let mut count = 0usize;
        for mut e in expired {
            e.status = MemoryStatus::Archived;
            e.version += 1;
            e.updated_at = now;
            let version = MemoryVersion {
                memory_id: e.id,
                version: e.version,
                content: e.content.clone(),
                tags: e.tags.clone(),
                metadata: e.metadata.clone(),
                confidence: e.confidence,
                changed_by: "system".to_string(),
                change_type: VersionChange::Archived,
                changed_at: now,
                scene: e.scene,
                layer: e.layer,
                valid_from: e.valid_from,
                valid_until: e.valid_until,
                occurred_at: e.occurred_at,
            };
            self.items
                .write()
                .unwrap()
                .insert((e.namespace.clone(), e.id), e);
            self.record_version(&version).await?;
            count += 1;
        }
        Ok(count)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}
