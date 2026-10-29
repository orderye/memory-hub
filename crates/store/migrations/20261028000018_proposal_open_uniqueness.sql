-- 迁移 000018: 整合提案的跨进程唯一性（open 提案按业务键去重）
--
-- 背景：整合扫描是 check-then-act——先读该 namespace 的 open 提案集合，再逐个
-- insert。进程内 mutex 只能串行化同进程扫描；MCP server 与 backup-worker 等
-- 独立进程同时扫同一 namespace 时，两边都会在窗口内看到"无既有提案"并各自
-- 插入，产生重复 open 提案（同一 (namespace, kind, 成员集合) 出现多条）。
--
-- 方案：为 open 状态建立**部分唯一索引**，业务键为
--   (namespace, kind, member_ids)
-- 其中 member_ids 在创建前已排序（引擎侧 `ids.sort()`），因此数组值可直接比较。
-- 部分索引只约束 open 行：提案关闭（merged/dismissed/expired）后，
-- 同一逻辑候选再次出现仍允许新建提案，符合"关闭后可重新扫描"的语义。
--
-- 历史数据处理：CREATE UNIQUE INDEX 在存在重复行时会失败。本迁移先删除
-- 同业务键的**多余** open 提案，仅保留每组最早创建的一条（保留最早 = 保留
-- 最初发现该候选的记录，其余成员集合相同、信息等价）。删除仅作用于重复组，
-- 不触碰任何非 open 行，也不触碰唯一组。
--
-- 兼容性：
-- - 旧应用不受影响：索引只约束 open 行，读取路径不变；
-- - 幂等：可重复执行；
-- - 回滚：drop index if exists uq_integration_proposals_open_business_key。

-- 1. 清理历史重复（同 namespace + kind + 相同成员集合的 open 提案只留最早一条）
with ranked as (
  select id,
         row_number() over (
           partition by namespace, kind, member_ids
           order by created_at asc, id asc
         ) as rn
  from integration_proposals
  where status = 'open'
)
delete from integration_proposals p
using ranked r
where p.id = r.id
  and r.rn > 1;

-- 2. 部分唯一索引：只约束 open 行
create unique index if not exists uq_integration_proposals_open_business_key
  on integration_proposals (namespace, kind, member_ids)
  where status = 'open';

comment on index uq_integration_proposals_open_business_key is
  'open 整合提案的业务唯一键：同 namespace + kind + 成员集合只允许一条；关闭后可再次新建';
