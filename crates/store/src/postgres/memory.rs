use super::*;

#[async_trait]
impl mem_core::MemoryStore for PostgresHubStore {
    async fn insert(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut conn = self.pool.acquire().await.pg()?;
        Self::insert_memory_on(&mut conn, entry).await?;
        Ok(entry.clone())
    }

    /// 原子插入新条目并追加初始版本历史（VersionChange::Created）
    async fn insert_initial(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut tx = self.pool.begin().await.pg()?;
        Self::insert_memory_on(&mut tx, entry).await?;

        let row = MemoryVersion {
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
        Self::insert_version_on(&mut tx, &row).await?;
        tx.commit().await.pg()?;
        Ok(entry.clone())
    }

    async fn get(&self, namespace: &str, id: Uuid) -> Result<MemoryEntry> {
        let sql = format!(
            "select {} from memories where namespace = $1 and id = $2",
            Self::ENTRY_COLUMNS_NO_EMBEDDING
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .pg()?
            .ok_or(HubError::NotFound)?;
        Self::entry_from_row_slim(&row)
    }

    async fn get_by_key(&self, namespace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        let sql = format!(
            r#"
            select {} from memories
            where namespace = $1 and key = $2
            order by (case when status = 'active' then 0 when status = 'archived' then 1 else 2 end),
                     updated_at desc
            limit 1
            "#,
            Self::ENTRY_COLUMNS_NO_EMBEDDING
        );
        let row = sqlx::query(&sql)
            .bind(namespace)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .pg()?;
        row.map(|r| Self::entry_from_row_slim(&r)).transpose()
    }

    async fn save(&self, entry: &MemoryEntry) -> Result<MemoryEntry> {
        let mut conn = self.pool.acquire().await.pg()?;
        let rows = Self::apply_update_on(&mut conn, entry, None).await?;
        // 契约：save 只保存已存在条目；条目不存在（0 行受影响）必须报
        // NotFound，静默吞掉会变成"调用方以为写成功实则丢写"
        if rows == 0 {
            return Err(HubError::NotFound);
        }
        Ok(entry.clone())
    }

    /// 乐观并发修订：版本号不符即拒写，绝不静默覆盖并发修订
    async fn save_if_version(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
    ) -> Result<MemoryEntry> {
        let mut conn = self.pool.acquire().await.pg()?;
        let rows = Self::apply_update_on(&mut conn, entry, Some(expected_version)).await?;
        if rows == 0 {
            return Err(HubError::Conflict);
        }
        Ok(entry.clone())
    }

    /// 原子修订：乐观锁写回条目 + 追加版本历史，同一事务同成同败
    async fn revise(
        &self,
        entry: &MemoryEntry,
        expected_version: i64,
        change: mem_core::domain::VersionChange,
    ) -> Result<MemoryEntry> {
        let mut tx = self.pool.begin().await.pg()?;
        let rows = Self::apply_update_on(&mut tx, entry, Some(expected_version)).await?;
        if rows == 0 {
            // 版本不符：不追加历史，事务在此丢弃
            return Err(HubError::Conflict);
        }
        let row = MemoryVersion {
            memory_id: entry.id,
            version: entry.version,
            content: entry.content.clone(),
            tags: entry.tags.clone(),
            metadata: entry.metadata.clone(),
            confidence: entry.confidence,
            changed_by: entry.source_agent.clone(),
            change_type: change,
            changed_at: entry.updated_at,
            scene: entry.scene,
            layer: entry.layer,
            valid_from: entry.valid_from,
            valid_until: entry.valid_until,
            occurred_at: entry.occurred_at,
        };
        Self::insert_version_on(&mut tx, &row).await?;
        tx.commit().await.pg()?;
        Ok(entry.clone())
    }

    async fn delete(&self, namespace: &str, id: Uuid) -> Result<()> {
        sqlx::query("delete from memories where namespace = $1 and id = $2")
            .bind(namespace)
            .bind(id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn list(&self, filter: &MemoryFilter) -> Result<Vec<MemoryEntry>> {
        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
            "select {} from memories where 1 = 1",
            Self::ENTRY_COLUMNS_NO_EMBEDDING
        ));
        Self::push_memory_filters(&mut qb, filter);

        match filter
            .text_contains
            .as_deref()
            .and_then(mem_core::cjk::fts_or_query)
        {
            // 带查询词时按词法相关度排序：候选池此前按 updated_at 截断，
            // 更相关但更旧的文档会在打分前被系统性丢弃
            Some(or_query) => {
                qb.push(
                    " order by ts_rank_cd(to_tsvector('simple', tsv_tokens), to_tsquery('simple', ",
                )
                .push_bind(or_query)
                .push(")) desc, updated_at desc, id desc");
            }
            None => {
                qb.push(" order by updated_at desc, id desc");
            }
        }
        if let Some(limit) = filter.limit {
            qb.push(" limit ").push_bind(limit as i64);
        }

        let rows = qb.build().fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::entry_from_row_slim).collect()
    }

    async fn count(&self, filter: &MemoryFilter) -> Result<usize> {
        let mut qb =
            sqlx::QueryBuilder::<sqlx::Postgres>::new("select count(*) from memories where 1 = 1");
        Self::push_memory_filters(&mut qb, filter);
        let count: i64 = qb.build_query_scalar().fetch_one(&self.pool).await.pg()?;
        Ok(count.max(0) as usize)
    }

    async fn similar(
        &self,
        filter: &MemoryFilter,
        embedding: &[f32],
        k: usize,
    ) -> Result<Vec<Scored<MemoryEntry>>> {
        // 状态缺省按 active 处理（与历史行为一致，避免默认召回已归档条目）
        let mut effective = filter.clone();
        if effective.statuses.is_none() {
            effective.statuses = Some(vec![MemoryStatus::Active]);
        }

        let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("select ");
        qb.push(Self::ENTRY_COLUMNS_NO_EMBEDDING)
            // 瘦列契约：ANN 只需要距离排序，条目不携带向量（MMR 走 embeddings_by_ids）。
            // dist 表达式须与迁移 015 的表达式索引逐字一致，规划器才能命中
            // idx_memories_embedding_hf（halfvec 半精度，索引体积减半）。
            .push(", (embedding::halfvec(1024) <=> ");
        qb.push_bind(mapping::halfvec_param(embedding))
            .push(") as dist from memories where embedding is not null");
        // 过滤条件下推到 SQL：否则其它场景/已失效条目会占满 top-k，导致目标候选缺失
        Self::push_memory_filters(&mut qb, &effective);
        qb.push(" order by dist limit ").push_bind(k as i64);

        let rows = qb.build().fetch_all(&self.pool).await.pg()?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let item = Self::entry_from_row_slim(r)?;
            // halfvec 余弦距离与 vector 一样返回 double precision（f8），
            // pgvector 0.7/0.8 的 halfvec 距离函数均为 float8，不存在 f4 特例。
            let dist: f64 = r.try_get("dist").pg()?;
            out.push(Scored {
                item,
                score: (1.0 - dist as f32).clamp(0.0, 1.0),
            });
        }
        Ok(out)
    }

