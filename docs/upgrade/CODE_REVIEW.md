# Memory Hub 全量代码审校报告（2026-10）

> **审查范围**：全部 15 个 crate、约 5.4 万行 Rust 源码（store / retrieval / embed / mem-core / consolidation / worker / auth-core / auth-web / authz / mcp / common / bins 三入口），外加数据库迁移脚本与 CI 配置。
> **审查方法**：三路并行深度审查（存储与检索层、领域核心与整合层、认证与 Web/MCP 协议层），重点文件逐行阅读，反模式全仓扫描（unsafe / unwrap / 锁跨 await / format! 拼 SQL / secret 打印）。
> **审查基线**：主分支当前 HEAD；README「已知限制」中自述的问题均已复核确认。
> **文档性质**：归档报告（Reference）—— 记录审查时点的发现，修复后请在本表勾销。

---

## 0. 总体评价

**项目基础素质高于平均水平。** 以下底线均已守住，后续审查无需重复劳动：

| 维度 | 结论 |
| :--- | :--- |
| SQL 注入 | 无。用户可控值全部参数绑定，无一处 `format!` 拼 SQL |
| 锁纪律 | 内存后端锁从不跨 `await` 持有 |
| unsafe | 全部审查 crate `#![forbid(unsafe_code)]` |
| 常量时间比对 | PKCE verifier、HMAC 验签、邀请码、legacy 摘要四处均用 `subtle::ConstantTimeEq` |
| XSS | 账户控制台 HTML 全量 `esc()` 转义（五个实体），页面插值无一遗漏 |
| OAuth 2.1 | PKCE 强制 S256、授权码一次性消费先于 PKCE 校验（符合 RFC 7636）、刷新令牌重放联级吊销有完整测试链、`alg=none` 与算法混淆拒绝、redirect_uri 注册期白名单（含 userinfo/前缀绕过防御） |
| 密码学参数 | Argon2id 64MiB / t=3（OWASP 推荐量级）；占位密钥启动 fail-fast |
| 账户隔离 | 跨账户沙箱有测试锚定（越权次数必须为 0） |
| 时间处理 | 全程 UTC、时钟可注入、无 `Local` 依赖 |
| 评测纪律 | 整合引擎有 `scan_limit` 防护、租约跳过不推进游标、截断如实上报 |

**但审查发现约 60 项可优化点**，按严重程度分布：严重（正确性/安全）9 项、可靠性 9 项、性能 8 项、可维护性与轻微项约 34 项。问题集中在三条链路上（见下节）。

### 审查确认属实的 README 已知问题

README「已知限制」中自述的以下问题经复核**确认属实**，且部分比 README 描述更深：

1. **定时备份 v1 载荷**（`crates/worker/src/backup_job.rs:77-94`）——确认；且 prune 的 TTL 淘汰可能保留 v1 定时快照而删掉唯一 v4 手工备份。
2. **认知异步作业未装配**——确认；但缺口比 README 描述更深：不只是"没接进调度"，**认知提案的持久化层本身就缺失**（见危险链路一）。

---

## 1. 三条危险链路（最优先）

### 链路一：认知治理链路实际不可用

`process_chat_session` 生成的 `CognitiveProposalRecord` 只存在于返回值中（`crates/mem-core/src/chat_cognitive_service.rs:200-306`），`MemoryHubStore` 没有任何保存/查询认知提案的方法。而 MCP 工具 `cognitive_proposal_resolve` 实际路由到 `self.integration.resolve`（整合提案存储，`crates/mcp/src/tools.rs:1030-1066`）——把 `cognitive_chat_propose` 返回的提案 ID 传进去必然 NotFound，两套提案 ID 空间混用。

默认治理模式是 `Manual`（候选必须生成待审提案），意味着**默认配置下所有认知提炼结果都会丢失**，"人工审查"这条默认路径实际不可用。

### 链路二：双后端语义分叉，内存测试的公信力被打穿

