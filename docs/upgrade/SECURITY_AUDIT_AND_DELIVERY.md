# 安全审计与最终交付

**阶段：** P2（Task 7）
**时间：** 2026-10-29
**分支：** `reliability/productionization`
**基线：** `d35f033`（GitHub `main`）

---

## 一、安全审计（G1 / G2 / G3 / G4）

### 1. 入口—身份—权限—数据 映射

| 入口 | 身份来源 | 权限检查 | 数据边界 |
|---|---|---|---|
| HTTP MCP (`/mcp`) | `Authorization: Bearer` → `TokenIntrospector` | `required_scope_for_tool`（schema 驱动）+ 工具层 ACL | `AccountScopedStore` 按 `auth.account_id` 前缀 |
| SSE MCP | 同上（连接时校验） | 同上 | 同上 |
| stdio MCP | 本地 agent（`HUB_STDIO_LOCAL_AGENT`）或要求显式认证 | 同上 | 同上 |
| auth-server OAuth | 客户端凭据 / PKCE | claims: `exp`、`iss`、`aud`、`sub`、`account` | 签发含 account claim 的令牌 |

**结论**：三层一致——传输层校验令牌 → dispatch 层按 schema 强制 scope → 工具/服务层按 namespace ACL。MCP 与普通 API 共用同一 `AuthCtx` 语义，不存在"工具绕过 API 授权"的路径。

### 2. OAuth 2.1 校验（`crates/authz/src/validator.rs`）

逐项确认：

- `exp <= now` → `AuthRequired`（过期拒绝）
- `iss != expected_issuer` → `AuthRequired`
- `aud != expected_resource_uri` → `InsufficientScope`（RFC 8707 防跨服务透传）
- `sub` 为空 → `AuthRequired`
- **缺少 `account` claim → `AuthRequired`**（沙箱边界，旧令牌一律拒收）

### 3. 权限边界实证（本轮新增测试）

| 测试 | 断言 |
|---|---|
| `test_cross_namespace_access_denied_without_leaking_existence` | 跨 namespace 读"存在但无权"与"根本不存在"返回**同一错误类**，无法探测资源存在性；跨 namespace 写被拒且不污染原数据 |
| `test_restore_checks_acl_for_namespaces_derived_only_from_relations` | 仅由 relations 派生的 namespace 也必须通过 `require_write`，不能绕过 ACL |
| `test_grant_rejects_cross_account_and_wildcard`（既有） | `memory_grant` 要求 admin scope + 同账户 + namespace owner，且拒绝通配 |
| `test_backup_and_restore_are_isolated_between_accounts`（Task 1） | 另一账户无法读取或恢复本账户快照 |

### 4. 管理操作边界

`memory_grant` / `memory_revoke` / `memory_backup_create` / `memory_restore` 均声明 `memory:admin`。
其中：

- `memory_backup_create`：admin scope + `require_write`（每 namespace）
- `memory_restore`：admin scope + `require_write`（每个目标 namespace，**含 relations/task-links 派生的**）
- Replace 前先建安全备份，`require_write` 在写数据之前执行

### 5. 证据与敏感信息（G3）

- 写入前脱敏：`sanitize_evidence_excerpt`（`chat_cognitive_service` 证据落库路径）
- 校验闸门：`contains_sensitive_credentials`（候选/经验/diff 三道）
- 删除同步：`memory_versions`、`memory_relations` 均为 `on delete cascade`，
  删除记忆时历史与关系一并清除；证据到期由 `purge_expired_evidence` 物理删除
- 日志：认知作业失败日志不含正文/token；校验错误为固定文案，不回显内容
- 备份：备份负载含记忆内容（这是备份的固有语义），因此备份读写均要求 `memory:admin` + ACL

### 6. 供应链（G4）

CI `supply-chain` job 已通过：`cargo-deny check all --all-features` + `rustsec/audit-check`。
唯一豁免 `RUSTSEC-2023-0071`（rsa 时序侧信道）风险敞口为零——`rsa` 仅由 sqlx 可选 MySQL 驱动引入，
本项目只启用 postgres，该 crate 不参与编译。

---

## 二、变更清单（本轮全部提交）

| 提交 | 类型 | 内容 |
|---|---|---|
| `a9ca558` | fix | v6 备份格式补全（含 memory_versions）、未知版本拒绝、Replace 清理错误不再吞 |
| `5cb129e` | refactor | Replace 走单次原子多 namespace 恢复；内存/测试存储实现回滚 |
| `e063d0e` | fix | **数据丢失缺陷**：关系快照泄漏内部作用域键导致恢复时静默丢弃关系 |
| `fc3ce64` | feat | 恢复审计（此前 `audit_log` 从未被写入）+ 保留策略边界 |
| `682b1f0` | fix | 跨进程 open 提案唯一性（迁移 018 部分唯一索引 + 原子入口） |
| `a8d72bc` | docs | 认知作业调度核实（未启用 + Mock 抽取器契约测试） |
| `154f57a` | fix | **索引失效缺陷**：ANN 状态谓词参数化导致部分索引不被选用 |
| `e4baa0b` | fix | 评测基线不可比时改为硬失败（此前静默跳过） |
| `8ddd04c` | docs | 阻塞项整理 |
| `7e1fbe5` / `b54a246` / `044eb73` | fix | PG 测试修正（时间戳精度、ACL 作用域、payload 可见名） |
| `98044b9` | test | v1–v6 矩阵、迁移 018 清理、ANN 索引命中 |
| `8a2dbc2` | docs | 阻塞解除结果 |
| `9a91c6e` | test | 跨 namespace 越权与存在性泄露 |

