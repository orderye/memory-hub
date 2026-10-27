# 双部署模式：个人轻量版与完整生产版

[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)

> 本文描述**当前实现**的两种部署形态。默认值永远是 `production`：升级不改变既有实例行为，
> Lite 必须由部署者显式选择（`crates/common/src/profile.rs`）。

---

## 1. 一套实现，两套默认值

| 维度 | 个人轻量版 `lite` | 完整生产版 `production` |
|---|---|---|
| 部署入口 | `docker-compose.lite.yml` | `docker-compose.yml` |
| 核心领域逻辑 | 完全相同 | 完全相同 |
| 存储 | PostgreSQL + pgvector（不换引擎） | 同左 |
| OAuth 2.1 / ACL / 账户沙箱 | **保留** | 保留 |
| MCP 工具 | **全部 38 个** | 全部 38 个 |
| 备份 / 恢复 | **保留** | 保留 |
| 认知记忆（Chat/Working/Coding） | **保留** | 保留 |
| 嵌入服务 | 默认 `HUB_EMBED_PROVIDER=off`（不落向量、不走向量路） | 可配置真实 HTTP 嵌入 |
| 周期性清扫 | 默认关闭（`EXPIRY_SWEEP_INTERVAL_H=0`） | 默认开启 |
| 后台作业 | 由 `HUB_EMBEDDED_WORKER` 决定（lite compose 默认 1） | 独立 worker 或内嵌 |
| 多用户 / 多命名空间 | 支持（单用户可只用 `default`） | 支持 |

**没有编译期特性开关。** 当前所有可选组件都已是运行期可选（嵌入器 `off`、清扫间隔 `0`、
内嵌 worker 开关），再加 Cargo features 会把测试矩阵翻倍而收益有限。这是设计决策，
不是遗漏（见 `docs/upgrade/LIGHTWEIGHT_AUDIT.md` 的组件盘点）。

---

## 2. 组件矩阵

| 组件 | 编译期 | 运行期 | lite 必需 | production 必需 |
|---|---|---|---|---|
| `auth-server` | 二进制 | — | 是（或用既有令牌） | 是 |
| `mcp-server` | 二进制 | — | 是 | 是 |
| `backup-worker` | 二进制 | — | 可选（手动备份） | 是（定时备份） |
| PostgreSQL 15 + pgvector | 外部依赖 | — | 是 | 是 |
| 嵌入器（HTTP） | 已编译 | `HUB_EMBED_PROVIDER` | 否 | 可选 |
| 内嵌作业调度 | 已编译 | `HUB_EMBEDDED_WORKER` | 可选 | 可选（与独立 worker 二选一） |
| 整合模式 | 已编译 | `HUB_CONSOLIDATION_MODE` | `rules` | `rules` 或 `hybrid` |
| 生成式 LLM | **不依赖** | — | 否 | 否 |

---

## 3. 启动

### 3.1 个人轻量版

```bash
cp .env.example .env
# 必填：POSTGRES_PASSWORD；建议保持 HUB_DEPLOYMENT_PROFILE=lite
docker compose -f docker-compose.lite.yml up -d
```

关键环境变量（compose 已内置）：

```env
HUB_DEPLOYMENT_PROFILE=lite
HUB_EMBED_PROVIDER=off
HUB_EMBEDDED_WORKER=1
HUB_DB_MAX_CONNECTIONS=4
```

期望行为：

- `/readyz` 与 `/capabilities` 明确报告 `semantic.ready=false`（不伪报语义可用）；
- 检索为纯关键词/词法路径，整合候选发现为纯本地规则；
- 备份/恢复、认知记忆、全部 MCP 工具可用。

### 3.2 完整生产版

```bash
cp .env.example .env
# 显式设置生产参数（默认即 production，但建议写出来）
docker compose up -d
```

```env
HUB_DEPLOYMENT_PROFILE=production
HUB_EMBED_PROVIDER=http
HUB_EMBED_ENDPOINT=https://your-embedding-endpoint/v1
HUB_EMBED_MODEL=your-model
HUB_EMBED_DIM=1024
HUB_CONSOLIDATION_MODE=rules        # 需要向量辅助时显式改 hybrid
HUB_CONSOLIDATION_FALLBACK=rules    # 向量服务不可用时降级
```

> 语义检索必须同时满足：`HUB_EMBED_PROVIDER=http`、端到端可用、迁移 015 的 halfvec 索引已建立。
> 只配 `http` 不会自动把整合切成 `hybrid`（`HUB_CONSOLIDATION_MODE` 独立控制）。

---

## 4. 升级与降级

### 4.1 升级

1. **先备份**：`memory_backup_create`（全量）并校验 `memory_backup_list` 中的校验和；
2. 应用迁移（按版本顺序执行 `crates/store/migrations/*.sql`；CI 的 `postgres` job 是唯一执行口径）；
3. 滚动替换服务二进制；
4. 验证：`/readyz`、`memory_stats`、各一条 Chat/Working/Coding 读写。

迁移 `016` / `017` 均为 additive：新增列/表/约束，不重命名、不删除既有列，可重复执行。

### 4.2 降级（代码回滚）

- 每个阶段独立提交，可单独 `git revert`；
- 迁移 down SQL 见 `docs/upgrade/GOVERNANCE_RELEASE.md`；
- 已写入的 `rejected` 经验在旧代码中会被当作未知状态：**降级前**先执行
  `update memory_experiences set status='deprecated' where status='rejected';`（旧代码支持 `deprecated`）；
- 集合表对旧代码不可见，不影响旧代码运行；可保留或按 down SQL 删除。

### 4.3 模式切换

两种模式**共用同一份 schema 与备份格式**，可以双向切换：

- `lite → production`：设置真实嵌入器与 `HUB_DEPLOYMENT_PROFILE=production`；历史数据无需迁移；
- `production → lite`：设置 `HUB_EMBED_PROVIDER=off`；已有向量保留但不再被查询，重新开启后继续可用。

---

## 5. 备份与恢复

| 内容 | 是否进入命名空间备份 |
|---|---|
| 记忆条目 / 版本历史 | 是 |
| 证据存根 / 关系 | 是 |
| Working 任务 / 经验 / 关联 | 是 |
| 经验版本快照（迁移 016） | 是 |
| Coding 线程 / 修订 | 是 |
| 记忆集合与绑定（账户级配置） | **否**（配置项，可重建；不删除记忆） |

恢复仍遵循既有语义：`dry_run` / `merge` / `replace`；`replace` 前自动创建安全备份；
失败按 `RestoreReport` 逐项报告，不把部分恢复报成成功。

---

## 6. 验证状态（如实记录）

| 项 | 状态 | 说明 |
|---|---|---|
| 单元 / 集成测试（内存后端） | 见 `GOVERNANCE_RELEASE.md` | 本地可执行 |
| 真实 PostgreSQL 迁移与集成测试 | **NOT RUN (local)** | 本机无 docker / psql；由 CI `postgres` job 执行 |
| `docker compose` 实际启动 | **NOT RUN (local)** | 本机无 docker；配置为静态审查 |
| 真实嵌入检索效果 | **NOT RUN** | 需要外部嵌入服务；Mock/伪向量结果不得作为语义证据 |

复现命令见 `GOVERNANCE_RELEASE.md` 的测试章节。