| 操作 | PostgreSQL 行为 | 内存后端行为 | 后果 |
| :--- | :--- | :--- | :--- |
| `save()` 对不存在 id | 静默 no-op 返回 `Ok`（受影响行数被丢弃，`postgres.rs:989-993`） | 无条件 upsert 新建条目（`in_memory.rs:248-254`） | PG 上静默丢写；内存测试永远验不出 |
| `revise()` 版本不符 | `Conflict`（`postgres.rs:1016-1021`） | `NotFound`（`in_memory.rs:93`） | 错误码契约不一致 |
| `similar()` 距离 | 余弦距离；维度不匹配直接报错（`postgres.rs:1107-1126`） | 点积（未归一化向量排序不同）；维度不匹配静默返回空（`in_memory.rs:382-391`） | 换嵌入模型维度后内存环境"看起来正常"，生产直接炸 |
| `insert()` 同 (ns, key) 活跃冲突 | `23505 → Conflict` | 只查 `(ns, id)` 主键，不报冲突（`in_memory.rs:181-189`） | 绕过 service 直写 store 的路径在内存后端验不出键冲突 |

### 链路三：限流体系有四个洞

1. **XFF 完全可信**：`extract_client_ip` 取 `X-Forwarded-For` 第一个值且无可信代理配置（`crates/auth-web/src/routes.rs:649-662`）。攻击者每请求换一个随机 XFF 即可绕过登录 IP 限流（15/min），只剩用户名维度 5/min——对少量已知账户仍是每天 7200 次/账户的口令尝试空间。无代理部署时回退 `127.0.0.1`，全部直连用户共享同一个限流桶（正常用户互相触发 429）。
2. **`/oauth/token` 完全无限流**（`routes.rs:410-425`），且每次验证跑 64MiB 内存硬度的 Argon2id——未认证的 CPU/内存 DoS 入口。`/account/register`（开放注册模式下）同样无限流。
3. **JSON-RPC batch 绕过按 Agent 限流**：整个 POST 只 `check` 一次（`crates/mcp/src/transport.rs:741-749`），batch 循环顺序执行任意多条且无条数上限（`:765-796`）。默认 256KB 请求上限下，一批可塞约 2500 条 `tools/call`，每条都可触发检索/整合等昂贵操作——直接击穿"单个 Agent 的突发不得影响其他 Agent"的限流设计目标。
4. **内存令牌结构只增不删**：授权码（`oauth_flow.rs:512-524`）、刷新令牌族（`token_family.rs:58-137`）、确认令牌（`confirm.rs:35-44`）三个 HashMap 无过期清理——无界内存增长（纯 HTTP 可触发）+ 重启后所有已授权 Agent 同时被登出。

---

## 2. P0 — 正确性 / 安全（9 项，必修）

| # | 问题 | 位置 | 修复方向 |
|---|------|------|----------|
| 1 | 迁移执行器 check-then-act 无并发保护：多副本同时启动会都认为迁移未执行而重复跑 DDL，后提交者撞 `_hub_migrations` 主键被映射成 `Conflict`，`connect()` 直接失败，服务起不来 | `crates/store/src/postgres.rs:429-460` | 进入迁移前 `pg_advisory_lock(常量哈希)`，结束 unlock |
| 2 | `put` 的 key-upsert 分支丢弃请求中的 `scene`/`layer`/`valid_from`/`valid_until`/`occurred_at`——调用方以为更新了认知分层或有效期，实际静默不生效；新建路径却应用了它们 | `crates/mem-core/src/memory_service.rs:274-309` | upsert 分支补齐五个字段赋值 + 回归测试 |
| 3 | `contains_sensitive_credentials` 子串匹配：`"task-"`/`"risk-"`/`"desk-"` 都含 `"sk-"`，普通对话（"完成 task-1"）或 diff 被整条拒收；`"password="` 会拦下合法配置讨论。该函数是候选校验、经验校验、diff 校验三道闸门的共用底座 | `crates/mem-core/src/domain.rs:625-675` | 结构化前缀改正则（前缀 + 16 位以上 token 字符）；`password=` 类要求值像真凭证（长度/熵）；保留白名单通道 |
| 4 | `ttl_seconds` 无上界校验：`chrono::Duration::seconds(i64::MAX)` 溢出 panic，一次携带极大 ttl 的请求可崩进程（DoS 面） | `memory_service.rs:286,329` | `validate_new` 校验 ttl ∈ [0, 10 年]，超限返回 `InvalidArgument` |
| 5 | 认知提案不持久化 + `cognitive_proposal_resolve` 路由错位（链路一全文） | `chat_cognitive_service.rs:200-306`、`mcp/tools.rs:1030-1066` | 认知提案落库（复用/泛化 IntegrationStore），resolve 按提案类型分发 |
| 6 | `detect_supersedes_target` 硬编码 "MySQL→PostgreSQL" 测试启发式：任何同时提及这两个词的无关条目对都被判为取代关系，旧事实被错误改写 `valid_until`；真正的其他迁移（Redis→Memcached 等）识别不到。且 `existing.iter().rev()` 取"最后一个"，`all_in_namespace` 无排序保证，目标选择不确定 | `chat_cognitive_service.rs:499-512` | 删除硬编码对，只保留显式 `target_memory_id` 与标签交集判定；目标按 `occurred_at`/`updated_at` 确定排序 |
| 7 | `save()`/`revise()` 双后端分叉（链路二全文） | `postgres.rs:989-1021` vs `in_memory.rs:93,248-254` | 统一语义（PG `save()` 0 行返回 `NotFound`，或两边对齐为 upsert），写进 trait 文档 |
| 8 | 限流四洞（链路三全文） | `routes.rs:649-662,410-425`、`transport.rs:741-796` | `ConnectInfo<SocketAddr>` 取真实对端（可选 `TRUSTED_PROXIES`）；token 端点按 IP + client_id 双维限流；batch 每条 `tools/call` 单独扣令牌 + 条数上限（如 32） |
| 9 | 行映射普遍 `try_get(...).ok().flatten()`：`key`/`embedding_model`/`scene`/`valid_from`/`expires_at`/`merged_into` 等十余字段全部 `.ok()` 兜底——SELECT 列清单一旦与 SQL 漂移（19 列清单手抄了 7 次），缺列不报错而是静默变 `None`，等于把 schema 漂移转化为静默数据丢失 | `postgres.rs:536-578` 及全部 `*_from_row` | 可空列改强类型 `Option<T>` 解码并 `?` 传播；列清单收敛为常量 |

