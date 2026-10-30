# Lite / Production 运行态观察（2026-10-30）

**机器**：阿里云 2 vCPU / 1 GB（原 2C/2GB 已注销，2C/1G 是本仓当前测试基线），Debian 12，
PostgreSQL 15.19 + pgvector 0.8.7。
**二进制**：CI Release 构建产物（commit `541a9d4` 起的 `memory-hub-binaries-*`，
带 jemalloc，与镜像同构建参数），**未在 1 GB 机器上本地编译**。
**数据库**：`mh_lite` / `mh_prod` 两个独立库，应用首启自动执行 18 个迁移。

---

## 1. 方法

| 形态 | 进程 | 关键配置 |
|---|---|---|
| Lite | `auth-server` + `mcp-server`（2 进程） | `HUB_DEPLOYMENT_PROFILE=lite`、`HUB_EMBEDDED_WORKER=1`、`HUB_JOB_LEASE_H=0`（验证周期调度） |
| Production | `auth-server` + `mcp-server` + **两个** `backup-worker`（4 进程） | `HUB_DEPLOYMENT_PROFILE=production`、`HUB_EMBEDDED_WORKER=0`、`HUB_JOB_LEASE_H=1`、`BACKUP_CRON="*/2 * * * *"` |

观察手段：`/tmp/observe.py`（离线签发 HS256 令牌，调 `/healthz`、`/readyz`、`/capabilities`、
`tools/list` 与 6 个工具），加上进程列表与日志。

## 2. 结果

| 检查 | Lite | Production |
|---|---|---|
| `/healthz` `/readyz` `/capabilities` | 200 / 200 / 200 | 200 / 200 / 200 |
| `deployment.profile` | `lite` | `production` |
| `semantic.ready` | `false`（`embed.provider=off`，原因明确） | `false`（`embed.provider=mock`，**启用了但没有语义信号**） |
| `consolidation.mode` | `rules` | `rules` |
| 工具数 | **38** | **38** |
| `memory_put` / `memory_list` | 成功 | 成功 |
| `cognitive_chat_propose` | `E_METHOD_NOT_FOUND`（不写记忆） | `E_METHOD_NOT_FOUND` |
| `cognitive_intentions_list` | 成功（无服务时直读存储） | 成功 |
| `memory_backup_create` + `restore(dry_run)` | 成功 / 成功 | 成功 / 成功 |
| 迁移（`_hub_migrations`） | 18 条，二次启动不重复执行 | 18 条 |
| 后台作业 | **内嵌**在 mcp-server（日志：`内嵌后台作业调度已启动`） | mcp-server **无**内嵌日志；独立 `backup-worker` 运行 |
| 周期备份 | 每 2 分钟成功一次（`snapshots=1`） | 双副本中**仅一个执行**，另一个 `被租约跳过` |
| 日志脱敏 | token 与记忆正文均未出现在日志 | 同左 |

`backup-worker --once`（Production 库）：

```json
{"mode":"once","profile":"production",
 "backup":{"created":1,"pruned":0,"skipped_leased":false},
 "integration":{"scanned":{"<account>/obs-prod":1},"proposals":{"<account>/obs-prod":0},
                "skipped_leased":[],"failed":[],"truncated":[]}}
```

## 3. 发现的差异与问题

### 3.1 两种 profile 的默认嵌入行为不同（符合设计，但容易误读）

- Lite 启动配置：`embed_provider=off`、`embed_enabled=false`、`expiry sweep=0`；
- Production：`embed_provider=mock`、`embed_enabled=true`、`semantic_ready=false`、`sweep=6h`。

也就是说 **Production 默认会装配 mock 嵌入**：它有"向量"但没有语义信号，
健康检查如实报 `semantic.ready=false`。这与 README「生产必须接真实嵌入模型」一致，
但现场看到 `embed_enabled=true` 容易误以为语义可用——`/capabilities` 里的 `semantic.ready` 才是判据。

### 3.2 租约窗口会压掉重启后的第一次备份（真实行为）

默认 `HUB_JOB_LEASE_H=24`。Lite 首启时备份成功一次；重启后同一租约键在窗口内仍被占用，
**后续每 30 秒 tick 都被跳过**，日志连刷：

```
备份租约被其它副本占用，本轮跳过
定时备份被租约跳过（其它副本已执行），下个 tick 重试
```

这是"窗口内至多一次"的正确语义（多副本去重），但有两个副作用：

1. **单副本重启后 24 小时内不会再备份**——如果 cron 比租约窗口密，这是预期；如果 cron 是每天一次，
   重启恰好落在窗口内就会漏掉当天备份。
2. 日志文案 `其它副本` 在单副本场景下是误导性的（实际是同一节点上一次运行占用的租约）。

本次把 Lite 的 `HUB_JOB_LEASE_H` 设为 0 后，04:15:52 与 04:16:52 各成功备份一次（`snapshots=1`），
证明**调度本身正常**，跳过完全由租约造成。

### 3.3 其余观察

- 二次启动不会重跑迁移（日志 `relation "_hub_migrations" already exists, skipping`）；
- 迁移在 1 GB 机器上正常完成（18 个，含 halfvec 索引与 018 唯一索引）；
- `/healthz` 与 `/readyz` 语义不同：前者只回 `ok` 文本，后者带存储就绪状态。

## 4. 适用边界

- 单次运行、本机回环地址、无 TLS、无反向代理；未测多机部署与真实 OAuth 授权码流程
  （令牌用共享密钥离线签发，线上由 auth-server 颁发）；
- Production 的"4 进程"是为验证租约额外起了一个 `backup-worker`，参考部署是 3 进程；
- 未测：故障重启后的任务续跑（租约窗口内的行为见 §3.2）、真实 embedding 端点。

## 5. 结论

- 两种 profile 的进程形态、作业调度、健康检查与认知入口**均符合文档描述**；
- 未启用抽取器时 `cognitive_chat_propose` 明确不可用且不写数据（与 P1 修复一致）；
- 需要补一条运维说明：**重启后的第一次备份可能被租约窗口压掉**，以及日志文案的歧义。

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
