use super::*;

#[async_trait]
impl mem_core::GrantStore for InMemoryHubStore {
    async fn ensure_namespace(
        &self,
        namespace: &str,
        owner: &AgentId,
    ) -> Result<(NamespaceInfo, bool)> {
        let mut w = self.namespaces.write().unwrap();
        let mut created = false;
        let info = w
            .entry(namespace.to_string())
            .or_insert_with(|| {
                created = true;
                NamespaceInfo {
                    namespace: namespace.to_string(),
                    owner_agent: owner.clone(),
                    created_at: Utc::now(),
                }
            })
            .clone();
        Ok((info, created))
    }

    async fn get_namespace(&self, namespace: &str) -> Result<Option<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().get(namespace).cloned())
    }

    async fn list_namespace_infos(&self) -> Result<Vec<NamespaceInfo>> {
        Ok(self.namespaces.read().unwrap().values().cloned().collect())
    }

    async fn grant(&self, record: &NamespaceGrantRecord) -> Result<()> {
        let mut w = self.grants.write().unwrap();
        w.retain(|x| !(x.namespace == record.namespace && x.agent_id == record.agent_id));
        w.push(record.clone());
        Ok(())
    }

    async fn revoke(&self, namespace: &str, agent_id: &AgentId) -> Result<()> {
        self.grants
            .write()
            .unwrap()
            .retain(|x| !(x.namespace == namespace && x.agent_id == *agent_id));
        Ok(())
    }

    async fn grants_for(&self, agent_id: &AgentId) -> Result<Vec<NamespaceGrantRecord>> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .filter(|x| &x.agent_id == agent_id)
            .cloned()
            .collect())
    }

    async fn can_read(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        // 沙箱语义：授权行必须精确匹配命名空间；`*` 通配授权已废除
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_read))
    }

    async fn can_write(&self, namespace: &str, agent_id: &AgentId) -> Result<bool> {
        Ok(self
            .grants
            .read()
            .unwrap()
            .iter()
            .any(|x| x.namespace == namespace && x.agent_id == *agent_id && x.can_write))
    }

    async fn owned_namespaces(&self, agent_id: &AgentId) -> Result<Vec<Namespace>> {
        Ok(self
            .namespaces
            .read()
            .unwrap()
            .values()
            .filter(|n| &n.owner_agent == agent_id)
            .map(|n| n.namespace.clone())
            .collect())
    }
}

#[async_trait]
impl mem_core::AgentStore for InMemoryHubStore {
    async fn register_agent(
        &self,
        reg: &mem_core::AgentRegistration,
    ) -> Result<mem_core::AgentRecord> {
        // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker；
        // 先算散列再拿写锁，避免 RwLockGuard 跨 await（会破坏 future 的 Send）
        let secret_hash = match &reg.client_secret {
            Some(secret) => Some(
                crate::secret::hash_secret_blocking(reg.agent_id.clone(), secret.clone()).await,
            ),
            None => None,
        };
        let mut w = self.agents.write().unwrap();
        let entry = w
            .entry(reg.agent_id.clone())
            .or_insert_with(|| StoredAgent {
                name: reg.name.clone(),
                kind: reg.kind.clone(),
                status: "active".to_string(),
                owner_account_id: Some(reg.owner_account_id),
                secret_hash: None,
                redirect_uris: Vec::new(),
                max_scopes: Vec::new(),
                created_at: Some(Utc::now()),
            });
        // upsert 语义：名称/类型/账户归属/授权域以本次为准；密钥仅在提供时更新
        entry.name = reg.name.clone();
        entry.kind = reg.kind.clone();
        entry.owner_account_id = Some(reg.owner_account_id);
        entry.redirect_uris = reg.redirect_uris.clone();
        entry.max_scopes = reg.max_scopes.clone();
        if let Some(hash) = secret_hash {
            entry.secret_hash = Some(hash);
        }
        Ok(agent_record(&reg.agent_id, entry))
    }