---

## 三、测试结果汇总

| 范围 | 结果 |
|---|---|
| 本地 `cargo test --workspace --all-features` | **564 passed / 0 failed / 19 ignored** |
| 本地 `cargo fmt --all -- --check` | 通过 |
| 本地 `cargo clippy --workspace --all-targets --all-features -- -D warnings` | 通过 |
| CI 质量门禁 | 通过 |
| CI PostgreSQL 集成测试 | **12 passed / 0 failed**（pgvector:pg15） |
| CI 依赖准入与安全审计 | 通过 |
| CI Release 构建 | 通过 |
| ignored 项说明 | 19 项为需真实端点/校准/消融/微基准，已在报告中标注未验证 |

---

## 四、回滚手册

### 4.1 应用代码回滚

```bash
git revert <commit>   # 或 checkout 上一 tag
```

**注意**：v6 备份写入后，回滚到旧二进制**无法恢复 v6 备份**（旧代码只认 v1–v5 校验和）。
回滚前先确认是否有 v6 备份需要保留。

### 4.2 数据库迁移回滚

| 迁移 | 可逆性 | 回滚方式 |
|---|---|---|
| 018（提案唯一索引） | 索引可逆，**清理不可逆** | `drop index if exists uq_integration_proposals_open_business_key`；被删除的重复提案只能靠执行前 dump 恢复 |
| 015（向量索引） | 可逆但需维护窗口 | 重建 `idx_memories_embedding`（fp32 全量）后删 `idx_memories_embedding_hf` |
| 001–017 | 不提供 down 脚本 | 前向修复 |

### 4.3 备份恢复回滚

- Replace 前自动创建安全备份（`safety_backup_ids`），可用其回放找回现场
- 恢复失败时：Replace 走单事务，失败整体回滚；Merge 为逐条写入，失败计数见 `RestoreReport`
- **禁止**用损坏备份覆盖唯一有效备份

### 4.4 生产执行迁移 018 前必做

```sql
-- 1) 核对将被删除的重复行
select namespace, kind, member_ids, count(*) as dup, min(created_at), max(created_at)
from integration_proposals where status = 'open'
group by namespace, kind, member_ids having count(*) > 1;

-- 2) 备份该表
pg_dump -t integration_proposals -Fc memory_hub > integration_proposals_pre018.dump
```

---

## 五、剩余风险清单（按严重程度）

| # | 风险 | 严重度 | 是否阻塞发布 | 说明 |
|---|---|---|---|---|
| R1 | 真实 embedding 语义质量未验证 | 高 | **是**（若宣称语义检索） | 无端点；默认规则模式不受影响，Lite 可正常用 |
| R2 | 迁移 018 清理会删除重复提案 | 高 | 否（按 §4.4 先备份） | 仅限重复组，保留最早一条 |
| R3 | 大表 `CONCURRENTLY` 耗时未实测 | 中 | 否 | runbook 有可执行步骤 |
| R4 | Lite/Production 运行态未观察 | 中 | 否 | 静态装配与契约测试已覆盖调度集合 |
| R5 | 镜像同步无自动偏差检测 | 中 | 否 | GitHub 为权威源；建议加 CNB 侧 patch-id 校验 |
| R6 | 认知作业未接入生产调度 | 低 | 否 | 有意为之；启用前需补齐 5 项（见 COGNITIVE_SCHEDULING_AUDIT） |
| R7 | 阈值 0.65 在伪向量下标定 | 低 | 否 | 接真实模型后必须重标定 |

---

## 六、完成判定

| 任务书验收条件 | 状态 |
|---|---|
| P0-A 备份完整性有测试证据 | ✅ 离线 + PG 实测 |
| P0-B 跨进程 proposal 一致性有测试证据 | ✅ PG 实测（唯一索引） |
| 数据库升级路径实际验证 | ✅ 迁移 015/018 在 PG 上应用并验证 |
| 真实 embedding 能力边界未被夸大 | ✅ 三类能力分离，语义结论标为未验证 |
| 默认规则模式/Lite/安全边界未破坏 | ✅ 规则模式零嵌入调用；Lite 拒绝常驻；ACL 测试通过 |
| OAuth/ACL/MCP/备份恢复/Chat-Working-Coding 兼容 | ✅ 无公开 API/工具名/schema 变更 |
| CI、测试和镜像状态可追溯 | ✅ CI run 全绿；GitHub 权威源、CNB 镜像 |
| 未解决高风险问题明确列出 | ✅ 见 §5 |

**唯一阻止"完全完成"判定的项是 R1（真实 embedding 未验证）**——它是能力缺口，不是缺陷；
在补齐真实模型评测前，不得宣称语义检索达标。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
