# 记忆中枢 · Memory Hub

**多智能体（Multi-Agent）长期记忆的存储、备份、整合 MCP 服务。**

多个 AI Agent 共享一套长期记忆：每个 Agent 以自己的身份（OAuth 2.1 客户端）写入、检索记忆；记忆按**命名空间**（namespace）隔离与共享；系统提供**快照备份**与**整合归并**（去重 / 冲突合并）两大基础设施能力。

> 定位声明：本服务只做记忆的存取、备份与整合三件事——不内嵌任何技能框架、行为规范或生成式推理；那些属于 Agent 自己的事。

---

## 快速开始

```bash
# 1. 前置：PostgreSQL 15+ 与 pgvector
docker run -d --name hub-db -p 5432:5432 \
  -e POSTGRES_DB=memory_hub -e POSTGRES_USER=hub_app -e POSTGRES_PASSWORD=secret \
  pgvector/pgvector:pg15

# 2. 配置（所有配置项启动期强校验，非法即 fail-fast）
cp .env.example .env
# 至少修改 HUB_TOKEN_SECRET

# 3. 一键部署（postgres + auth-server + mcp-server + backup-worker）
docker compose up -d --build

# 也可以本地跑（无需 Docker）：
cargo run --bin mcp-server     # MCP 服务 :3000
cargo run --bin auth-server    # 授权服务器 :8080
cargo run --bin backup-worker  # 备份/整合作业（需要 PostgreSQL）
```

开发环境无数据库时也可直接用内存存储启动：`HUB_DB_URL=memory://local`（重启即失忆，仅限开发）。

**多账户沙箱**：终端用户通过控制台注册/登录（`Account`，沙箱边界）。每个账户拥有独立的命名空间空间与自己的 Agent 集合——不同账户可各有 `personal`，互不可见、互不授权、互不共享。

## 三个进程

| 进程 | 职责 |
| :--- | :--- |
| `mcp-server` | MCP 服务：18 个记忆工具 + Streamable HTTP / stdio 传输 |
| `auth-server` | 账户注册/登录/控制台（沙箱边界）+ OAuth 2.1：Agent 注册（DCR，需账户身份）、`client_credentials`、授权码+PKCE、令牌内省/撤销 |
| `backup-worker` | 周期作业：定时快照（cron）、过期记忆归档、整合中枢冲突/重复扫描 |

存储：PostgreSQL 15 + pgvector（生产）或内存实现（开发/测试）。嵌入器：OpenAI 兼容 `/embeddings` 服务或确定性 mock。

## 账户（沙箱）与 Agent

1. 打开 `http://localhost:8080/account/register` 注册账户（设了 `AUTH_INVITE_CODE` 则需邀请码）；
2. 登录后在控制台（`/console`）注册 Agent，或点「生成注册令牌」走程序化 DCR：
   `curl -X POST http://localhost:8080/oauth/register -H "Authorization: Bearer <注册令牌>" ...`
3. 引导账户（`HUB_BOOTSTRAP_USERNAME`/`HUB_BOOTSTRAP_PASSWORD`）承载升级前的存量数据。

## 一次完整调用

```bash
# 0a. 若首次使用：控制台注册账户并登录；再按上面的说明注册 Agent 或取注册令牌
# 0b. 机器 Agent 直接换取访问令牌（client_credentials，需先注册 Agent）
TOKEN=$(curl -s -X POST http://localhost:8080/oauth/token \
  -d 'grant_type=client_credentials&client_id=agent-alpha&client_secret=s3cret' \
  | jq -r .access_token)

# 1. 写入记忆（首次写入的 Agent 自动成为命名空间 owner）
curl -X POST http://localhost:3000 -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{
    "jsonrpc":"2.0","id":1,"method":"tools/call",
    "params":{"name":"memory_put","arguments":{
      "namespace":"team-x","kind":"fact","content":"服务部署在东京区域",
      "tags":["ops","infra"],"ttl_seconds":86400}}
  }'

# 2. 混合检索（FTS 全文 + 向量语义 + 五维加权 + MMR 多样性重排）
curl -X POST http://localhost:3000 -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{
    "jsonrpc":"2.0","id":2,"method":"tools/call",
    "params":{"name":"memory_search","arguments":{
      "namespace":"team-x","query_text":"部署在哪里","top_k":5}}
  }'

# 3. 整合中枢（扫描重复/冲突并一键融合，支持 dry_run 与 auto_merge）
curl -X POST http://localhost:3000 -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{
    "jsonrpc":"2.0","id":3,"method":"tools/call",
    "params":{"name":"memory_consolidate","arguments":{
      "namespace":"team-x","dry_run":false,"auto_merge":true}}
  }'

# 4. 备份 → 恢复（replace 前自动创建安全备份）
BACKUP=$(curl -s -X POST http://localhost:3000 -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{
    "jsonrpc":"2.0","id":4,"method":"tools/call",
    "params":{"name":"memory_backup_create","arguments":{"namespace":"team-x"}}}' \
  | jq -r .structuredContent.backup.id)
```

## 核心特性

