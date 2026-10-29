use super::*;

#[async_trait]
impl mem_core::IntegrationStore for PostgresHubStore {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        sqlx::query(
            r#"
            insert into integration_proposals
              (id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            on conflict (id) do update set
              status = excluded.status, resolution_note = excluded.resolution_note,
              resolved_at = excluded.resolved_at, suggested_survivor = excluded.suggested_survivor
            "#,
        )
        .bind(p.id)
        .bind(&p.namespace)
        .bind(p.kind.as_str())
        .bind(&p.member_ids)
        .bind(p.suggested_survivor)
        .bind(p.similarity)
        .bind(match p.status {
            ProposalStatus::Open => "open",
            ProposalStatus::Merged => "merged",
            ProposalStatus::Dismissed => "dismissed",
            ProposalStatus::Expired => "expired",
        })
        .bind(&p.resolution_note)
        .bind(p.created_at)
        .bind(p.resolved_at)
        .bind(&p.metadata)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        let row = sqlx::query(
            r#"
            select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
            from integration_proposals where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?
        .ok_or(HubError::NotFound)?;
        Self::proposal_from_row(&row)
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let rows = match status {
            Some(s) => {
                sqlx::query(
                    r#"
                    select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
                    from integration_proposals where namespace = $1 and status = $2
                    order by created_at desc
                    "#,
                )
                .bind(namespace)
                .bind(match s {
                    ProposalStatus::Open => "open",
                    ProposalStatus::Merged => "merged",
                    ProposalStatus::Dismissed => "dismissed",
                    ProposalStatus::Expired => "expired",
                })
                .fetch_all(&self.pool)
                .await
                .pg()?
            }
            None => {
                sqlx::query(
                    r#"
                    select id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata
                    from integration_proposals where namespace = $1
                    order by created_at desc
                    "#,
                )
                .bind(namespace)
                .fetch_all(&self.pool)
                .await
                .pg()?
            }
        };
        rows.iter().map(Self::proposal_from_row).collect()
    }

    /// 原子创建 open 提案：部分唯一索引 + `on conflict do nothing`。
    ///
    /// 同一 `(namespace, kind, member_ids)` 的 open 提案只允许一条；并发插入时
    /// 后到者 0 行受影响，返回 AlreadyExists 而不是报错或产生重复行。
    async fn create_open_proposal_once(
        &self,
        p: &IntegrationProposal,
    ) -> Result<mem_core::CreateProposalOutcome> {
        let res = sqlx::query(
            r#"
            insert into integration_proposals
              (id, namespace, kind, member_ids, suggested_survivor, similarity, status, resolution_note, created_at, resolved_at, metadata)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            on conflict (namespace, kind, member_ids) where status = 'open' do nothing
            "#,
        )
        .bind(p.id)
        .bind(&p.namespace)
        .bind(p.kind.as_str())
        .bind(&p.member_ids)
        .bind(p.suggested_survivor)
        .bind(p.similarity)
        .bind(match p.status {
            ProposalStatus::Open => "open",
            ProposalStatus::Merged => "merged",
            ProposalStatus::Dismissed => "dismissed",
            ProposalStatus::Expired => "expired",
        })
        .bind(&p.resolution_note)
        .bind(p.created_at)
        .bind(p.resolved_at)
        .bind(&p.metadata)
        .execute(&self.pool)
        .await
        .pg()?;

        if res.rows_affected() == 0 {
            Ok(mem_core::CreateProposalOutcome::AlreadyExists)
        } else {
            Ok(mem_core::CreateProposalOutcome::Created)
        }
    }

    async fn open_exists(&self, namespace: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        for (kind, ids) in members {
            let count: i64 = sqlx::query_scalar(
                r#"
                select count(*) from integration_proposals
                where namespace = $1 and status = 'open' and kind = $2
                  and cardinality(member_ids) = cardinality($3)
                  and member_ids @> $3
                "#,
            )
            .bind(namespace)
            .bind(kind)
            .bind(ids)
            .fetch_one(&self.pool)
            .await
            .pg()?;
            if count > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[async_trait]
impl mem_core::CognitiveProposalStore for PostgresHubStore {
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
        let candidate = serde_json::to_value(&p.candidate)
            .map_err(|e| HubError::Internal(anyhow::anyhow!("认知提案载荷序列化失败: {}", e)))?;
        sqlx::query(
            r#"
            insert into cognitive_proposals
              (id, namespace, candidate, source_evidence_id, status, rejection_reason,
               resulting_memory_id, resolved_by, created_at, updated_at)
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(p.id)
        .bind(&p.namespace)
        .bind(&candidate)
        .bind(p.source_evidence_id)
        .bind(p.status.as_str())
        .bind(&p.rejection_reason)
        .bind(p.resulting_memory_id)
        .bind(&p.resolved_by)
        .bind(p.created_at)
        .bind(p.updated_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
        let sql = format!(
            "select {} from cognitive_proposals where id = $1",
            Self::COGNITIVE_PROPOSAL_COLUMNS
        );
        let row = sqlx::query(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .pg()?
            .ok_or(HubError::NotFound)?;
        Self::cognitive_proposal_from_row(&row)
    }

    async fn list_cognitive_proposals(
        &self,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        let sql = match status {
            Some(_) => format!(
                "select {} from cognitive_proposals where namespace = $1 and status = $2 \
                 order by created_at desc",
                Self::COGNITIVE_PROPOSAL_COLUMNS
            ),
            None => format!(
                "select {} from cognitive_proposals where namespace = $1 \
                 order by created_at desc",
                Self::COGNITIVE_PROPOSAL_COLUMNS
            ),
        };
        let mut q = sqlx::query(&sql).bind(namespace);
        if let Some(s) = status {
            q = q.bind(s.as_str());
        }
        let rows = q.fetch_all(&self.pool).await.pg()?;
        rows.iter().map(Self::cognitive_proposal_from_row).collect()
    }

    async fn update_cognitive_proposal_status(
        &self,
        id: Uuid,
        expected_status: CognitiveProposalStatus,
        new_status: CognitiveProposalStatus,
        rejection_reason: Option<&str>,
        resulting_memory_id: Option<Uuid>,
        resolved_by: Option<&str>,
    ) -> Result<CognitiveProposalRecord> {
        // 先原子推进状态（乐观前置），再回读整行
        let result = sqlx::query(
            r#"
            update cognitive_proposals
            set status = $2, rejection_reason = $3, resulting_memory_id = $4,
                resolved_by = $5, updated_at = now()
            where id = $1 and status = $6
            "#,
        )
        .bind(id)
        .bind(new_status.as_str())
        .bind(rejection_reason)
        .bind(resulting_memory_id)
        .bind(resolved_by)
        .bind(expected_status.as_str())
        .execute(&self.pool)
        .await
        .pg()?;
        if result.rows_affected() == 0 {
            // 区分两种失败：提案不存在（NotFound）vs 已被别人裁决（Conflict）
            let exists: Option<Uuid> =
                sqlx::query_scalar("select id from cognitive_proposals where id = $1")
                    .bind(id)
                    .fetch_optional(&self.pool)
                    .await
                    .pg()?;
            return match exists {
                Some(_) => Err(HubError::Conflict),
                None => Err(HubError::NotFound),
            };
        }
        self.get_cognitive_proposal(id).await
    }
}