## 3. P1 — 可靠性（9 项）

| # | 问题 | 位置 | 修复方向 |
|---|------|------|----------|
| 10 | 内存授权码/刷新令牌族/确认令牌只增不删：无界内存增长 + 重启全失效 | `oauth_flow.rs:512-524`、`token_family.rs:58-137`、`confirm.rs:35-44` | 定期清扫（tokio interval 或交给 backup-worker）+ insert 时惰性淘汰（参考 `InMemoryRateLimitStore` 的 10000 条阈值做法） |
| 11 | HttpEmbedder 无重试、固定 10s 超时、错误分支无界读 body：瞬时 5xx/超时后该条目**永久**无 embedding（写入路径静默降级，之后不补算） | `crates/embed/src/http.rs:55-124` | 对 5xx/超时加 1-2 次带抖动的重试；错误 body 限制读取长度；可考虑异步补嵌入任务 |
| 12 | 迁移 005 回填 `tsv_tokens = content`：`tsv_tokens` 的设计是存放 Rust 侧预分词流，回填却写原文——`to_tsvector('simple', ...)` 对无空格中文整句当一个词元，**存量中文行 FTS 实际失效**，直到该行下次被更新 | `crates/store/migrations/20261013000005_cjk_bigram_fts.sql` | 新迁移分批读出 → Rust 重分词 → 回写（或至少文档明确该已知缺陷） |
| 13 | Argon2id（64MiB/t=3）同步跑在 async 上下文：一次验证几十毫秒纯 CPU 直接阻塞 tokio worker，认证高峰拖住整个 runtime | `postgres.rs:1747-1776`、`secret.rs:18-21` | `tokio::task::spawn_blocking` 包裹 verify/hash |
| 14 | 恢复流程系统性 `let _ =` 吞错：证据/关系/任务/经验/代码线程恢复全部不检查返回值，单条失败完全不可见（连日志都没有），一次"成功"的恢复可能悄悄丢一半数据 | `crates/mem-core/src/backup_service.rs:657-820` | 失败计数入 `RestoreReport`（如 `failed_relations`）+ `tracing::warn`；关键实体失败考虑中止 |
| 15 | supersedes 封堵旧事实的 `revise` 失败静默（`let _ =`）：旧事实继续有效被召回，与新事实并存矛盾——恰是 supersedes 机制要防止的局面。合并时 Supersedes 关系建立失败同样静默，破坏溯源链 | `chat_cognitive_service.rs:461-464`、`consolidation/src/engine.rs:746-758` | 失败 `warn!` 带实体 ID，或上抛让整次提交可见 |
| 16 | Hybrid 整合 LSH 分桶忽略 `embed_missing` 新算的向量（overrides 未传入 `lsh_buckets`）：超过 256 条走 LSH 时，刚花了嵌入调用算出的向量在候选生成里完全没用上——大规模场景语义候选失效，与重复带（用 `vector_of` 尊重 overrides）行为不一致 | `crates/consolidation/src/candidates/hybrid.rs:345-371` | `lsh_buckets` 接收 overrides，或 embed 后写回本地 entries 副本 |
| 17 | 定时备份仍是 v1 载荷（缺证据/关系/任务/经验/代码线程），校验和算法也与 v4 不同；prune 可能删掉唯一 v4 备份保留 v1 | `crates/worker/src/backup_job.rs:77-94` | `BackupJob` 改为委托 `DefaultBackupService::create`（系统特权 AuthCtx）统一 v4 载荷 |
| 18 | `cognitive_proposal_resolve` 的 `modified` 决策非原子：`resolve`（标记提案 merged）成功后 `memory.update` 失败则提案状态不可逆，用户自定义 `custom_content` 丢失，重试得到"提案已裁决" | `crates/mcp/src/tools.rs:1068-1095` | 先校验/修订内容再 resolve，或把自定义内容并入 `ResolveRequest` 一次性完成 |