### 1. 快速混合检索（Hybrid Retrieval）
- **双路并发召回**：全文倒排索引（FTS）与向量相似度（pgvector HNSW Cosine）并发检索，弥补单一关键词或纯语义的边界短板；
- **五维打分融合**：综合向量相似度、关键词命中率、基于遗忘曲线的时间衰减（Recency）、显式记忆权重（Weight）以及访问频次；
- **MMR 多样性重排**：通过 Maximal Marginal Relevance 算法降低冗余度，避免上下文窗口被高度相似的重复条目侵占。

### 2. 整合中枢（Consolidation Engine）
- **多维冲突与冗余检测**：精确识别相同 Key 的事实分歧（Conflict）、语义高相似的重复碎片（Duplicate，向量阈值可配）以及超期过期数据；
- **分级融合器（Tiered Merger）**：正文内容智能合并，Tag 标签集自动并集，并在元数据深度写入 `merged_from` 完整溯源链；
- **自适应策略调度**：
  - `Manual`：仅生成审查提案，绝不静默合并，严格人工/Agent 裁决；
  - `SemiAuto`：超高置信度（>= 阈值）且轻量条目自动融合，复杂冲突生成提案；
  - `FullAuto`：全自动收敛并归档重复历史。

---

## MCP 工具集（18 个）

| 工具 | 作用域 | 说明 |
| :--- | :--- | :--- |
| `memory_put` | write | 写入/ upsert（同 namespace+key 修订，版本 +1） |
| `memory_get` | read | 按 ID 或 key 读取 |
| `memory_update` | write | 修订部分字段 |
| `memory_delete` | write | 软删除（归档，保留溯源） |
| `memory_search` | read | 混合检索（FTS+向量+五维加权+MMR） |
| `memory_list` | read | 条件列举 + 游标分页 |
| `memory_stats` | read | 命名空间统计 |
| `memory_history` | read | 条目版本历史 |
| `memory_backup_create` | admin | 创建快照（namespace 或全量 `*`） |
| `memory_backup_list` | admin | 列举快照 |
| `memory_restore` | admin | 恢复：dry_run / merge / replace |
| `memory_consolidate` | write | 整合中枢：一键扫描/检测/归并（支持 dry_run 与 auto_merge） |
| `memory_consolidation_stats` | read | 整合健康度与冗余统计（重复组、冲突组、活跃条目） |
| `memory_integrate_scan` | write | 扫描重复/冲突，生成待审提案 |
| `memory_integration_list` | read | 列举待审提案 |
| `memory_integration_resolve` | write | 裁决提案：merge / dismiss |
| `memory_grant` | admin | 命名空间授权给其他 Agent |
| `memory_revoke` | admin | 撤销授权 |

设计规范与实现细节见 [`docs/设计-多智能体长期记忆服务.md`](docs/设计-多智能体长期记忆服务.md)。

## 架构一览

```
                 ┌────────────────────────────┐
   Agent A ─────▶│  MCP Server (mcp-server)   │── 18 个记忆工具
   Agent B ─────▶│  HTTP(Streamable)/stdio    │
                 └───────────┬────────────────┘
                             │
            ┌────────────────┴────────────────┐
            │ Consolidation Engine (整合中枢) │── 冲突检测 / 语义去重 / 分级融合 / 策略调度
            └────────────────┬────────────────┘
                             │ 同一份存储
                 ┌───────────▼────────────────┐
                 │  PostgreSQL 15 + pgvector  │  memories / memory_versions /
                 │  (或内存实现，开发模式)    │  memory_backups / integration_proposals /
                 └───────────▲────────────────│  namespaces / agents / namespace_grants
                             │                └───────────────────────────────────────┘
   Agent 注册/授权 ──▶ auth-server (OAuth 2.1 + DCR + 授权码/PKCE + 控制台)
   定时备份/清理/扫描 ─▶ backup-worker (cron)
```

## 开发

```bash
cargo test --workspace                 # 237 个测试
cargo clippy --all-targets             # 零警告
cargo fmt --all
# PostgreSQL 集成测试（需 DATABASE_URL）
cargo test -p store --features pg-integration -- --ignored
```

## 目录结构

```
crates/
  common/        错误、脱敏日志、鉴权上下文、配置强校验、幂等、限流、cron
  mem-core/      领域模型 + 记忆/备份/整合三大服务 + 命名空间 ACL + 检索打分算法
  consolidation/ 整合中枢引擎（冲突/重复/时效性检测、分级融合器、全/半自动策略调度）
  store/         PostgreSQL (含活跃条件唯一索引/迁移) 与内存两套存储
  embed/         嵌入器（OpenAI 兼容 HTTP / mock）
  retrieval/     混合检索引擎（双路召回 + 五维加权 + MMR 多样性重排 + 软超时）
  authz/         令牌校验（HS256 离线验签、RFC 8707 aud 绑定、RFC 9728 元数据）
  auth-core/     OAuth 安全原语（PKCE S256、Token Family 轮换、HS256）
  auth-web/      OAuth 编排：DCR / client_credentials / 授权码 / 内省 / 撤销 / 控制台
  mcp/           协议层（JSON-RPC 2.0 / Streamable HTTP / stdio）+ 18 工具 schema + 传输层
  worker/        备份作业 / 过期清理 / 整合扫描作业 / cron 调度
bins/
  mcp-server / auth-server / backup-worker
```

## License

MIT OR Apache-2.0

