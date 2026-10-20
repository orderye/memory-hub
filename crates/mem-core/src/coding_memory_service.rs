//! Coding 代码演进链服务 (P4)
//!
//! 核心原则：
//! 1. 真实代码变更捕获：保存真实的 Git commit / diff / 工具变更，绝不用模型生成或重写 diff；
//! 2. 确定性哈希与幂等重放：基于 SHA-256 规范计算 diff_hash，同一 diff 重复提交直接幂等返回已存记录；
//! 3. 严格安全过滤门槛：拒绝敏感 token、私钥、`.env` 文件、路径穿越、二进制与超限 (>512KB) diff；
//! 4. 逻辑线程与文件快照解耦：按逻辑演进主题（Thread）与文件路径演进链（Revision）双向检索；
//! 5. 跨命名空间与账户隔离：fail-closed 强沙箱隔离，跨账户同名空间互不可见。

use crate::account_scope::AccountScopedStore;
use crate::domain::{
    validate_coding_revision, CodingRevision, CodingThread, NewCodingRevision, NewCodingThread,
};
use crate::infra::MemoryHubStore;
use common::{AccountId, AuthCtx, HubError, Result, Scope};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

/// Coding 代码演进记忆服务
pub struct CodingMemoryService {
    store: Arc<dyn MemoryHubStore>,
}

impl CodingMemoryService {
    pub fn new(store: Arc<dyn MemoryHubStore>) -> Self {
        Self { store }
    }

    /// 切换底层作用域存储（账户沙箱化）
    pub fn scoped(&self, account_id: AccountId) -> Self {
        Self {
            store: Arc::new(AccountScopedStore::new(self.store.clone(), account_id)),
        }
    }