## 4. P2 — 性能（8 项）

| # | 问题 | 位置 | 修复方向 |
|---|------|------|----------|
| 19 | 所有读路径恒拉 embedding 文本（每行约 4KB）：`ENTRY_COLUMNS` 含 `embedding::text`，`list()` 100 条 ≈ 400KB 传输 + 逐 float 字符串解析（两次分配），而 list/count 场景几乎从不需要向量。**审查中性能收益最大的单项** | `postgres.rs:841-844` | 双列清单（瘦版/全量版），list/count/备份摘要走瘦版，`similar` 保留全量版 |
| 20 | `stats` 全量加载命名空间所有条目（含 merged/archived）到内存只为分类计数，存储层已有 `count(filter)` 却没用 | `memory_service.rs:528` | 按状态/类型 `count(filter)` 聚合；`expiring_24h` 下推 `expires_at` 范围条件 |
| 21 | 28 个工具定义每次 `tools/call` 用 `json!` 宏全量重建（上百个 serde_json Value 节点）只为线性查一个 `required_scope` 字符串；`tools/list` 同样全量重建 | `mcp/src/schema.rs:42-997`、`transport.rs:111-121` | `OnceLock<Vec<ToolDefinition>>` 缓存 + `OnceLock<HashMap<&str, Scope>>` 常数查找 |
| 22 | `process_chat_session` 每次 `all_in_namespace` 全量加载（含全部内容字段）做线性查重，无上限；`review_proposal` 同样全量加载。整合引擎有 `scan_limit` 防护，这里没有 | `chat_cognitive_service.rs:192-198,335` | 查重下推存储层（内容指纹/哈希索引），或复用整合候选机制；至少加装载上限 |
| 23 | Hybrid 重复带 O(n²) 全量两两余弦比对（模块头注释宣称"超过 256 走 LSH 避免爆炸"，但 LSH 只用于冲突带）；`scan_limit=0`（默认构造）且已存向量条目多时，几千条即千万级点积 | `candidates/hybrid.rs:227-258` | 重复带同样走 LSH/倒排召回，或强制 scan_limit |
| 24 | 冲突组分歧度对大组 O(k²) 次重复分词：两两调用 `content_divergence`，每次对两段正文重新分词建集合；50 人冲突组 = 1225 对 × 2 次分词 | `consolidation/src/detectors/conflict.rs:127-142` | 组内每成员分词一次缓存 `HashSet`，再两两求 Jaccard |
| 25 | IDF 闭包内 `terms.iter().position()` 线性查找：O(terms²·candidates)，长查询 × 200 候选时白算 | `crates/retrieval/src/hybrid.rs:361-367` | 先建 `HashMap<&str, f32>` |
| 26 | 内存后端版本历史 `Vec` + 全表线性扫描，幂等判重 O(n)（且发生在写锁内），长期运行写路径退化为 O(历史总量) | `in_memory.rs:434-457` | 改 `BTreeMap<(Uuid, i64), MemoryVersion>`，`contains_key` 判重、`range` 按前缀取 |

