# P4 Coding 代码演进链技术规范与安全策略

本文档详细规范 Memory Hub P4 阶段引入的 **Coding 代码演进链（Coding Memory Evolution Chain）** 数据模型、捕获策略、安全过滤门槛、备份恢复完整性及检索关联约定。

---

## 1. 核心目标与设计原则

1. **真实代码演进捕获，严禁模型幻觉生成**：
   - 生产路径中所有代码修订快照（`CodingRevision`）必须源自真实的 Git commit 或实时工作区 patch，严格保留真实 diff、commit SHA、parent SHA 及发生时刻；
   - 严禁让大语言模型重新“生成”或“补全”不存在的代码 diff，确保代码历史的确定性与可溯源性。

2. **多层级解耦：逻辑主题（Thread）与文件修订（Revision）**：
   - **CodingThread**：承载业务层逻辑主题（如“重构重试机制”、“升级认证流程”），可关联任务 ID（`task_id`）；
   - **CodingRevision**：承载具体文件级别的代码变更快照，精确记录 `repository`、`branch`、`commit_sha`、`parent_sha`、`file_path`、`diff_content`、`diff_hash`、`symbol_fingerprint`、`is_committed`。

3. **确定性哈希与严格幂等**：
   - 使用规范 SHA-256 算法对 `diff_content` 计算 `diff_hash`；
   - 同一 `(namespace, repository, diff_hash)` 的重复写入请求执行严格幂等重放，直接返回已存实体，不增加重复版本或污染数据库。

4. **Fail-Closed 跨账户沙箱隔离**：
   - 所有表结构均包含 `namespace`，并在运行时强制通过 `AccountScopedStore` 封装，保证账户 A 与账户 B 即使拥有同名仓库或同名 namespace，数据在物理层与逻辑层均绝对隔离，跨租户越权读取次数恒为 0。

---

## 2. 数据库模式设计与迁移

在 PostgreSQL 迁移 `20261018000008_coding_memory_revisions.sql` 中新增两张核心表：

```sql
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
```

### 索引优化策略
- `idx_coding_threads_ns_repo`: 支持按命名空间与仓库快速筛选主题线程；
- `idx_coding_threads_task_id`: 支持从 Working 记忆任务直接反查关联代码线程；
- `idx_coding_revisions_ns_repo_file`: 支持基于 `(namespace, repository, file_path, occurred_at DESC)` 的高效演进历史链回溯；
- `idx_coding_revisions_diff_hash`: 支撑亚毫秒级 diff 重放去重检索；
- `idx_coding_revisions_thread_id`: 支撑完整主题演进上下文加载；
- `idx_coding_revisions_task_id`: 支撑跨任务演进链关联分析。

---

## 3. 严格安全过滤门槛 (Security Boundary)

所有进入 `CodingMemoryService` 的修订请求在落库前必须通过 `validate_coding_revision` 校验，违规请求立即被阻断（Fail-Fast）：

1. **路径穿越与越权防御**：
   - 拒绝文件路径中包含 `..` 的路径穿越；
   - 拒绝以 `/` 或 `\` 开头的绝对系统路径；
   - 拒绝直接包含操作系统敏感路径（如 `/etc/passwd`, `/proc`, `/sys`）。

2. **敏感文件阻断**：
   - 阻断所有 `.env` 环境变量文件（如 `.env`, `.env.production`, `.env.local`）；
   - 阻断私钥与证书文件（`.pem`, `.key`, `id_rsa`, `id_ed25519`）；
   - 阻断内部 Git 元数据目录（`.git/config`, `.git/HEAD` 等）。

3. **凭据与 Token 特征检测**：
   - AWS Access Key (`AKIA...`)；
   - OpenAI / Anthropic API Key (`sk-proj-...`, `sk-ant-...`)；
   - GitHub Personal Access Token (`ghp_...`)；
   - Bearer 认证 Token (`bearer eyJ...`)；
   - 私钥证书标头（`-----BEGIN RSA PRIVATE KEY-----`）。

4. **单条变更体量上限与二进制防护**：
   - 单条 diff 严格限制在 `512 KB`（`MAX_DIFF_SIZE_BYTES`）以内，防止大文件导致数据库膨胀或内存攻击；
   - 扫描 diff 内容，若含有 `\0` 二进制空字符，则直接拒绝，防止二进制资产误入文本记忆库。

---

## 4. 备份与恢复 (v4 协议)

- **备份负载版本升级**：`BACKUP_PAYLOAD_VERSION` 升级至 `4`；
- **全量快照包含内容**：
  - `entries`: 事实、规则与意图条目；
  - `evidences`: 证据存根；
  - `relations`: 记忆实体拓扑网；
  - `tasks`: 任务事实归档；
  - `experiences`: 经验记忆库；
  - `task_experiences`: 任务-经验引用图谱；
  - `coding_threads`: 代码演进主题线程；
  - `coding_revisions`: 代码修订历史快照。
- **向下完全兼容**：恢复时校验和依次校验 `checksum_v4 || checksum_v3 || checksum_v2 || checksum_v1`，确保旧版快照 100% 能够无损恢复。
- **引用完整性保障**：恢复时先按依赖拓扑恢复 `coding_threads`，再恢复 `coding_revisions`，避免产生孤儿记录。

---

## 5. 质量与回归验证

- 经由 50+ 个核心单测与完整工作区测试验证；
- 覆盖纯新增、修改、删除、重命名、diff 幂等重放、敏感凭证拦截、二进制防护、超大 diff 拦截、以及跨账户租户绝对隔离。
