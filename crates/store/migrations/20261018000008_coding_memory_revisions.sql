-- 迁移 000008: Coding 代码演进链与修订记录
--
-- 核心目标：
-- 1. 将真实的 Git commit / diff / 工具变更作为 Working 记忆的附加演进链；
-- 2. 区分逻辑主题线程（coding_threads）与具体代码修订快照（coding_revisions）；
-- 3. 支持 branch、commit_sha、parent_sha、symbol_fingerprint 精准定位；
-- 4. 级联外键与命名空间强关联，保证沙箱隔离与清理完整性。

CREATE TABLE IF NOT EXISTS coding_threads (
    id UUID PRIMARY KEY,
    namespace VARCHAR(128) NOT NULL REFERENCES namespaces(namespace) ON DELETE CASCADE,
    repository VARCHAR(256) NOT NULL,
    branch VARCHAR(128),
    task_id UUID REFERENCES memory_tasks(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    description TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_coding_threads_ns_repo
    ON coding_threads(namespace, repository);

CREATE INDEX IF NOT EXISTS idx_coding_threads_task_id
    ON coding_threads(task_id);

CREATE TABLE IF NOT EXISTS coding_revisions (
    id UUID PRIMARY KEY,
    thread_id UUID REFERENCES coding_threads(id) ON DELETE CASCADE,
    namespace VARCHAR(128) NOT NULL REFERENCES namespaces(namespace) ON DELETE CASCADE,
    repository VARCHAR(256) NOT NULL,
    branch VARCHAR(128),
    commit_sha VARCHAR(64),
    parent_sha VARCHAR(64),
    file_path TEXT NOT NULL,
    change_kind VARCHAR(32) NOT NULL DEFAULT 'modify',
    diff_content TEXT NOT NULL,
    diff_hash VARCHAR(64) NOT NULL,
    symbol_fingerprint VARCHAR(128),
    is_committed BOOLEAN NOT NULL DEFAULT true,
    author VARCHAR(128),
    occurred_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    source_evidence_id UUID REFERENCES memory_evidence(id) ON DELETE SET NULL,
    source_task_id UUID REFERENCES memory_tasks(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_ns_repo_file
    ON coding_revisions(namespace, repository, file_path, occurred_at DESC);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_diff_hash
    ON coding_revisions(namespace, diff_hash);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_commit_sha
    ON coding_revisions(namespace, commit_sha);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_thread_id
    ON coding_revisions(thread_id);

CREATE INDEX IF NOT EXISTS idx_coding_revisions_task_id
    ON coding_revisions(source_task_id);