## 5. P3 — 可维护性（精选 6 项）

| # | 问题 | 修复方向 |
|---|------|----------|
| 27 | 超大文件：postgres.rs 3391 行（7 个 store trait 挤一个文件）、backup_service.rs 2580 行、chat_cognitive_service.rs 1815 行、in_memory.rs 2192 行、hybrid.rs 1969 行（实现 + 1300 行测试混放） | postgres 按 trait 拆 `postgres/memory.rs`、`postgres/coding.rs` 等子模块；hybrid 测试移 `tests/` |
| 28 | `coding_revisions` 19 列 SELECT 清单在 postgres.rs 重复 7 次，memories INSERT 语句重复 2 次（`insert` 与 `insert_initial` 几乎全同）——正是 #9 行映射吞错的温床 | 列清单与 INSERT 语句收敛为常量/函数 |
| 29 | 事实演化/含糊意图启发式 marker 列表（"改用/不再使用/replace…"）在 domain.rs、chat_cognitive_service.rs、conflict.rs 三处各维护一份且互有出入 | 收敛到 domain.rs（或独立 heuristics 模块）单一常量 |
| 30 | `JobScheduler` 是死代码（全文件无任何调用方）；backup-worker main.rs 用裸变量手写调度，四处判定逻辑各不相同，整合扫描间隔 24h 硬编码不走 cron 也不走配置 | main.rs 循环迁移到 JobScheduler（补"租约跳过不推进游标"语义），或删除 scheduler.rs |
| 31 | TestHubStore 在 mem-core 三个文件（backup_service / chat_cognitive_service / memory_service 的测试模块）重复手写三份，行为还略有差异（如 `link_task_experience` 查重与否）——mem-core 一半以上行数是测试支撑代码 | 统一用 `testing::TestHubStore`（或 store crate 的 `InMemoryHubStore`），删除内嵌副本 |

## 6. 轻微项全清单（34 项，按区域）

### 6.1 存储与检索层

| 问题 | 位置 | 说明 |
| :--- | :--- | :--- |
| 内存后端 similar 用点积、维度不匹配静默返回空 | `in_memory.rs:382-391` | 见链路二；改 `cosine_similarity`（mem-core 已有） |
| `vec_candidates` 防御性复核在 Prefer 模式下硬删非同场景候选：`scene_pushdown`/`scene_keeps` 刻意保留场景多样性交由打分降权，向量路事后复核却无条件硬过滤，与 FTS 路不对称 | `retrieval/src/hybrid.rs:269-277` | 复核条件改为 `SceneMode::Strict` 时才过滤，或复用 `scene_keeps` |
| 软超时降级的 `fallback_candidates` 失败静默（`unwrap_or_default()`）：FTS/向量路失败都有 warn（注释自己写明"降级必须可观测"），最需要观测的兜底路径反而无声 | `hybrid.rs:302-309` | 同样 warn 或上抛 |
| `ensure_namespace` 非事务三段式：并发确保同一命名空间时后者吃 23505 变 Conflict（内存后端此时返回 Ok）；且任何未登记 agent_id 被静默插入 agents 表归属 `BOOTSTRAP_ACCOUNT_ID` | `postgres.rs:1445-1486` | 单事务 + `on conflict do nothing` 回读；agent 兜底注册上移到服务层显式决策 |
| 内存后端 `archive_expired` 先快照再逐条回写：快照与回写之间的并发 `revise` 被旧快照+1 版本直接覆盖，与 PG 单条原子 update 语义不同（注释却声称"语义与 PostgreSQL 对齐"） | `in_memory.rs:459-501` | 回写带版本校验（`save_if_version`） |
| 内存后端 100+ 处 `lock().unwrap()`：任一 panic 发生在持锁期间即 poisoning，之后所有请求级联 panic | `in_memory.rs` 全文 | 换 `parking_lot::RwLock` 或统一 `unwrap_or_else(poisoned => .into_inner())` |
| `list_namespaces` 从 memories distinct 推导，`list_namespace_infos` 走 namespaces 表：同一概念两个事实来源，刚建好未写入的命名空间不可见 | `postgres.rs:1152-1159`、`in_memory.rs:421-432` | `list_namespaces` 改查 namespaces 表或删除该方法 |
| MMR 输出逐条 clone 含 1024 维向量的 `MemoryEntry`；相似度矩阵按全矩阵计算（对称性可省一半） | `hybrid.rs:507-508` | 量级不大（pool ≤ 4k）但属纯浪费 |
| `archive_expired` 返回值是 CTE 里 memory_versions 插入行数，`on conflict do nothing` 命中时与实际归档条数不符 | `postgres.rs:1215-1237` | 改 `returning` 计数归档行 |
| 乐观锁占位符硬编码 `$23`，依赖 23 个绑定参数的隐式约定，加列即错位 | `postgres.rs:738-741` | 动态计算或 QueryBuilder |
| `ping_store` 恒返回 Ok 的假探测（当前无调用方，若被 /readyz 引用会假健康） | `store/src/lib.rs:47-50` | 删除或实现为 `store.ping()` 转发 |
| `open_exists` 循环 N+1 查询（members 每组一次） | `postgres.rs:1419-1440` | `unnest` 合并或 or 拼条件（仍参数绑定） |
| 过滤型 ANN 与 relaxed_order 边界：pgvector < 0.8 带 WHERE 的 HNSW 查询可能返回不足 k 条（代码已探测版本并正确降级，有日志）；relaxed_order 下 `order by dist` 不保证精确有序——hybrid 层会重打分无碍，但直连 `similar()` 的消费方（consolidation）拿到近似顺序 | 迁移 004 注释、`postgres.rs` | 已知边界，文档化即可 |
| 迁移后 `set hnsw.iterative_scan` 是连接级会话变量，只对新建连接生效（代码注释已自知），滚动期间存在混合行为窗口 | 同上 | 已知边界，文档化 |