    /// 批量取回条目向量（瘦列契约的二次取回，见 trait 文档）
    ///
    /// 命名空间是硬隔离条件；无向量 / 不在命名空间内的 id 自然缺席。
    /// embedding 列走 pgvector 二进制协议解码，不做文本序列化。
    async fn embeddings_by_ids(
        &self,
        namespace: &str,
        ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Vec<f32>)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(Uuid, pgvector::Vector)> = sqlx::query_as(
            "select id, embedding from memories \
             where namespace = $1 and id = any($2) and embedding is not null",
        )
        .bind(namespace)
        .bind(ids)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        Ok(rows.into_iter().map(|(id, v)| (id, v.to_vec())).collect())
    }

    async fn all_in_namespace(
        &self,
        namespace: &str,
        include_merged: bool,
    ) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "select {} from memories where namespace = $1 {} order by created_at, id",
            Self::ENTRY_COLUMNS_NO_EMBEDDING,
            if include_merged {
                ""
            } else {
                "and status <> 'merged'"
            }
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .fetch_all(&self.pool)
            .await
            .pg()?;
        rows.iter().map(Self::entry_from_row_slim).collect()
    }

    /// 有上限的命名空间条目扫描：SQL 层 `ORDER BY updated_at DESC LIMIT`，
    /// 避免传输超大结果集；最近更新优先（认知查重的比对语义，见 trait 文档）
    async fn all_in_namespace_limited(
        &self,
        namespace: &str,
        include_merged: bool,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let sql = format!(
            "select {} from memories where namespace = $1 {} order by updated_at desc, id desc limit {}",
            Self::ENTRY_COLUMNS_NO_EMBEDDING,
            if include_merged {
                ""
            } else {
                "and status <> 'merged'"
            },
            limit as i64,
        );
        let rows = sqlx::query(&sql)
            .bind(namespace)
            .fetch_all(&self.pool)
            .await
            .pg()?;
        rows.iter().map(Self::entry_from_row_slim).collect()
    }

    async fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        let rows: Vec<String> =
            sqlx::query_scalar("select distinct namespace from memories order by namespace")
                .fetch_all(&self.pool)
                .await
                .pg()?;
        Ok(rows)
    }

    async fn record_version(&self, v: &MemoryVersion) -> Result<()> {
        let mut conn = self.pool.acquire().await.pg()?;
        Self::insert_version_on(&mut conn, v).await
    }

    async fn versions(&self, id: Uuid) -> Result<Vec<MemoryVersion>> {
        let rows = sqlx::query(
            r#"
            select memory_id, version, content, tags, metadata, confidence, changed_by, change_type, changed_at,
                   scene, layer, valid_from, valid_until, occurred_at
            from memory_versions where memory_id = $1 order by version
            "#,
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .pg()?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let change: String = r.try_get("change_type").pg()?;
            let scene_str: Option<String> = r.try_get("scene").pg()?;
            let layer_str: Option<String> = r.try_get("layer").pg()?;
            let scene = match scene_str.as_deref() {
                Some(s) => Some(mapping::parse_scene(s)?),
                None => None,
            };
            let layer = match layer_str.as_deref() {
                Some(s) => Some(mapping::parse_layer(s)?),
                None => None,
            };
            let valid_from: Option<DateTime<Utc>> = r.try_get("valid_from").pg()?;
            let valid_until: Option<DateTime<Utc>> = r.try_get("valid_until").pg()?;
            let occurred_at: Option<DateTime<Utc>> = r.try_get("occurred_at").pg()?;
            out.push(MemoryVersion {
                memory_id: r.try_get("memory_id").pg()?,
                version: r.try_get("version").pg()?,
                content: r.try_get("content").pg()?,
                tags: r.try_get("tags").pg()?,
                metadata: r.try_get("metadata").pg()?,
                confidence: r.try_get("confidence").pg()?,
                changed_by: r.try_get("changed_by").pg()?,
                change_type: mapping::parse_version_change(&change)?,
                changed_at: r.try_get("changed_at").pg()?,
                scene,
                layer,
                valid_from,
                valid_until,
                occurred_at,
            });
        }
        Ok(out)
    }

    async fn archive_expired(&self, now: DateTime<Utc>) -> Result<usize> {
        // 单条 CTE：归档 + 版本历史一次完成
        let res = sqlx::query(
            r#"
            with expired as (
              update memories set status = 'archived', version = version + 1, updated_at = $1
              where status = 'active' and expires_at is not null and expires_at <= $1
              returning id, version, content, tags, metadata, confidence, scene, layer, valid_from, valid_until, occurred_at
            )
            insert into memory_versions
              (memory_id, version, content, tags, metadata, confidence, changed_by, change_type, changed_at,
               scene, layer, valid_from, valid_until, occurred_at)
            select id, version, content, tags, metadata, confidence, 'system', 'archived', $1, scene, layer, valid_from, valid_until, occurred_at
            from expired
            on conflict (memory_id, version) do nothing
            "#,
        )
        .bind(now)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(res.rows_affected() as usize)
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("select 1").execute(&self.pool).await.pg()?;
        Ok(())
    }
}