    /// 创建代码演进逻辑线程
    pub async fn start_thread(
        &self,
        auth: &AuthCtx,
        thread: NewCodingThread,
    ) -> Result<CodingThread> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }
        let repo = thread.repository.trim();
        if repo.is_empty() {
            return Err(HubError::InvalidArgument("仓库标识不能为空".into()));
        }
        if repo.contains("..") || repo.starts_with('/') {
            return Err(HubError::InvalidArgument("仓库标识存在非法路径字符".into()));
        }
        let title = thread.title.trim();
        if title.is_empty() {
            return Err(HubError::InvalidArgument("演进主题标题不能为空".into()));
        }

        self.store.create_thread(&thread).await
    }

    /// 获取代码演进线程
    pub async fn get_thread(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
    ) -> Result<Option<CodingThread>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store.get_thread(namespace, id).await
    }

    /// 列表代码演进线程
    pub async fn list_threads(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        repository: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CodingThread>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store
            .list_threads(namespace, repository, limit.max(1))
            .await
    }

    /// 记录代码修订记录（严格安全门槛 + 确定性哈希 + 幂等重放）
    pub async fn record_revision(
        &self,
        auth: &AuthCtx,
        revision: NewCodingRevision,
    ) -> Result<CodingRevision> {
        if !auth.has_scope(&Scope::Write) {
            return Err(HubError::InsufficientScope);
        }

        // 1. 安全过滤门槛校验
        validate_coding_revision(&revision)?;

        // 2. 校验所属代码线程存在性（若指定）
        if let Some(tid) = revision.thread_id {
            if self
                .store
                .get_thread(&revision.namespace, tid)
                .await?
                .is_none()
            {
                return Err(HubError::NotFound);
            }
        }

        // 3. 计算确定性 diff SHA-256 哈希
        let mut hasher = Sha256::new();
        hasher.update(revision.diff_content.as_bytes());
        let diff_hash = hex::encode(hasher.finalize());

        // 4. 幂等查重检查：同一 namespace + repository + diff_hash 保持幂等
        if let Some(existing) = self
            .store
            .get_revision_by_hash(&revision.namespace, &revision.repository, &diff_hash)
            .await?
        {
            return Ok(existing);
        }

        // 5. 持久化存储
        self.store.save_revision(&revision, &diff_hash).await
    }

    /// 按 ID 读取代码修订快照
    pub async fn get_revision(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        id: Uuid,
    ) -> Result<Option<CodingRevision>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store.get_revision(namespace, id).await
    }

    /// 列表某代码线程下的全部修订快照
    pub async fn list_thread_revisions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        thread_id: Uuid,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store
            .list_revisions_for_thread(namespace, thread_id, limit.max(1))
            .await
    }

    /// 列表某文件路径下的修订历史演进链
    pub async fn list_file_revisions(
        &self,
        auth: &AuthCtx,
        namespace: &str,
        repository: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CodingRevision>> {
        if !auth.has_scope(&Scope::Read) {
            return Err(HubError::InsufficientScope);
        }
        self.store
            .list_revisions_for_file(namespace, repository, file_path, limit.max(1))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::CodeChangeKind;
    use crate::testing::TestHubStore;
    use chrono::Utc;
    use common::BOOTSTRAP_ACCOUNT_ID;

    fn test_auth(agent: &str) -> AuthCtx {
        AuthCtx {
            agent_id: agent.to_string(),
            account_id: BOOTSTRAP_ACCOUNT_ID,
            scopes: vec![Scope::Read, Scope::Write, Scope::Admin],
            acl: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_record_revision_success_and_idempotent_replay() {
        let store = Arc::new(TestHubStore::default());
        let svc = CodingMemoryService::new(store);
        let auth = test_auth("coding-agent");

        let thread = svc
            .start_thread(
                &auth,
                NewCodingThread {
                    id: None,
                    namespace: "project-alpha".into(),
                    repository: "core-repo".into(),
                    branch: Some("main".into()),
                    task_id: None,
                    title: "优化重试机制".into(),
                    description: Some("添加指数退避".into()),
                },
            )
            .await
            .unwrap();

        let rev_req = NewCodingRevision {
            id: None,
            thread_id: Some(thread.id),
            namespace: "project-alpha".into(),
            repository: "core-repo".into(),
            branch: Some("main".into()),
            commit_sha: Some("a1b2c3d4e5f6".into()),
            parent_sha: Some("000000000000".into()),
            file_path: "src/retry.rs".into(),
            change_kind: CodeChangeKind::Modify,
            diff_content:
                "@@ -1,3 +1,5 @@\n+use std::time::Duration;\n+const MAX_RETRIES: u32 = 3;\n".into(),
            symbol_fingerprint: Some("retry::backoff".into()),
            is_committed: true,
            author: Some("alice".into()),
            occurred_at: Some(Utc::now()),
            source_evidence_id: None,
            source_task_id: None,
        };

        let rev1 = svc.record_revision(&auth, rev_req.clone()).await.unwrap();
        assert_eq!(rev1.thread_id, Some(thread.id));
        assert_eq!(rev1.repository, "core-repo");
        assert_eq!(rev1.file_path, "src/retry.rs");

        // 幂等重放同一变更
        let rev2 = svc.record_revision(&auth, rev_req).await.unwrap();
        assert_eq!(rev1.id, rev2.id, "重复记录相同变更必须幂等返回已存记录");

        // 检索历史
        let list = svc
            .list_file_revisions(&auth, "project-alpha", "core-repo", "src/retry.rs", 10)
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, rev1.id);
    }

    #[tokio::test]
    async fn test_sensitive_credentials_rejection() {
        let store = Arc::new(TestHubStore::default());
        let svc = CodingMemoryService::new(store);
        let auth = test_auth("coding-agent");

        // 分段拼接的厂商形态见 crates/mem-core/src/redaction.rs 的同款说明：
        // 公开仓库里连续的凭证字面量会被 secret scanning 当成真实泄露上报。
        let bad_diffs = vec![
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
            concat!("sk-proj-", "1234567890abcdef1234567890abcdef12345678"),
            concat!("ghp_", "1234567890abcdefghijklmnopqrstuvwxyz"),
            "bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "-----BEGIN RSA PRIVATE KEY-----",
        ];

        for bad in bad_diffs {
            let res = svc
                .record_revision(
                    &auth,
                    NewCodingRevision {
                        id: None,
                        thread_id: None,
                        namespace: "safe-ns".into(),
                        repository: "safe-repo".into(),
                        branch: None,
                        commit_sha: None,
                        parent_sha: None,
                        file_path: "src/auth.rs".into(),
                        change_kind: CodeChangeKind::Modify,
                        diff_content: format!("+ let key = \"{}\";", bad),
                        symbol_fingerprint: None,
                        is_committed: false,
                        author: None,
                        occurred_at: None,
                        source_evidence_id: None,
                        source_task_id: None,
                    },
                )
                .await;
            assert!(
                res.is_err(),
                "包含敏感信息 [{}] 的 diff 必须被拒绝入库",
                bad
            );
        }
    }

    #[tokio::test]
    async fn test_security_rejections() {
        let store = Arc::new(TestHubStore::default());
        let svc = CodingMemoryService::new(store);
        let auth = test_auth("coding-agent");

        // 1. .env 文件
        let res = svc
            .record_revision(
                &auth,
                NewCodingRevision {
                    id: None,
                    thread_id: None,
                    namespace: "safe-ns".into(),
                    repository: "safe-repo".into(),
                    branch: None,
                    commit_sha: None,
                    parent_sha: None,
                    file_path: ".env.production".into(),
                    change_kind: CodeChangeKind::Add,
                    diff_content: "+FOO=bar".into(),
                    symbol_fingerprint: None,
                    is_committed: false,
                    author: None,
                    occurred_at: None,
                    source_evidence_id: None,
                    source_task_id: None,
                },
            )
            .await;
        assert!(res.is_err(), ".env 文件修改必须拒绝");

        // 2. 路径穿越
        let res = svc
            .record_revision(
                &auth,
                NewCodingRevision {
                    id: None,
                    thread_id: None,
                    namespace: "safe-ns".into(),
                    repository: "safe-repo".into(),
                    branch: None,
                    commit_sha: None,
                    parent_sha: None,
                    file_path: "../../../etc/passwd".into(),
                    change_kind: CodeChangeKind::Modify,
                    diff_content: "+root".into(),
                    symbol_fingerprint: None,
                    is_committed: false,
                    author: None,
                    occurred_at: None,
                    source_evidence_id: None,
                    source_task_id: None,
                },
            )
            .await;
        assert!(res.is_err(), "路径穿越必须拒绝");

        // 3. 包含二进制空字符
        let res = svc
            .record_revision(
                &auth,
                NewCodingRevision {
                    id: None,
                    thread_id: None,
                    namespace: "safe-ns".into(),
                    repository: "safe-repo".into(),
                    branch: None,
                    commit_sha: None,
                    parent_sha: None,
                    file_path: "bin/binary.dat".into(),
                    change_kind: CodeChangeKind::Add,
                    diff_content: "foo\0bar".into(),
                    symbol_fingerprint: None,
                    is_committed: false,
                    author: None,
                    occurred_at: None,
                    source_evidence_id: None,
                    source_task_id: None,
                },
            )
            .await;
        assert!(res.is_err(), "二进制文件必须拒绝");

        // 4. 超大 diff (>512KB)
        let huge_diff = "a".repeat(513 * 1024);
        let res = svc
            .record_revision(
                &auth,
                NewCodingRevision {
                    id: None,
                    thread_id: None,
                    namespace: "safe-ns".into(),
                    repository: "safe-repo".into(),
                    branch: None,
                    commit_sha: None,
                    parent_sha: None,
                    file_path: "src/huge.rs".into(),
                    change_kind: CodeChangeKind::Modify,
                    diff_content: huge_diff,
                    symbol_fingerprint: None,
                    is_committed: false,
                    author: None,
                    occurred_at: None,
                    source_evidence_id: None,
                    source_task_id: None,
                },
            )
            .await;
        assert!(res.is_err(), "超限 diff 必须拒绝");
    }

    #[tokio::test]
    async fn test_cross_account_isolation() {
        let raw_store = Arc::new(TestHubStore::default());
        let svc_root = CodingMemoryService::new(raw_store);

        let acc_a = AccountId::new_v4();
        let acc_b = AccountId::new_v4();

        let svc_a = svc_root.scoped(acc_a);
        let svc_b = svc_root.scoped(acc_b);

        let mut auth_a = test_auth("agent-a");
        auth_a.account_id = acc_a;

        let mut auth_b = test_auth("agent-b");
        auth_b.account_id = acc_b;

        // Account A 创建修订
        let rev_a = svc_a
            .record_revision(
                &auth_a,
                NewCodingRevision {
                    id: None,
                    thread_id: None,
                    namespace: "common-ns".into(),
                    repository: "shared-repo".into(),
                    branch: None,
                    commit_sha: Some("commit-a".into()),
                    parent_sha: None,
                    file_path: "src/main.rs".into(),
                    change_kind: CodeChangeKind::Modify,
                    diff_content: "+// change in account A".into(),
                    symbol_fingerprint: None,
                    is_committed: true,
                    author: Some("alice".into()),
                    occurred_at: None,
                    source_evidence_id: None,
                    source_task_id: None,
                },
            )
            .await
            .unwrap();

        // Account B 查询同一 namespace 和同一 repo，必须读不到 Account A 的变更
        let list_b = svc_b
            .list_file_revisions(&auth_b, "common-ns", "shared-repo", "src/main.rs", 10)
            .await
            .unwrap();
        assert!(
            list_b.is_empty(),
            "账户 B 绝不能看到账户 A 的同名 namespace 代码演进记录"
        );

        let get_b = svc_b
            .get_revision(&auth_b, "common-ns", rev_a.id)
            .await
            .unwrap();
        assert!(
            get_b.is_none(),
            "账户 B 按 ID 查询账户 A 的修订必须返回 None"
        );
    }
}