### 6.2 领域核心与整合层

| 问题 | 位置 | 说明 |
| :--- | :--- | :--- |
| 全量备份 8 段复制粘贴（entries/evidences/relations/tasks/experiences/task_experiences/coding_threads/coding_revisions 各一段 `if namespace == "*"` 循环）+ 8 次 `list_namespaces` 重复扫描 | `backup_service.rs:371-452` | 提取 `load_all<F, T>` 泛型辅助，`list_namespaces` 只查一次 |
| 多命名空间 Replace 恢复时 `safety_backup_id` 逐次覆盖只保留最后一个，前面命名空间的安全备份 ID 丢失 | `backup_service.rs:591-595` | 改 `Vec<Uuid>` 或 `(namespace, id)` 对 |
| DryRun 预演把存储错误计为 created（`Err(_) => report.created += 1` 把 DB 故障与 NotFound 混同） | `backup_service.rs:562-570` | 区分 `HubError::NotFound` 与其他错误 |
| Replace 清空与恢复全部逐条 SQL 无批量接口，万级条目恢复数万次往返，且无事务包裹——中途失败留下半清空状态（报告不提示中断） | `backup_service.rs:597-650,657-820` | 存储层补批量 delete/insert 或事务包裹，恢复入口加中断标记 |
| 关系恢复的命名空间兜底可能写入 `*` 垃圾键（全量备份时 `backup.namespace` 是 `*`，被作用域包装拼成 `account/*` 落库） | `backup_service.rs:700-724` | 兜底为 `*` 时跳过该关系并计数告警 |
| 会话幂等查重只看最近 100 条证据：同一 session 积累超 100 条后，同内容重试可能重复追加证据存根 | `chat_cognitive_service.rs:134-146` | 为 (namespace, source_kind, source_ref, content_hash) 提供精确查询或建索引 |
| `execute_merge` 静默跳过读取失败的成员（`if let Ok`，含瞬态 DB 错误，无日志）；自动合并门槛处 `.is_ok()` 把存储故障与"正文超长需人工"混为一谈 | `consolidation/src/engine.rs:508-528,655-662` | 区分错误类型：InvalidArgument 落提案，其余上抛；get 失败 warn + 计入报告 |
| 命名空间互斥只在进程内有效（`NAMESPACE_LOCKS` 静态注册表，注释已承认）：MCP 进程与 backup-worker 进程可同时操作同一命名空间，双进程同时 `execute_merge` 可能产出双重版本历史 | `engine.rs:33-47` | 数据库部分唯一索引，或 MCP 侧扫描走 `JobLease` |
| cron 表达式按 UTC 解释（"0 3 * * *" 是 UTC 3 点 = 北京 11 点），配置层与文档均无提示 | `bins/backup-worker/src/main.rs:193` | 解析时输出 UTC 与本地时间对照的 info 日志；`.env.example` 注明 |