    async fn get_agent(&self, agent_id: &AgentId) -> Result<Option<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .get(agent_id)
            .map(|a| agent_record(agent_id, a)))
    }

    async fn list_agents(&self) -> Result<Vec<mem_core::AgentRecord>> {
        Ok(self
            .agents
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| agent_record(id, a))
            .collect())
    }

    async fn set_agent_status(&self, agent_id: &AgentId, status: &str) -> Result<()> {
        let mut w = self.agents.write().unwrap();
        match w.get_mut(agent_id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn verify_agent_secret(&self, agent_id: &AgentId, secret: &str) -> Result<bool> {
        // 读锁作用域内取出散列副本，锁不跨 await（否则 future 非 Send）
        let stored = {
            let w = self.agents.read().unwrap();
            w.get(agent_id).and_then(|a| a.secret_hash.clone())
        };
        match stored {
            // Argon2 为 CPU 密集计算，经 spawn_blocking 执行避免阻塞 tokio worker
            Some(h) => Ok(crate::secret::verify_secret_blocking(
                agent_id.to_string(),
                secret.to_string(),
                h,
            )
            .await),
            None => Ok(false),
        }
    }
}

/// StoredAgent → AgentRecord
fn agent_record(agent_id: &str, a: &StoredAgent) -> mem_core::AgentRecord {
    mem_core::AgentRecord {
        agent_id: agent_id.to_string(),
        name: a.name.clone(),
        kind: a.kind.clone(),
        status: a.status.clone(),
        owner_account_id: a.owner_account_id.unwrap_or(common::BOOTSTRAP_ACCOUNT_ID),
        redirect_uris: a.redirect_uris.clone(),
        max_scopes: a.max_scopes.clone(),
        confidential: a.secret_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

/// StoredAccount → AccountRecord（不暴露口令散列本体）
fn account_record(id: &AccountId, a: &StoredAccount) -> AccountRecord {
    AccountRecord {
        id: *id,
        username: a.username.clone(),
        display_name: a.display_name.clone(),
        status: a.status.clone(),
        has_password: a.password_hash.is_some(),
        created_at: a.created_at.unwrap_or_else(Utc::now),
    }
}

#[async_trait]
impl mem_core::AccountStore for InMemoryHubStore {
    async fn create_account(
        &self,
        username: &str,
        password_hash: Option<&str>,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        if w.values().any(|a| a.username == normalized) {
            return Err(HubError::InvalidArgument(format!(
                "用户名 {} 已被占用",
                normalized
            )));
        }
        let id = Uuid::new_v4();
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: password_hash.map(str::to_string),
            created_at: Some(Utc::now()),
        };
        w.insert(id, record.clone());
        Ok(account_record(&id, &record))
    }

    async fn ensure_account(
        &self,
        id: &AccountId,
        username: &str,
        display_name: &str,
    ) -> Result<AccountRecord> {
        let normalized = username.trim().to_ascii_lowercase();
        let mut w = self.accounts.write().unwrap();
        // 用户名已被别的账户占用时，退回按 ID 查找（保持幂等）
        let existing = w.get(id).map(|a| (id.to_owned(), a.clone())).or_else(|| {
            w.iter()
                .find(|(_, a)| a.username == normalized)
                .map(|(k, a)| (*k, a.clone()))
        });
        if let Some((found_id, record)) = existing {
            return Ok(account_record(&found_id, &record));
        }
        let record = StoredAccount {
            username: normalized,
            display_name: display_name.to_string(),
            status: "active".to_string(),
            password_hash: None,
            created_at: Some(Utc::now()),
        };
        w.insert(*id, record.clone());
        Ok(account_record(id, &record))
    }

    async fn get_account(&self, id: &AccountId) -> Result<Option<AccountRecord>> {
        Ok(self
            .accounts
            .read()
            .unwrap()
            .get(id)
            .map(|a| account_record(id, a)))
    }

    async fn get_account_by_username(&self, username: &str) -> Result<Option<AccountRecord>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .iter()
            .find(|(_, a)| a.username == normalized)
            .map(|(id, a)| account_record(id, a)))
    }

    async fn password_hash_of(&self, username: &str) -> Result<Option<String>> {
        let normalized = username.trim().to_ascii_lowercase();
        Ok(self
            .accounts
            .read()
            .unwrap()
            .values()
            .find(|a| a.username == normalized)
            .and_then(|a| a.password_hash.clone()))
    }

    async fn set_account_password(&self, id: &AccountId, password_hash: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.password_hash = Some(password_hash.to_string());
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn set_account_status(&self, id: &AccountId, status: &str) -> Result<()> {
        let mut w = self.accounts.write().unwrap();
        match w.get_mut(id) {
            Some(a) => {
                a.status = status.to_string();
                Ok(())
            }
            None => Err(HubError::NotFound),
        }
    }

    async fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let mut out: Vec<AccountRecord> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .map(|(id, a)| account_record(id, a))
            .collect();
        out.sort_by_key(|a| a.created_at);
        Ok(out)
    }

    async fn create_session(
        &self,
        token_hash: &[u8],
        account_id: &AccountId,
        expires_at: DateTime<Utc>,
    ) -> Result<()> {
        self.sessions.write().unwrap().insert(
            token_hash.to_vec(),
            SessionRecord {
                account_id: *account_id,
                expires_at,
            },
        );
        Ok(())
    }

    async fn get_session(&self, token_hash: &[u8]) -> Result<Option<SessionRecord>> {
        Ok(self.sessions.read().unwrap().get(token_hash).cloned())
    }

    async fn delete_session(&self, token_hash: &[u8]) -> Result<()> {
        self.sessions.write().unwrap().remove(token_hash);
        Ok(())
    }

    async fn prune_sessions(&self, now: DateTime<Utc>) -> Result<usize> {
        let mut w = self.sessions.write().unwrap();
        let before = w.len();
        w.retain(|_, s| s.expires_at > now);
        Ok(before - w.len())
    }
}
