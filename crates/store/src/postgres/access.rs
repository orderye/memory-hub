use super::*;

#[async_trait]
impl mem_core::GrantStore for PostgresHubStore {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        // owner 必须先注册为 agent（外键约束）；未知 Agent 归入引导账户兜底
        sqlx::query(
            r#"
            insert into agents (agent_id, name, owner_account_id) values ($1, $1, $2)
            on conflict (agent_id) do nothing
            "#,
        )
        .bind(owner)
        .bind(common::BOOTSTRAP_ACCOUNT_ID)
        .execute(&self.pool)
        .await
        .pg()?;

        let existing: Option<String> =
            sqlx::query_scalar("select namespace from namespaces where namespace = $1")
                .bind(namespace)
                .fetch_optional(&self.pool)
                .await
                .pg()?;

        let created = existing.is_none();
        if created {
            sqlx::query("insert into namespaces (namespace, owner_agent) values ($1, $2)")
                .bind(namespace)
                .bind(owner)
                .execute(&self.pool)
                .await
                .pg()?;
        }

        let info = NamespaceInfo {
            namespace: namespace.to_string(),
            owner_agent: owner.clone(),
            created_at: Utc::now(),
        };
        Ok((info, created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        let row = sqlx::query(
            "select namespace, owner_agent, created_at from namespaces where namespace = $1",
        )
        .bind(namespace)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| NamespaceInfo {
            namespace: r.get("namespace"),
            owner_agent: r.get("owner_agent"),
            created_at: r.get("created_at"),
        }))
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        let rows = sqlx::query(
            "select namespace, owner_agent, created_at from namespaces order by namespace",
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows
            .iter()
            .map(|r| NamespaceInfo {
                namespace: r.get("namespace"),
                owner_agent: r.get("owner_agent"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        sqlx::query(
            r#"
            insert into namespace_grants (namespace, agent_id, can_read, can_write, granted_by, created_at)
            values ($1, $2, $3, $4, $5, $6)
            on conflict (namespace, agent_id) do update set
              can_read = excluded.can_read, can_write = excluded.can_write,
              granted_by = excluded.granted_by, created_at = excluded.created_at
            "#,
        )
        .bind(&record.namespace)
        .bind(&record.agent_id)
        .bind(record.can_read)
        .bind(record.can_write)
        .bind(&record.granted_by)
        .bind(record.created_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        sqlx::query("delete from namespace_grants where namespace = $1 and agent_id = $2")
            .bind(namespace)
            .bind(agent_id)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        let rows = sqlx::query(
            r#"
            select namespace, agent_id, can_read, can_write, granted_by, created_at
            from namespace_grants where agent_id = $1 order by namespace
            "#,
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows
            .iter()
            .map(|r| NamespaceGrantRecord {
                namespace: r.get("namespace"),
                agent_id: r.get("agent_id"),
                can_read: r.get("can_read"),
                can_write: r.get("can_write"),
                granted_by: r.get("granted_by"),
                created_at: r.get("created_at"),
            })
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        // 沙箱语义：授权行必须精确匹配命名空间；`*` 通配授权已废除
        let count: i64 = sqlx::query_scalar(
            r#"
            select count(*) from namespace_grants
            where agent_id = $1 and namespace = $2 and can_read
            "#,
        )
        .bind(agent_id)
        .bind(namespace)
        .fetch_one(&self.pool)
        .await
        .pg()?;
        Ok(count > 0)
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        let count: i64 = sqlx::query_scalar(
            r#"
            select count(*) from namespace_grants
            where agent_id = $1 and namespace = $2 and can_write
            "#,
        )
        .bind(agent_id)
        .bind(namespace)
        .fetch_one(&self.pool)
        .await
        .pg()?;
        Ok(count > 0)
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        let rows: Vec<String> = sqlx::query_scalar(
            "select namespace from namespaces where owner_agent = $1 order by namespace",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows)
    }
}

#[async_trait]
impl mem_core::AgentStore for PostgresHubStore {
    async fn register_agent(
        &self,
        reg: &mem_core::AgentRegistration,
    ) -> Result<mem_core::AgentRecord> {
        // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker
        let secret_hash: Option<String> = match &reg.client_secret {
            Some(s) => {
                Some(crate::secret::hash_secret_blocking(reg.agent_id.clone(), s.clone()).await)
            }
            None => None,
        };
        let max_scopes: Vec<String> = reg
            .max_scopes
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();

        sqlx::query(
            r#"
            insert into agents (agent_id, name, kind, status, owner_account_id, client_secret_hash, redirect_uris, max_scopes, created_at)
            values ($1, $2, $3, 'active', $4, $5, $6, $7, now())
            on conflict (agent_id) do update set
              name = excluded.name,
              kind = excluded.kind,
              owner_account_id = excluded.owner_account_id,
              redirect_uris = excluded.redirect_uris,
              max_scopes = excluded.max_scopes,
              client_secret_hash = coalesce(excluded.client_secret_hash, agents.client_secret_hash)
            "#,
        )
        .bind(&reg.agent_id)
        .bind(&reg.name)
        .bind(&reg.kind)
        .bind(reg.owner_account_id)
        .bind(&secret_hash)
        .bind(&reg.redirect_uris)
        .bind(&max_scopes)
        .execute(&self.pool)
        .await
        .pg()?;

        self.get_agent(&reg.agent_id)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("agent 注册后读取失败")))
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<mem_core::AgentRecord>> {
        let row = sqlx::query(
            r#"
            select agent_id, name, kind, status, owner_account_id, redirect_uris, max_scopes,
                   (client_secret_hash is not null) as confidential, created_at
            from agents where agent_id = $1
            "#,
        )
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;

        match row {
            Some(r) => Ok(Some(Self::agent_from_row(&r)?)),
            None => Ok(None),
        }
    }

    async fn list_agents(&self) -> Result<Vec<mem_core::AgentRecord>> {
        let rows = sqlx::query(
            r#"
            select agent_id, name, kind, status, owner_account_id, redirect_uris, max_scopes,
                   (client_secret_hash is not null) as confidential, created_at
            from agents order by created_at desc
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(Self::agent_from_row(r)?);
        }
        Ok(out)
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        let res = sqlx::query("update agents set status = $2 where agent_id = $1")
            .bind(agent_id)
            .bind(status)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        let row = sqlx::query("select client_secret_hash from agents where agent_id = $1")
            .bind(agent_id)
            .fetch_optional(&self.pool)
            .await
            .pg()?;
        let stored = match row {
            Some(r) => {
                let h: Option<String> = r.try_get("client_secret_hash").pg()?;
                h
            }
            None => None,
        };
        let stored = match stored {
            Some(h) => h,
            None => return Ok(false),
        };
        // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker
        let ok = crate::secret::verify_secret_blocking(
            agent_id.to_string(),
            secret.to_string(),
            stored.clone(),
        )
        .await;
        // 存量旧摘要（无盐 sha256）在首次校验成功后就地升级为 Argon2id
        if ok && crate::secret::is_legacy_digest(&stored) {
            let upgraded =
                crate::secret::hash_secret_blocking(agent_id.to_string(), secret.to_string()).await;
            sqlx::query("update agents set client_secret_hash = $2 where agent_id = $1")
                .bind(agent_id)
                .bind(&upgraded)
                .execute(&self.pool)
                .await
                .pg()?;
        }
        Ok(ok)
    }
}

#[async_trait]
impl mem_core::AccountStore for PostgresHubStore {
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let exists: Option<Uuid> =
            sqlx::query_scalar("select id from accounts where lower(username) = $1")
                .bind(&normalized)
                .fetch_optional(&self.pool)
                .await
                .pg()?;
        if exists.is_some() {
            return Err(HubError::InvalidArgument(format!(
                "用户名 {} 已被占用",
                normalized
            )));
        }
        let id = Uuid::new_v4();
        sqlx::query(
            r#"
            insert into accounts (id, username, display_name, status, password_hash, created_at)
            values ($1, $2, $3, 'active', $4, now())
            "#,
        )
        .bind(id)
        .bind(&normalized)
        .bind(display_name)
        .bind(password_hash)
        .execute(&self.pool)
        .await
        .pg()?;
        self.get_account(&id)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("账户创建后读取失败")))
    }

    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        sqlx::query(
            r#"
            insert into accounts (id, username, display_name, status, created_at)
            values ($1, $2, $3, 'active', now())
            on conflict (id) do nothing
            "#,
        )
        .bind(id)
        .bind(&normalized)
        .bind(display_name)
        .execute(&self.pool)
        .await
        .pg()?;
        // 用户名冲突兜底：若该用户名已归属其他账户，按用户名回读
        if let Some(record) = self.get_account(id).await? {
            return Ok(record);
        }
        self.get_account_by_username(&normalized)
            .await?
            .ok_or_else(|| HubError::Internal(anyhow::anyhow!("引导账户确保失败")))
    }

    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>> {
        let row = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts where id = $1
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| account_record_of(&r)))
    }

    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>> {
        let normalized = username.trim().to_ascii_lowercase();
        let row = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts where lower(username) = $1
            "#,
        )
        .bind(&normalized)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| account_record_of(&r)))
    }

    async fn password_hash_of(&self, username: &str) -> Result<Option<String>> {
        let normalized = username.trim().to_ascii_lowercase();
        let hash: Option<String> =
            sqlx::query_scalar("select password_hash from accounts where lower(username) = $1")
                .bind(&normalized)
                .fetch_optional(&self.pool)
                .await
                .pg()?
                .flatten();
        Ok(hash)
    }

    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()> {
        let res = sqlx::query("update accounts set password_hash = $2 where id = $1")
            .bind(id)
            .bind(password_hash)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()> {
        let res = sqlx::query("update accounts set status = $2 where id = $1")
            .bind(id)
            .bind(status)
            .execute(&self.pool)
            .await
            .pg()?;
        if res.rows_affected() == 0 {
            return Err(HubError::NotFound);
        }
        Ok(())
    }

    async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let rows = sqlx::query(
            r#"
            select id, username, display_name, status,
                   (password_hash is not null) as has_password, created_at
            from accounts order by created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .pg()?;
        Ok(rows.iter().map(account_record_of).collect())
    }

    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            insert into account_sessions (token_hash, account_id, expires_at, created_at)
            values ($1, $2, $3, now())
            on conflict (token_hash) do update set
              account_id = excluded.account_id, expires_at = excluded.expires_at
            "#,
        )
        .bind(token_hash)
        .bind(account_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .pg()?;
        Ok(())
    }

    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>> {
        let row = sqlx::query(
            "select account_id, expires_at from account_sessions where token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .pg()?;
        Ok(row.map(|r| SessionRecord {
            account_id: r.get("account_id"),
            expires_at: r.get("expires_at"),
        }))
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        sqlx::query("delete from account_sessions where token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(())
    }

    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize> {
        let res = sqlx::query("delete from account_sessions where expires_at <= $1")
            .bind(now)
            .execute(&self.pool)
            .await
            .pg()?;
        Ok(res.rows_affected() as usize)
    }
}

fn account_record_of(r: &PgRow) -> AccountRecord {
    AccountRecord {
        id: r.get("id"),
        username: r.get("username"),
        display_name: r.get("display_name"),
        status: r.get("status"),
        has_password: r.get("has_password"),
        created_at: r.get("created_at"),
    }
}