### 6.3 认证与 Web/MCP 层

| 问题 | 位置 | 说明 |
| :--- | :--- | :--- |
| Origin 校验 fail-open：`allowed_origins` 空列表（默认）恒 true、无 Origin 恒 true；consent/register/login/logout 全部 POST 依赖它做 CSRF 纵深防御，目前靠 SameSite=Lax 兜底但脆弱 | `auth-web/src/routes.rs:1145-1153`、`mcp/transport.rs:706-717` | 空列表时启动 WARN，或要求 Origin 与 external_base_url 同源 |
| CJS 适配器默认启用（`HUB_ENABLE_CJS_ADAPTER` 缺省 on）且触发条件（header/参数）完全客户端可控：任何 Agent 发一个 header 就能让服务端改写其命名空间 | `mcp/src/adapter/cjs/mod.rs:50-58`、`fingerprint.rs:38-50` | 默认改 opt-in |
| `dual_search` 双路都报错时返回 `{"results": []}` 成功响应：权限不足、存储故障被吞成"没有记忆"，调用方 LLM 会把空结果当事实——正是 tools.rs 注释明确要避免的失败模式 | `mcp/src/adapter/cjs/router.rs:94-96` | 错误透传，仅"命名空间未初始化"优雅降级 |
| 任何机密客户端可内省任意令牌并读出该 Agent 的完整命名空间授权清单（`grants` 全量）——跨账户信息泄露 | `routes.rs:470-501`、`oauth_flow.rs:530-549` | 非属主客户端只返回 `active`/`exp`，或要求 client_id 一致才返回明细 |
| HS256 共享密钥无轮换、jti 无吊销清单：密钥泄露即全失守；Agent 停用后其已签发令牌在 TTL 内仍通过验签（`validate_claims` 不查 agent 状态） | `common/src/config.rs:505-525`、`auth-core/src/access_token.rs` | 短期下调 access_token_ttl_s；中期 mcp-server 定期拉取停用名单缓存拦截，或 kid + 双密钥过渡轮换 |
| 过期会话只在启动时清理一次，运行期靠惰性删除（只有被再次访问的才删），`account_sessions` 表持续膨胀 | `bins/auth-server/src/main.rs:73-75` | tokio interval 定期 `prune_sessions` 或交给 worker cron |
| mcp-server stdio 分支死代码：读 `HUB_STDIO_LOCAL_AGENT` 默认 `"local-agent"` 后 `let _ =` 丢弃；真正逻辑在 transport.rs。误导性注释可能让维护者打开全权限旁路 | `bins/mcp-server/src/main.rs:295-299` | 删除死代码，权威逻辑只留一处 |
| `AUTH_EXTERNAL_BASE_URL` 未配置时默认 `http://{bind}` → 会话 Cookie 永不带 Secure，HTTPS 站点上可被明文 HTTP 劫持 | `bins/auth-server/src/main.rs:62-63`、`routes.rs:1156-1172` | http 且非回环绑定时启动 WARN，或独立 `COOKIE_SECURE` 覆盖项 |
| 刷新令牌 TTL（30 天）硬编码在 auth-server 入口，不可配置且与其他 TTL 配置项不一致 | `bins/auth-server/src/main.rs:77-85` | 加 `REFRESH_TOKEN_TTL_DAYS` 进 AppConfig 并纳入 validate |
| token 端点错误响应不区分 `invalid_grant`/`invalid_client`，一律 400 且内部错误串进 `error_description` | `routes.rs:416-424` | 按 OAuth 2.1 错误码映射，细节进日志 |
| authorization_code/refresh_token 流程 `client_id` 可选校验（提供了才比对），OAuth 2.1 要求必带 | `oauth_flow.rs:360-366,426-432` | 改必填（公共客户端也带 client_id，只是不带 secret） |
| 限流存储出错时 fail-open（`unwrap_or(true)`）：注释解释了可用性取舍，但配合限流四洞，PG 短暂不可用期间暴力破解完全无阻力 | `routes.rs:678-694`、`transport.rs:286-290` | 至少对 fail-open 事件计数告警 |
| 会话 Cookie 未用 `__Host-` 前缀，`Path=/` 全站生效 | `routes.rs:1164-1172` | HTTPS 部署下改 `__Host-hub_session` |
| `ns_prop()` pattern 允许 `*`，与 grant/revoke 工具层显式拒绝 `*` 不一致，全靠服务层兜底——schema 契约与实际校验脱节 | `mcp/src/schema.rs:32-40` vs `tools.rs:840-844` | 普通工具的 ns_prop 用不含 `*` 的 pattern |
| 三处 RwLock 全部 `.unwrap()`（锁中毒即级联 panic） | `token_family.rs:53,80,149`、`confirm.rs:31,49`、`oauth_flow.rs:338,512` | 统一 `unwrap_or_else(\|e\| e.into_inner())` 或封装辅助 |

