# 记忆中枢 · Memory Hub

> **语言 / Language**：[简体中文](README.md) · [English](README.en.md)

**给一堆 AI 助手共用的一本长期记忆账本：记得住、查得到、分得清、丢不了。**

[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![Edition](https://img.shields.io/badge/edition-2021-blue.svg)](https://doc.rust-lang.org/edition-guide/rust-2021/index.html)
[![Tests](https://img.shields.io/badge/tests-passing-brightgreen.svg)](https://github.com/orderye/memory-hub/actions/workflows/ci.yml)
[![Clippy](https://img.shields.io/badge/clippy-zero%20warnings-brightgreen.svg)](https://github.com/orderye/memory-hub/actions/workflows/ci.yml)
[![MCP](https://img.shields.io/badge/MCP-38%20tools-purple.svg)](https://modelcontextprotocol.io/)
[![Storage](https://img.shields.io/badge/storage-PostgreSQL%2015%20%2B%20pgvector%20%7C%20in--memory-blue.svg)](https://www.postgresql.org/)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

[![powered by KimiCode](.github/assets/kimi-badge.svg)](https://www.kimi.com/code) [![powered by dsh](https://img.shields.io/badge/powered_by-dsh-4D6BFE?style=flat-square&logo=deepseek&logoColor=white)](https://github.com/deepseek-ai/deepseek-harness)

用一句大白话说：**AI 助手一关窗口就失忆，这个服务负责把该记住的东西存起来，下次接着用。**

它对外是一个标准 MCP 服务，Claude Desktop、Cursor、Cline、LangChain 之类的 Agent 框架都能直接接上；
内部管好了四件事——**存取、备份、整合、演化**。

> **它不做什么（很重要）**：不往记忆里塞 Prompt 规范，也不替 Agent 做推理决策。
> 它只保证"返回的记忆是真实、结构化、带证据的"，怎么用是 Agent 自己的事。

---

## 目录

- [一、它解决什么麻烦](#一它解决什么麻烦)
- [二、整体长相：三个进程 + 一个数据库](#二整体长相三个进程--一个数据库)
- [三、快速开始](#三快速开始)
- [四、记忆是怎么组织的](#四记忆是怎么组织的)
- [五、检索是怎么工作的](#五检索是怎么工作的)
- [六、安全与可靠性](#六安全与可靠性)
- [七、MCP 工具清单（38 个）](#七mcp-工具清单38-个)
- [八、项目结构](#八项目结构)
- [九、开发、测试与质量门禁](#九开发测试与质量门禁)
- [十、已知限制与下一步](#十已知限制与下一步)
- [十一、文档导航](#十一文档导航)

---

## 一、它解决什么麻烦

让 Agent 长期记住事情，坑基本都在这几处。下面是"麻烦"和"这个服务怎么处理"的对照：

| 常见麻烦 | Memory Hub 的做法 |
| :--- | :--- |
| **助手一关窗口就忘了** | 记忆存在服务端（PostgreSQL），换个会话、换台机器都能接着查 |
| **好几个助手各记各的，重复还互相矛盾** | 整合中枢自动扫出"重复"与"冲突"，能自动合并，也能生成提案交给人裁决 |
| **老信息被翻出来当新信息用** | 记忆带有效期（`valid_from` / `valid_until`）。新事实取代旧事实时，旧的那条自动"过期封存"，检索默认不返回它 |
| **模型编造事实、编造代码改动** | 写入前先把原始材料存成**只增不改的证据**（`memory_evidence`）；代码只认真实 Git diff，不认模型口述 |
| **在这台机器上管用的经验，换台机器就闯祸** | 经验条目带适用环境标签（如 `linux`、`k8s`）。检索时环境对不上会**显式告警并降权** |
| **多租户/多用户数据互相看得见** | 以"账户"为硬边界做沙箱：即使两个账户用了同名的命名空间，数据也完全隔离（有测试守着，越权次数必须是 0） |
| **记忆库用久了变慢、越查越乱** | 关键词 + 语义双路检索、相似结果去重、向量服务挂了自动降级为关键词检索，检索接口不会挂死 |

### 一句话定位

不做"又一个 Agent 框架"，只做**基础设施**：一个可以被多个 Agent 共享、按账户隔离、可备份可恢复的记忆底座。

---

## 二、整体长相：三个进程 + 一个数据库

| 进程 | 对外形态 | 它管什么（打个比方） |
| :--- | :--- | :--- |
| **`mcp-server`** | HTTP `:3000`（`/mcp` Streamable HTTP、`/sse` 旧版 SSE），也支持 stdio | 图书馆前台：接收记忆读写与检索请求，对外提供 38 个 MCP 工具 |
| **`auth-server`** | HTTP `:8080`，带网页控制台 | 门卫 + 办证处：注册账户、登记 Agent、发令牌（OAuth 2.1） |
| **`backup-worker`** | 后台常驻 + 定时任务 | 夜班管理员：定时做备份快照、清理过期记忆、离线扫描重复与冲突 |
| **PostgreSQL 15 + pgvector** | 数据文件 | 仓库本体（本地开发可以换成纯内存模式，重启即失忆，仅用于调试） |

三个进程都是**无状态**的，可以各自多开几份来扛量。

> **两进程形态（可选）**：给 `mcp-server` 设 `HUB_EMBEDDED_WORKER=1`，backup-worker 的
> 常驻作业（定时备份、过期/证据清理、整合扫描）会改在 MCP 进程内按**同一 cron 与跨进程
> 租约**运行，部署收敛为 auth-server + mcp-server 两个进程。Lite 部署默认已开启；
> 作业逻辑只有这一份（`worker::embedded`），独立进程与内嵌模式行为完全一致。

```mermaid
flowchart TD
    subgraph Agents["多个 AI 助手"]
        A1["助手 A 写代码"]
        A2["助手 B 排障"]
        A3["助手 C 聊天"]
    end

    Auth["auth-server :8080<br/>账户注册 / Agent 登记 / OAuth 2.1 发令牌"]

    subgraph Hub["mcp-server :3000"]
        Guard["鉴权与账户隔离检查"]
        Tools["38 个工具路由"]
        Engines["认知引擎组<br/>对话 / 经验 / 代码 / 整合"]
        Search["混合检索引擎"]
        Guard --> Tools --> Engines
        Engines --> Search
    end

    Worker["backup-worker<br/>定时快照 / 过期清理 / 离线扫描"]
    DB[("PostgreSQL + pgvector<br/>记忆 / 证据 / 快照 / 提案")]

    A1 -->|1 换令牌| Auth
    A2 -->|1 换令牌| Auth
    A3 -->|1 换令牌| Auth
    A1 -->|2 带令牌调工具| Guard
    A2 -->|2 带令牌调工具| Guard
    A3 -->|2 带令牌调工具| Guard
    Search --> DB
    Worker --> DB
    Auth --> DB
```

---

## 三、快速开始

### 两种部署形态：Personal Lite / Production Full

同一套代码、同一套数据库 schema、同一套备份格式，靠一个配置项选择**默认行为**：

| | Personal Lite（个人轻量） | Production Full（完整生产，默认） |
| :--- | :--- | :--- |
| 适用 | 单机、个人、少量 Agent | 团队 / 多账户 / 对外服务 |
| 进程 | postgres + auth-server + mcp-server（三进程） | 再加**常驻** backup-worker（四进程） |
| 嵌入 | `HUB_EMBED_PROVIDER=off`：不产生向量、不外呼任何嵌入服务 | `mock`（开发）或 `http`（真实语义） |
| 检索 | 纯关键词（FTS + 五维评分 + `min_score` 域外拒绝） | 关键词 + pgvector 语义召回 |
| 整合 | 纯本地规则（embedding 调用恒为 0） | 同左，可显式切 `hybrid` |
| 后台作业 | 内嵌进 mcp-server（定时备份 + 每日扫描；清扫类仍手动 `--once`） | 常驻进程：cron 备份 + 周期清理 + 跨副本租约 |
| 安全能力 | OAuth 2.1 / ACL / 账户隔离 / 备份恢复 **全部保留** | 同左 |

> **升级不会改变现有行为**：`HUB_DEPLOYMENT_PROFILE` 默认就是 `production`，
> 不设置它 = 今天的行为。Lite 只是"同一实现的另一组默认值"，不是另一套代码。

### 方式 A：Docker Compose 一键起（推荐）

```bash
git clone https://github.com/orderye/memory-hub.git && cd memory-hub

# 1. 复制配置模板
cp .env.example .env

# 2. 至少填两个必填项：
#    - HUB_TOKEN_SECRET：令牌签名密钥，至少 32 字节的随机串（两个服务必须一致）
#    - POSTGRES_PASSWORD：数据库口令（没填 compose 会直接拒绝启动）
#
# 3. 编译并启动全部 4 个容器（数据库 + 授权 + MCP + 后台任务）
docker compose up -d --build

# 4. 看状态、看日志
docker compose ps
docker compose logs -f mcp-server
```

起来之后直接打开 <http://localhost:8080/account/register> 注册账户，再进 <http://localhost:8080/console> 登记一个 Agent（拿到 `client_id` / `client_secret`）。

### 方式 A2：个人轻量部署（Lite）

```bash
# 1. 同样的 .env（至少填 HUB_TOKEN_SECRET 与 POSTGRES_PASSWORD）
cp .env.example .env

# 2. 起 Lite：三进程形态（定时备份已内嵌进 mcp-server），嵌入默认关闭
docker compose -f docker-compose.lite.yml up -d --build

# 3. 需要时手动跑一轮完整作业（含内嵌模式不覆盖的过期清理 → 证据清理），跑完即退出
docker compose -f docker-compose.lite.yml run --rm backup-worker --once
```

Lite 下 `memory_search` 是**纯关键词检索**：响应的 `capabilities.keyword_only` 为 `true`，
`/capabilities` 里 `retrieval.vector_path_enabled=false`、`semantic.ready=false`。
想启用真实向量检索，在 `.env` 里显式配置 `HUB_EMBED_PROVIDER=http` +
`HUB_EMBED_ENDPOINT` + `HUB_EMBED_API_KEY` 后重启——**只有显式配置才会把记忆正文发往嵌入端点**。

### 方式 B：本地直接跑（开发调试）

本机没有 PostgreSQL 也不要紧，`memory://local` 是纯内存模式：

```bash
# 终端 1：授权服务
HUB_DB_URL=memory://local HUB_TOKEN_SECRET=dev-secret-at-least-32-chars-long \
  cargo run --bin auth-server

# 终端 2：MCP 服务
HUB_DB_URL=memory://local HUB_TOKEN_SECRET=dev-secret-at-least-32-chars-long \
  cargo run --bin mcp-server

# 终端 3（可选）：后台定时任务
HUB_DB_URL=memory://local HUB_TOKEN_SECRET=dev-secret-at-least-32-chars-long \
  cargo run --bin backup-worker
```

### 第一次调用：拿令牌 → 写一条 → 查出来

**第 1 步：机器 Agent 用 `client_credentials` 换令牌**

```bash
TOKEN=$(curl -s -X POST http://localhost:8080/oauth/token \
  -d "grant_type=client_credentials" \
  -d "client_id=agent-alpha" \
  -d "client_secret=your-agent-secret" \
  -d "scope=memory:write memory:read memory:admin" \
  | jq -r .access_token)

echo "拿到令牌：${TOKEN:0:16}..."
```

**第 2 步：写一条记忆**

MCP 端点是 `http://localhost:3000/mcp`，走标准 JSON-RPC。

```bash
curl -s -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "name": "memory_put",
      "arguments": {
        "namespace": "infra-team",
        "key": "db-cluster-spec",
        "kind": "fact",
        "content": "生产数据库已升级到 PostgreSQL 16，启用 pgvector 向量索引",
        "tags": ["database", "infra", "pgvector"],
        "confidence": 0.98
      }
    }
  }' | jq .
```

**第 3 步：检索出来**

```bash
curl -s -X POST http://localhost:3000/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "tools/call",
    "params": {
      "name": "memory_search",
      "arguments": {
        "namespace": "infra-team",
        "query_text": "生产数据库用的哪个版本？",
        "scene": "working",
        "top_k": 3
      }
    }
  }' | jq .
```

**第 4 步：整理 + 备份**

```bash
# 扫重复和冲突，高置信度的直接合并，其余生成提案
curl -s -X POST http://localhost:3000/mcp -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_consolidate",
       "arguments":{"namespace":"infra-team","dry_run":false,"auto_merge":true}}}' | jq .

# 打一个不可变快照
curl -s -X POST http://localhost:3000/mcp -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_backup_create",
       "arguments":{"namespace":"infra-team"}}}' | jq .
```

### 接入 MCP 客户端：三种传输方式（Stdio / Streamable HTTP / SSE）

MCP 客户端（Cherry Studio、Claude Desktop、Cursor、Cline、ZCode 等）添加服务器时都要选一种传输方式，本服务三种都支持，跑的是同一套 38 个工具，区别只在"JSON-RPC 帧怎么走"：

| 客户端里的选项 | 地址 / 启动方式 | 什么时候用 |
| :--- | :--- | :--- |
| **Stdio** | 启动子进程 `mcp-server`（`HUB_SERVE_MODE=stdio`） | 本机单人使用，不暴露端口 |
| **Streamable HTTP** | `http://localhost:3000/mcp` | **推荐**：新版 MCP 客户端的默认协议 |
| **SSE** | `http://localhost:3000/sse` | 兼容：只支持旧版（2024-11-05）HTTP + SSE 的客户端 |

#### ① Stdio

服务以子进程形式活在客户端进程里，stdin/stdout 走换行分隔的 JSON-RPC 帧。适合"就我自己用"：

```json
{
  "mcpServers": {
    "memory-hub": {
      "type": "stdio",
      "command": "cargo",
      "args": ["run", "--bin", "mcp-server"],
      "env": {
        "HUB_SERVE_MODE": "stdio",
        "HUB_DB_URL": "memory://local",
        "HUB_TOKEN_SECRET": "dev-secret-at-least-32-chars-long",
        "HUB_STDIO_LOCAL_AGENT": "local-agent"
      }
    }
  }
}
```

- `HUB_DB_URL` 按需换成 PostgreSQL 连接串，即可与其他部署共用同一个记忆库；
- `HUB_STDIO_LOCAL_AGENT` 只在 `HUB_DEPLOYMENT_PROFILE=lite` 下生效：stdio 会话被视作"进程所有者本人的本地桥接"，以 bootstrap 账户全权限运行；不设置则与 HTTP 一样必须带令牌；
- Docker 部署想走 stdio 也行：`"command": "docker", "args": ["exec", "-i", "<mcp 容器名>", "mcp-server"]`，容器环境需设 `HUB_SERVE_MODE=stdio`。

> Claude Desktop 原生只支持 Stdio；Cursor / Cherry Studio 等三种都支持，`type` 字段的写法按所用客户端的枚举填（如 `streamableHttp` / `http`）。

#### ② Streamable HTTP（推荐）

所有 JSON-RPC 帧直接 POST 到同一个端点，无需握手：

```json
{
  "mcpServers": {
    "memory-hub": {
      "type": "streamableHttp",
      "url": "http://localhost:3000/mcp",
      "headers": { "Authorization": "Bearer <按上文流程换取的令牌>" }
    }
  }
}
```

令牌从授权服务换：`POST http://localhost:8080/oauth/token`（`client_credentials`），见上面"第一次调用"。本地开发（内存模式、限流关闭时）也可以先不带令牌调 `initialize` / `tools/list`，但 `tools/call` 一律要求令牌。

#### ③ SSE（旧版兼容）

客户端先 `GET /sse` 建立下行事件流，服务端推回 `endpoint` 事件告知上行地址（`/messages?sessionId=...`），此后把 JSON-RPC 帧 POST 到该地址，响应再从下行流推回。客户端配置里只填 `/sse` 这一个地址：

```json
{
  "mcpServers": {
    "memory-hub": {
      "type": "sse",
      "url": "http://localhost:3000/sse",
      "headers": { "Authorization": "Bearer <按上文流程换取的令牌>" }
    }
  }
}
```

两个注意点：

- **反代要关缓冲**：SSE 是长连接事件流，nginx 之类默认缓冲会把它卡到超时。服务端已回 `X-Accel-Buffering: no`，且 `/sse` 不吃 30s 请求硬超时；自建反代时同样要配 `proxy_buffering off`；
- **会话跟连接绑定**：SSE 断开即会话失效，客户端重连会拿到新的 `sessionId`。

> 什么时候选 SSE：客户端只实现了 2024-11-05 的 "HTTP with SSE"（某些旧版客户端）。能用 Streamable HTTP 就不要用 SSE。

---

## 四、记忆是怎么组织的

### 1. 先存证据，再做结论

任何"从原始材料里提炼出的事实"，提炼之前都会先把原始材料原封不动存进 `memory_evidence`（只增不改）。
提炼出来的事实挂着证据 ID，于是任何一条记忆都能回答"你凭什么这么说"。

### 2. 每条记忆都有时间概念

- `occurred_at`：事情**真实发生**的时间（不是"什么时候记的"）；
- `valid_from` / `valid_until`：这条信息**在什么时候是对的**。

举个例子：主节点从 v1 换成 v2 后，系统会把 v1 那条的 `valid_until` 自动封口，检索默认只返回 v2。
想翻旧账时传 `include_superseded`，旧条目会带着降权系数一起返回——历史留痕，但不会干扰当前判断。

### 3. 三层认知记忆，各管各的

| 场景 | 管什么 | 关键点 |
| :--- | :--- | :--- |
| **Chat**（对话） | 事实 `Fact`、摘要 `Summary`、长期偏好 `Pattern`、待办意图 `Intention` | 意图带有效期，过期自动失效；"或许会 / 待定"这类含糊措辞在入库前就会被拒，防止"幻觉待办"；入库可选全自动、半自动或人工裁决 |
| **Working**（干活） | 单次任务 `WorkingFact` 与可复用经验 `Experience` 分开存 | 提炼只产出**候选**经验；必须显式 `verify` 且满足「≥2 个不同已完成来源 + 无失败任务 + 有验证方法」才升级为已验证；带适用环境标签，环境不符会告警降权；每次状态流转写版本快照 |
| **Coding**（改代码） | 逻辑线程 `CodingThread` + 版本切片 `CodingRevision` | 只记**真实 Git diff**；同样的补丁重复写会幂等去重；顺手扫描私钥、Token 等敏感串并截断；提供按线程/文件/单条的只读查询来重建演化与回滚链 |

### 4. 经验资产：提炼只是候选，验证才升级

Working 场景里的"经验"不是写完就能复用的。它走一条显式的资产生命周期：

```
工作事实 ──distill──▶ candidate（候选）
                          │
                          │ verify（≥2 个不同已完成来源 + 无失败 + 有验证方法）
                          ▼
                        active（已验证，可复用）
                          │
        reject（需理由）──┼── deprecate（需理由）
                          ▼
                    rejected / deprecated（保留来源与版本，不物理删除）
```

- **单次成功不等于已验证**：正反馈只加反馈分，不改状态；自动提炼与后台作业都不会把候选直接升级；
- **只在声明的范围内推荐**：`boundary_envs` 不匹配会降权或按 `strict_env` 直接拒绝；
- **版本不丢**：每次状态流转写入 `memory_experience_versions`（每个 version 一行），可回看"谁在什么时候因为什么改了它"；
- **检索有优先级**：`active` 权重 1.0、`candidate` 0.6、`deprecated` 0.3；`rejected/archived` 默认不返回。

对应工具：`working_experience_distill`、`working_experience_transition`、`working_experience_history`、`working_experience_feedback`。细节见 [`docs/upgrade/ASSET_GOVERNANCE.md`](docs/upgrade/ASSET_GOVERNANCE.md)。

### 5. Agent 记忆集合：只缩小范围，不发放权限

不同的 Agent 可以用不同的"记忆集合"（例如 Coding Agent 只看项目规范、历史故障与已验证修复经验）。集合是账户级配置：

- 集合只声明**候选范围**（命名空间、场景、类型、标签、状态、是否含经验）；
- 调用 Agent **必须被显式绑定**，否则直接拒绝（fail closed）；
- 绑定不绕过原有 ACL：检索仍逐命名空间做实时授权，没权限的会出现在 `skipped_namespaces` 里，不会静默变成空结果；
- 解绑立即生效；删除集合只删绑定，**不删除任何记忆**。

对应工具：`memory_collection_upsert/list/delete/bind/unbind`；`memory_search` 与 `working_experience_search` 可用 `collection` 代替 `namespace`。细节见 [`docs/upgrade/ASSET_GOVERNANCE.md`](docs/upgrade/ASSET_GOVERNANCE.md)。

### 6. 几个经常出现的词

| 词 | 通俗解释 |
| :--- | :--- |
| `namespace`（命名空间） | 一座"资料室"，比如 `infra-team`。第一个往里写的 Agent 自动成为它的负责人 |
| `key` | 资料室里的文件橱标签。同一个 `namespace` + `key` 再写一次就是**修订**，版本号自动 +1 |
| `kind` | 这条记忆是什么类型：事实 / 偏好 / 事件 / 技能…… |
| `scene` | 用在什么场景：`chat` / `working` / `coding`，检索时按场景收敛范围 |
| `layer` | 认知层级，结构化知识 > 原始观测，打分时给结构化知识更高先验 |
| `confidence` | 置信度 0~1，越靠得住越优先 |
| `ttl_seconds` | 过期时间，到点自动归档（不是删除） |

---

## 五、检索是怎么工作的

检索这件事，可以想象成一个图书管理员：

1. **两条路一起找**：一条按**字面**找（中文全文索引，自带分词，不依赖外部分词库），一条按**意思**找（向量相似度，pgvector HNSW **halfvec 半精度 + 部分索引**，只索引 `active` / `archived`，体积约为 fp32 的一半）；
2. **过滤条件下推到存储层**：场景、层级、有效期、状态、标签这些条件在取候选时就过滤掉，不浪费打分；
3. **五维打分排序**：五项加权求和，公式见下；
4. **结果去重（MMR）**：相似内容不会一起挤满前几名，把名额让给真正不同的信息。ANN 一路走「瘦列契约」不回传向量文本，MMR 需要的相似度按 `top_k × 4` 候选池二次取回，省掉大字段搬运；
5. **坏了也不趴窝**：向量服务故障 → 自动退化成纯关键词检索；查询超时 → 保留已经拿到的那一路结果；两路都没结果才走游标列表兜底。

> 向量索引自 2026-10-27（迁移 015）起为 **halfvec 半精度 + 部分索引**：查询侧
> `embedding::halfvec(1024) <=>` 与索引表达式必须逐字一致，带过滤的 ANN 靠
> `hnsw.iterative_scan` 与 `hnsw.ef_search=100` 凑满召回；重建与观测见
> [`docs/pg-maintenance-runbook.md`](docs/pg-maintenance-runbook.md)。

打分公式（语义相似、关键词命中、置信度、新鲜度、层级先验五项加权求和）：

$$\text{Score} = w_{sim}\cdot\text{语义相似} + w_{kw}\cdot\text{关键词命中} + w_{conf}\cdot\text{置信度} + w_{rec}\cdot\text{新鲜度} + w_{prior}\cdot\text{层级先验}$$

两个容易讲错的地方：

- **关键词命中按"池内 IDF"加权**：稀有的词更有分量，`api` 不会莫名其妙命中 `rapid`；
- **新鲜度以"事件时间"为准**（不是更新时间），且按记忆类型区分衰减速度：偏好 180 天 / 技能 90 天 / 事实 60 天 / 事件 14 天 / 笔记 7 天；权重与时间常数都能用环境变量覆盖（`HUB_SCORE_WEIGHTS`、`HUB_RETRIEVAL_RECENCY_TAU_DAYS`），也支持按场景单独配一套权重。

### 没有可靠匹配，就说没有

检索有一条**默认拒绝阈值** `HUB_RETRIEVAL_MIN_SCORE`（默认 0.65）：低于这个分的候选一律丢弃。
一条都不剩时，工具返回 `{"results": [], "no_reliable_match": true}`——
**明确告诉调用方"没找到可靠的"**，而不是硬凑一条低分结果。

对长期记忆来说这两者的差别很大：域外问题硬凑一条结果，Agent 就会把它当事实依据，
轻则答非所问，重则把错误事实写进记忆。阈值也可以按场景单独配
（`HUB_SCENE_MIN_SCORE_PROFILES="chat=0.70;working=0.60"`），或由调用方显式传 `min_score` 覆盖。

> 阈值是在**伪向量**（纯词法）模态下标定的，换上真实嵌入模型后必须重新标定。
> 标定曲线怎么跑，见 [`docs/upgrade/EVALUATION.md`](docs/upgrade/EVALUATION.md)。

### 语义能力是否就绪，健康检查里说了算

伪向量嵌入器（`HUB_EMBED_PROVIDER=mock`）的余弦相似度是**噪声**，不是语义。
系统在启动时就会识别出来，并且：

- **不把向量维度混进打分**：该维度按"不可用"处理并对剩余维度重新归一化
  （配置权重**不动**，换上真实模型后该维度自动恢复参与，权重仍是配置值）；
- **在健康检查里如实声明**：`GET /readyz` 与 `GET /capabilities` 返回
  `semantic.ready` 与原因说明，不会把"词法检索"伪装成"语义检索"；
- **启动时打醒目告警**，避免"接口能返回结果 = 语义检索可用"的误判。

```mermaid
flowchart LR
    Q["查询：关键词 + 过滤条件"] --> FTS["按字面找<br/>中文全文索引"]
    Q --> VEC["按意思找<br/>向量相似度（halfvec 索引）"]
    FTS --> Score["五维打分融合"]
    VEC --> Score
    Score --> Dedup["结果去重 MMR"]
    Dedup --> Out["返回 Top-K"]
```

### 实测数据（评测基线 Bench v2）

评测集是**程序化生成、固定时钟、可复现**的：60 个主题、**480 条文档 / 600 条查询**，
覆盖精确命中、语义改写、时间演化、场景区分、环境边界与噪声负例，数据集指纹 `9f5b3b075a558371`。

```bash
# 跑检索评测（默认用确定性伪向量，只检测管道与排序逻辑回归）
cargo test --test eval_bench -- --nocapture
# 冻结/更新基线
HUB_EVAL_WRITE_BASELINE=1 cargo test --test eval_bench
# 消融实验（逐信号权重 / MMR λ / 新鲜度 τ / 场景偏好）
HUB_EVAL_ABLATIONS=1 cargo test --test eval_bench -- --ignored --nocapture
# 阈值标定：扫描 min_score，输出"域外拒绝对 / 域内召回"的权衡曲线
HUB_EVAL_CALIBRATE=1 cargo test --test eval_bench -- --ignored --nocapture
# 用真实嵌入服务评测（语义验收必须走这条）
HUB_EVAL_EMBED_ENDPOINT=https://…/v1/embeddings HUB_EVAL_EMBED_MODEL=bge-m3 \
  HUB_EVAL_REQUIRE_SEMANTIC=1 cargo test --test eval_bench -- --nocapture
```

当前冻结基线（**伪向量模态**，含默认拒绝阈值 `min_score=0.65`）：
Recall@1 68.15% / Recall@5 95.37% / Recall@10 97.22% / MRR@10 0.7989 / nDCG@10 0.6649，
硬负例违规率（Top-3）19.63%、**首位误取率 0.56%**、**域外误召回率 0.00%**，
进程内 p50 ≈ 10.1ms、p95 ≈ 28.4ms。

> **别被基线数字骗了**：这一轮用的是 Mock 伪向量，**语义维度在伪向量下是噪声**——
> 系统现在会**自动识别**伪向量嵌入器并把向量维度排除在打分之外（配置权重不变，
> 换上真实模型后该维度自动恢复参与），所以这些数字只证明"管道和词法排序没有回归"，
> **不能当作语义能力结论**。基线文件里 `semantic_ready: false` 就是这个标记。
> 接入真实嵌入模型后需要用 `HUB_EVAL_EMBED_*` 重跑并重新冻结基线。
> 完整的口径、分层指标与消融结果见 [`docs/upgrade/EVALUATION.md`](docs/upgrade/EVALUATION.md)。

### 门禁分三层：相对门禁不够用

| 层 | 判定依据 | 抓什么问题 |
| :--- | :--- | :--- |
| **相对门禁** | 与冻结基线逐查询配对比较（Δ > 1pt 且 \|z\| ≥ 2） | 指标**退化** |
| **绝对门禁** | 域外误召回率 ≤ 10%；首位误取率 ≤ 2% | 长期存在的缺陷（不随版本变化，相对门禁永远抓不到） |
| **业务正确性门禁** | 时间有效性 / 场景 / 标签 / 层级 / 环境边界**零违规** | 硬错误，与排序质量无关 |

"比上次没变差"不等于"够好"：一个本身就很弱的基线不发生退化时，测试照样通过。
所以域外误召回率、首位误取率与业务约束都设了**绝对**判定，不看基线。

质量门禁本身也做了防误报设计：基线里保存**每条查询的逐条结果**，回归判定按查询 ID 配对计算
差值 Δ 与显著性 z，要求"Δ 超过 1 个百分点 **且** |z| ≥ 2"才算退化；样本覆盖率不足或数据集指纹、
嵌入器不一致、**评测配置不一致**时直接判"不可比"，而不是稀里糊涂地比。

---

## 六、安全与可靠性

### 账户隔离与授权

- **账户即沙箱**：账户 A 与账户 B 就算建了同名命名空间，数据、授权、备份也完全隔离，越权检索有测试卡着 0 次；
- **标准 OAuth 2.1**：动态客户端注册（RFC 7591）、强制 PKCE S256、刷新令牌轮换与重放检测、受众绑定（RFC 8707）；
- **命名空间级授权**：谁的空间谁能读写，可以单独把某个空间授权给另一个 Agent，也能随时撤销（立即生效）；
- **三种权限范围**：`memory:read` / `memory:write` / `memory:admin`，每个工具都要声明自己需要哪一级。

### 认知记忆的权限边界

- **所有认知工具都校验命名空间 ACL**：Working 任务与经验、Coding 线程与修订、Chat 提案与意图，不再只查 token scope；
  同一账户内未获授权的命名空间一律拒绝；
- **集合不是权限**：`memory_collection_*` 只决定候选范围，读取仍走实时授权；未绑定或已解绑的 Agent 直接拒绝；
- **单命名空间请求 fail closed**：显式指定 `namespace` 时，无权限会返回错误；只有 `collection` 扇出才会把无权限命名空间放进
  `skipped_namespaces` 并在响应里说明。

### 记忆整合的两种候选发现模式（A / B）

整合中枢负责把"多 Agent 写重了"的记忆收拾干净。它有两种**候选发现**模式，
但**共用同一套判定与合并逻辑**——模式只影响"从哪找候选"，不影响"能不能合"。

| 模式 | 配置 | 候选从哪来 | 嵌入调用 | 记忆内容出站 |
| :--- | :--- | :--- | :--- | :--- |
| **B 纯本地规则**（默认） | `HUB_CONSOLIDATION_MODE=rules` | 规范化指纹、结构化字段（业务键/场景/标签/环境）、本地词法索引 | **恒为 0** | **否** |
| **A 向量辅助** | `HUB_CONSOLIDATION_MODE=hybrid` | B 的候选 **∪** 嵌入向量召回的语义候选 | 按需、有界、可计数 | 视嵌入端点而定 |

- **默认是 B**：即使配了 `HUB_EMBED_PROVIDER=http`，整合也不会自动调用向量服务——
  要不要把记忆内容发出去必须由管理员显式决定；
- **A 只是"向量帮你找候选"**：余弦相似度高只能说明"这两条值得一起看"，
  说不了"这两条相同"。`PostgreSQL 15` 和 `PostgreSQL 16` 语义很像，但它们是不同事实，
  绝不能合。**只有原文完全相同、或保守规范化后完全相同**的记忆才允许自动合并，
  语义近重复一律只出待审提案；
- **规范化是保守的**：只折叠空白、换行与 Unicode 兼容形式；
  大小写、否定词、数字、单位、日期、版本号、路径、URL 一律保留
  （"不是 X" 与 "X"、"v1" 与 "v2" 绝不会落到同一个指纹）；
- **降级是显式可见的**：`HUB_CONSOLIDATION_FALLBACK=rules` 时，向量不可用就降级为规则模式，
  响应里如实写明 `requested_mode=hybrid` / `effective_mode=rules` / `fallback_applied=true`，
  不会把降级结果伪装成成功的向量整合；`=error` 则直接失败，不执行任何整合；
- **整合与检索互不干扰**：整合模式不改变 `memory_search` 的检索模式、分数与 MMR；
  整合模式 B 也**不等于**"全系统离线"（检索与认知提炼是否调用模型是另一回事）；
- 健康检查里 `semantic`（检索语义能力）与 `consolidation`（整合模式）是**分开报**的。

```bash
# 默认就是 B：不发任何嵌入请求
HUB_CONSOLIDATION_MODE=rules

# 想用 A：先只预演，观察候选质量
HUB_CONSOLIDATION_MODE=hybrid
HUB_EMBED_PROVIDER=http
HUB_EMBED_ENDPOINT=https://embed.example.com/v1/embeddings
HUB_EMBED_MODEL=bge-m3
```

细节见 [`docs/upgrade/CONSOLIDATION_AB.md`](docs/upgrade/CONSOLIDATION_AB.md)。

### 备份与恢复

- **只读快照**：单个命名空间或整个账户（`*`）都能打快照，带 SHA-256 校验和，建完就改不了；
- **三种恢复姿势**：`dry_run` 先预演看看会变成什么样；`merge` 增量并入（推荐）；`replace` 整体替换——**替换前会强制先给当前数据打一份安全备份**，防止误操作把库清空；
- **恢复前先验校验和**，对不上直接拒绝恢复。

---

## 七、MCP 工具清单（38 个）

`memory_*` 18 个是基础盘，`cognitive_*` / `working_*` / `coding_*` 15 个是认知增强，`memory_collection_*` 5 个是集合治理。

### 1. 基础存取（4 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `memory_put` | `memory:write` | 写入或修订（同 `namespace` + `key` 自动版本 +1） |
| `memory_get` | `memory:read` | 按 ID 或业务键取一条 |
| `memory_update` | `memory:write` | 改部分字段（内容 / 标签 / 元数据 / 置信度 / TTL） |
| `memory_delete` | `memory:write` | 软删除：归档但保留全部历史 |

### 2. 检索与统计（4 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `memory_search` | `memory:read` | 混合检索。可传 `scene`、`layer`、`valid_at`、`include_superseded`、`min_score` |
| `memory_list` | `memory:read` | 按条件列举，游标分页 |
| `memory_stats` | `memory:read` | 命名空间统计（总量、状态、类型分布、24 小时内到期数） |
| `memory_history` | `memory:read` | 一条记忆的完整版本变更史 |

### 3. 整合与提案治理（5 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `memory_consolidate` | `memory:write` | 一键整理：查重、查冲突、评陈旧度、按策略合并。可选 `mode` 参数切换候选发现模式（默认禁止逐请求覆盖） |
| `memory_consolidation_stats` | `memory:read` | 整合健康度（重复组数、冲突组数、陈旧条目、冗余率、健康分） |
| `memory_integrate_scan` | `memory:write` | 扫描并生成待审提案（已有 open 提案的组会自动跳过）。可选 `mode` 参数，语义同上 |
| `memory_integration_list` | `memory:read` | 列出待处理提案 |
| `memory_integration_resolve` | `memory:write` | 裁决提案：`merge` 合并保留溯源 / `dismiss` 保持分立 |

### 4. 备份与恢复（3 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `memory_backup_create` | `memory:admin` | 打不可变快照（某命名空间或 `*` 全量），生成 SHA-256 校验和 |
| `memory_backup_list` | `memory:admin` | 列出快照与体积 |
| `memory_restore` | `memory:admin` | 恢复：`dry_run` / `merge` / `replace`（replace 前自动安全备份） |

### 5. 权限管理（2 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `memory_grant` | `memory:admin` | 把自己的命名空间授权给别的 Agent |
| `memory_revoke` | `memory:admin` | 撤销授权，立即生效 |

### 6. 认知增强与治理（20 个）

| 工具 | 权限 | 干什么 |
| :--- | :--- | :--- |
| `cognitive_chat_propose` | `memory:write` | 把多轮对话丢进来做认知提炼（候选事实、实体关联、整合提案） |
| `cognitive_proposal_resolve` | `memory:write` | 审核认知提案：采纳 / 驳回 / 编辑后合并 |
| `cognitive_intentions_list` | `memory:read` | 列出当前意图与候选提案（看有没有过期意图） |
| `working_task_start` | `memory:write` | 开一项工作流任务，可关联证据 |
| `working_task_finish` | `memory:write` | 结束任务，记录结果摘要或失败原因 |
| `working_experience_distill` | `memory:write` | 把任务沉淀成**候选**经验（含前置条件、边界环境、已知失败模式、验证方法） |
| `working_experience_feedback` | `memory:write` | 给经验打分反馈（+1 有效 / -1 踩坑）；只调反馈分，不改状态 |
| `working_experience_search` | `memory:read` | 检索可复用经验，带当前环境做边界匹配，不匹配会告警降权；可限定状态或按集合检索 |
| `working_experience_transition` | `memory:write` | 状态流转：`verify`（需满足来源与验证门槛）/ `reject` / `deprecate` / `restore` |
| `working_experience_history` | `memory:read` | 读取经验版本历史（只追加快照） |
| `coding_thread_create` | `memory:write` | 建一条代码演进/重构逻辑线程 |
| `coding_thread_list` | `memory:read` | 列出代码演进线程 |
| `coding_revision_record` | `memory:write` | 记录一次代码修订（真实 diff、变更类型、安全相对路径、符号指纹） |
| `coding_revision_list` | `memory:read` | 按线程或文件列出修订，重建演化与回滚链 |
| `coding_revision_get` | `memory:read` | 读取单条修订快照 |
| `memory_collection_upsert` | `memory:admin` | 新建/更新 Agent 记忆集合（只收窄候选范围，不授权） |
| `memory_collection_list` | `memory:admin` | 列出账户的记忆集合 |
| `memory_collection_delete` | `memory:admin` | 删除集合（级联删除绑定，不删记忆） |
| `memory_collection_bind` | `memory:admin` | 绑定集合到账户内 Agent |
| `memory_collection_unbind` | `memory:admin` | 解绑（立即生效，不删记忆） |

---

## 八、项目结构

标准 Rust Workspace，按职责分层：

```text
.
├── Cargo.toml                  # Workspace 与统一依赖
├── Dockerfile                  # 多阶段构建的生产镜像
├── docker-compose.yml          # 一键编排（DB + 三个服务）
├── docker-compose.lite.yml     # Lite 形态编排（三进程，定时作业内嵌 mcp-server）
├── .cnb.yml                    # CNB 云构建：PR / 分支 push 跑质量门禁；main push 出镜像并部署；tag push 出定版镜像；tag 部署触发器；分支页手动按钮事件
├── .cnb/tag_deploy.yml         # CNB 自定义部署环境：Tag 详情页「部署」选 production
├── .cnb/settings.yml           # CNB 页面 UI 定制：NPC 角色（记忆管家 / 审查员）、按钮文案与悬浮图
├── .cnb/web_trigger.yml        # CNB 分支详情页自定义按钮：部署生产 / 回滚上一版本 / 仅构建镜像 / 问问记忆管家
├── .github/workflows/ci.yml    # GitHub Actions：质量门禁 / PG 集成 / 依赖审计 / Release 构建
├── scripts/push-cnb.sh         # 双平台推送：以 CNB 身份重放并推送镜像（patch-id 口径）
├── scripts/ci/deploy-production.sh  # 生产部署阶段（切镜像 / 探活 / 失败回滚），CNB 三条流水线共用
├── scripts/ci/rollback-production.sh  # 生产回滚阶段：切回远端 previous_release.env 的镜像并探活
├── docs/                       # 设计与规范文档（见第十一节）
└── crates/
    ├── common/                 # 公共部件：配置强校验、脱敏日志、限流、Cron、整合模式枚举
    ├── mem-core/               # 领域核心：记忆与备份模型、权限、认知领域服务、内容指纹
    ├── consolidation/          # 整合中枢：候选发现（规则 / 向量）、关系分类、分级融合、自动调度
    ├── store/                  # 存储：PostgreSQL 迁移 + 纯内存双后端
    ├── embed/                  # 嵌入：OpenAI 兼容 HTTP 客户端 + 确定性 Mock
    ├── retrieval/              # 检索：中文分词、双路召回、五维打分、MMR
    ├── authz/                  # 鉴权：HS256 验签、受众校验、受保护资源元数据
    ├── auth-core/              # OAuth 原语：PKCE S256、令牌家族轮换、Argon2
    ├── auth-web/               # 授权服务：DCR、OAuth 端点、账户控制台
    ├── mcp/                    # MCP 协议层：38 个工具 Schema（tools/ 按域拆分）、HTTP / SSE / stdio 传输（transport/）、生态适配拦截层（adapter/）
    ├── worker/                 # 后台任务：定时快照、过期清理、离线扫描；embedded 内嵌调度（两进程部署复用同一份作业）
    └── bins/                   # 三个可独立部署的二进制
        ├── mcp-server/
        ├── auth-server/
        └── backup-worker/
```

---

## 九、开发、测试与质量门禁

```bash
# 完整测试套件（默认特性；含契约快照、治理回归与轻量化用例）
cargo test --workspace

# 只跑治理回归（生命周期 / ACL / 集合 / Coding 演化链）
cargo test -p mem-core --test asset_governance

# 静态检查（零警告门禁）
cargo clippy --workspace --all-targets -- -D warnings

# 格式检查
cargo fmt --all -- --check

# 检索评测基线（排序质量；不得因本次改动退化）
cargo test --test eval_bench -- --nocapture

# 整合评测（整合质量：错误合并数、硬约束违规数、B 模式 embedding 调用数必须为 0）
cargo test --test consolidation_eval -- --nocapture

# 真实 PostgreSQL 集成测试（需要 DATABASE_URL 指向测试库）
cargo test -p store --features pg-integration -- --ignored
```

CI 分两条；平台专属的流水线细节以配置为准，这里只记口径：

- **GitHub Actions**（`.github/workflows/ci.yml`）：质量门禁（fmt / clippy / test / 文档构建，与本地同一脚本）、
  PostgreSQL + pgvector 真实库集成测试（串行）、依赖准入与安全审计（cargo-deny + cargo-audit）、Release 构建三个二进制。
- **CNB 云构建**（`.cnb.yml`）：PR 与分支 push 跑同一套质量门禁；`main` push 构建镜像并部署（探活 `/readyz`，失败自动回滚）；
  tag push 出定版镜像，可在 Tag 详情页手动部署。分支页一键按钮、NPC 助手、云原生预览环境属 CNB 专属能力，
  细节都在 `.cnb/` 配置里（`web_trigger.yml` / `settings.yml` / `tag_deploy.yml`），README 不展开。

### 回滚（应用版本 / 部署形态）

Lite 与 Production 共用同一 schema、同一备份格式，回滚只涉及二进制/镜像，不需要数据库回滚。
迁移 015 只重建向量索引并收紧表级 autovacuum、不改表结构，回滚到早于它的二进制功能照常，
只是 fp32 向量查询不再命中索引、退化为顺序扫描，按
[`docs/pg-maintenance-runbook.md`](docs/pg-maintenance-runbook.md) 可随时恢复。

```bash
# 1. 应用版本回滚：回到上一个稳定 tag/commit，重新构建并重启
git checkout <上一个稳定 tag 或 commit>
docker compose up -d --build                              # Production
docker compose -f docker-compose.lite.yml up -d --build    # Lite

# 2. 只回滚\"部署形态\"：把 HUB_DEPLOYMENT_PROFILE 改回 production 并重启，
#    数据、账户、权限、备份全部不动（profile 只影响默认值）
```

备份/恢复的验证方式（`dry_run` 必须零写入、校验和不符必须拒绝恢复、
`replace` 前必须自动生成安全备份）见
[`docs/upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md`](docs/upgrade/LIGHTWEIGHT_EXECUTION_REPORT.md)。

> [!NOTE]
> **路径里带空格的坑**：如果代码检出在含空格的目录（比如某些外挂盘），请用 `cargo test --workspace`，
> **不要**加 `--all-features`——可选特性 `tikv-jemalloc-sys` 的构建脚本不支持带空格的前缀路径；
> 默认内存分配器在各平台上完全够用。详见 [`docs/upgrade/REPOSITORY_AUDIT.md`](docs/upgrade/REPOSITORY_AUDIT.md)。

---

## 十、已知限制与下一步

写在这里是为了不让人踩坑，而不是自夸：

1. **生产必须接真实嵌入模型**。`HUB_EMBED_PROVIDER=mock` 的伪向量只是让开发/测试可复现，语义能力为零；
   接了 `http` 提供方之后要按评测流程重新冻结基线。
   语义能力是否就绪**在健康检查里如实暴露**：`GET /readyz` 与 `GET /capabilities`
   返回 `semantic.ready` 与原因说明，不会把"词法检索"伪装成"语义检索"。
2. **默认拒绝阈值是在伪向量下标定的**。`HUB_RETRIEVAL_MIN_SCORE` 默认 0.65，
   作用是把域外误召回率压到 0%（原来 100%），代价是 Top-3 硬负例占比偏高
   （19.63%，但**首位**误取率仅 0.56%）。换上真实嵌入模型后**必须重新标定**：
   跑 `HUB_EVAL_CALIBRATE=1` 拿新曲线再定值。
3. **阈值仍需外部标注**：冲突/重复判定的阈值、以及 Chat 认知提炼的真实 LLM 抽取器，都还缺真实资源来校准。
4. **`temporal` / `paraphrase` / `env` 三类查询是当前短板**，需要真实嵌入模型介入后复测排序质量。
5. **证据不可变 ≠ 写前不能脱敏**：凭证扫描、敏感片段掩码、最小化摘要与保留期限都在**写入前/到期清理**
   两道关卡上完成，详见 [`docs/upgrade/CHAT_MEMORY.md`](docs/upgrade/CHAT_MEMORY.md)。
6. **整合默认不再使用向量**：`HUB_CONSOLIDATION_MODE` 默认 `rules`（纯本地规则整合），
   即使配了真实嵌入服务也不会自动启用向量整合；A 模式的语义增益需真实模型 +
   `cargo test --test consolidation_eval` 才能验收，详见
   [`docs/upgrade/CONSOLIDATION_AB.md`](docs/upgrade/CONSOLIDATION_AB.md)。
7. **认知异步作业尚未装配**：`ChatCognitiveJob`（会话提炼重试）与 `ExperienceDistillJob`（经验提炼）
   已实现并有测试，但**没有接进任何二进制的调度**——真实 LLM 抽取器尚未接入，
   接进去只会用 Mock 抽取器产出占位数据。等真实抽取器落地后再装配。
8. **定时快照与手工备份统一为 v6 载荷**：`backup-worker` 的周期快照与 `memory_backup_create` 使用同一格式
   （条目、**记忆版本历史**、证据、关系、任务、经验、经验版本历史、代码线程）；恢复路径支持 v1–v6，
   但**回滚后的旧二进制不能恢复 v6 备份**——降级前请保留新二进制或先完成恢复。
9. **多副本只保证\"不同副本重复执行\"被租约挡住**（`HUB_JOB_LEASE_H`），
   不保证单副本内部的扫描进度可续传；大命名空间请用 `HUB_INTEGRATION_SCAN_MAX_ENTRIES` 分批。
10. **迁移 015 的向量索引重建适合空表 / 小表**：`create index`（非 `CONCURRENTLY`）在启动迁移事务内执行，
    大表升级请在维护窗口按 [`docs/pg-maintenance-runbook.md`](docs/pg-maintenance-runbook.md) 手动重建。
11. **经验提炼与反馈的语义已变更**：`working_experience_distill` 只产出 `candidate`；`working_experience_feedback`
    只调反馈分、不再接受 `status` 参数。需要"立即可复用"的调用方应显式调用
    `working_experience_transition(action="verify")`，并满足来源与验证门槛。

---

## 十一、文档导航

| 想了解 | 看这个 |
| :--- | :--- |
| 文档总入口、演进史、阅读路径 | [`docs/README.md`](docs/README.md) |
| 系统总体设计规格与工具契约 | [`docs/设计-多智能体长期记忆服务.md`](docs/设计-多智能体长期记忆服务.md) |
| 认知升级总纲（P0–P5 实施要求） | [`docs/Memory_Hub_Cognitive_Upgrade_Agent_Brief.md`](docs/Memory_Hub_Cognitive_Upgrade_Agent_Brief.md) |
| 对话记忆四层模型 | [`docs/upgrade/CHAT_MEMORY.md`](docs/upgrade/CHAT_MEMORY.md) |
| 经验记忆与环境边界（生命周期以资产治理文档为准） | [`docs/upgrade/WORKING_MEMORY.md`](docs/upgrade/WORKING_MEMORY.md) |
| 经验资产生命周期、版本历史、Agent 记忆集合与 ACL 语义 | [`docs/upgrade/ASSET_GOVERNANCE.md`](docs/upgrade/ASSET_GOVERNANCE.md) |
| 个人轻量版 / 完整生产版双部署与模式切换 | [`docs/upgrade/DEPLOYMENT_MODES.md`](docs/upgrade/DEPLOYMENT_MODES.md) |
| 治理升级的源码审计与安全基线 | [`docs/upgrade/GOVERNANCE_AUDIT.md`](docs/upgrade/GOVERNANCE_AUDIT.md) |
| 治理升级发布说明、迁移与回滚 | [`docs/upgrade/GOVERNANCE_RELEASE.md`](docs/upgrade/GOVERNANCE_RELEASE.md) |
| 本轮任务书原文与逐章完成情况 | [`docs/Memory-Hub-认知记忆与知识资产治理升级-任务书与完成情况.md`](docs/Memory-Hub-认知记忆与知识资产治理升级-任务书与完成情况.md) |
| 代码演进链与真实 Diff | [`docs/upgrade/CODING_MEMORY.md`](docs/upgrade/CODING_MEMORY.md) |
| 检索评测、基线与消融 | [`docs/upgrade/EVALUATION.md`](docs/upgrade/EVALUATION.md) |
| 记忆整合 A/B 双模式（候选发现、合并门槛、评测） | [`docs/upgrade/CONSOLIDATION_AB.md`](docs/upgrade/CONSOLIDATION_AB.md) |
| PostgreSQL / pgvector 维护（halfvec 索引、ef_search、观测与重建） | [`docs/pg-maintenance-runbook.md`](docs/pg-maintenance-runbook.md) |
| 双平台推送流程（GitHub + CNB 身份隔离与重放） | [`docs/upgrade/PUSH_PROCESS.md`](docs/upgrade/PUSH_PROCESS.md) |
| 质量与安全基线 | [`docs/upgrade/BASELINE.md`](docs/upgrade/BASELINE.md) |
| 仓库与构建环境审计 | [`docs/upgrade/REPOSITORY_AUDIT.md`](docs/upgrade/REPOSITORY_AUDIT.md) |
| 业务系统零侵入接入方案 | [`docs/创界山智能接入层实施方案.md`](docs/创界山智能接入层实施方案.md) |

> **文档语言**：`docs/` 下的专题文档目前**仅提供中文版**；本 README 提供中英双版本
> （[English](README.en.md)），其余文档可按需补充。

---

## 📄 License

MIT OR Apache-2.0 双协议，任选其一：[MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE)。