---

## 7. 建议执行方案

分四个批次实施，每批次完成后统一验证全绿再进入下一批。

> ⚠️ **路径约束**：本仓库检出路径含空格（`/Volumes/WD Drive/...`）时，**不要用 `--all-features`**——`tikv-jemalloc-sys` 构建脚本不支持带空格前缀路径。默认内存分配器各平台够用。详见 [`REPOSITORY_AUDIT.md`](./REPOSITORY_AUDIT.md)。

### 批次 1 — P0 正确性/安全（#1–#9）

核心承诺"数据不丢、服务不崩、限流有效"。每项修复配回归测试：

- #2 补"upsert 保留 scene/layer"测试；#3 补误报/漏报用例；#4 补溢出用例；#7 补双后端契约测试；
- #5 涉及认知提案，需补端到端 propose → list → resolve 测试；
- 检索行为无改动，不影响 eval_bench 基线。

### 批次 2 — P1 可靠性（#10–#18）

- #17（备份 v4 统一）改动面最大，先做；#12（FTS 回填迁移）需新迁移脚本；
- #13 的 `spawn_blocking` 注意 trait 是 async fn，需在实现内包裹。

### 批次 3 — P2 性能（#19–#26）

- #19（瘦列清单）收益最大且改动小，先做；
- 改完跑 `cargo test --test eval_bench` 确认检索指标无退化。

### 批次 4 — P3 可维护性 + 轻微项

纯重构不改行为，靠既有 485 个测试兜底；轻微项按区域自愿认领。

### 每批次统一验证门槛

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                # 485+ 用例全绿
cargo test --test eval_bench          # 检索指标不退化（批次 3 后必跑）
cargo test --test consolidation_eval  # 整合指标不退化（涉及整合时跑）
```

---

## 8. 修复进度跟踪

修复后请在对应条目标记 PR 与日期：

| 批次 | 状态 | 完成项 | 备注 |
| :--- | :--- | :--- | :--- |
| 批次 1（P0） | ✅ 完成（2026-10-25） | #1–#9 全部 | #5 采用独立表 `cognitive_proposals`（迁移 012）；#7 顺带修正审校事实：内存 revise 本就返回 Conflict；#8 新增 `TRUSTED_PROXIES` 配置；验证全绿：fmt/clippy/496 测试 + eval_bench 门禁（指标零退化）+ consolidation_eval 门禁；提交 `e934321` |
| 批次 2（P1） | ✅ 完成（2026-10-25） | #10–#18 全部 | #12 采用迁移 013 标记列 + 启动回填（分词与写路径收敛到同一入口 `tsv_tokens_for`，pg-integration 端到端测试由 CI 执行）；#17 新增 `system_snapshot` 走系统特权主体，定时快照与手工 v4 同构；#18 核实批次 1 已基本覆盖，补 MCP 端到端守卫测试；验证全绿：fmt/clippy/517 测试 + 双评测门禁零退化；提交 `8f9d6e8` |
| 批次 3（P2） | ✅ 完成（2026-10-26） | #19–#26 全部 | #19 双列清单（瘦版省 embedding）；#20 stats 下推 `count()`；#21 MCP schema `OnceLock` 缓存 + scope 常量查找；#22 认知查重走 `all_in_namespace_limited`；#23 重复带接入 LSH；#24 冲突组预缓存分词；#25 IDF 闭包改 HashMap；#26 版本历史改 BTreeMap；fmt 通过，完整测试/评测待环境空闲后复跑；本报告随批次 3 提交 |
| 批次 4（P3+轻微） | ⬜ 未开始 | — | — |

<!-- kimi-badge -->
[![powered by KimiCode](../../.github/assets/kimi-badge.svg)](https://www.kimi.com/code)
