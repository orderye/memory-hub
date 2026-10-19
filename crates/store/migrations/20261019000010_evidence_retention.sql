-- 10. 证据保留期限（evidence retention）
--
-- 背景：memory_evidence 是 append-only 的不可变表。一旦凭证 / 个人信息写进去，
-- 之后再"修订"或"删除"也补不回来——原始副本可能已随备份扩散。
-- 因此敏感内容必须在写入前脱敏（mem-core::redaction），本迁移补上第二道防线：
-- 给每条证据一个明确的到期时刻（expires_at），到期后由清理任务物理删除。
--
-- expires_at 为 NULL 表示永久保留（历史数据默认不设期限，不做隐式删除）。

alter table memory_evidence add column if not exists expires_at timestamptz;

-- 清理任务按到期时间扫描：部分索引让它只扫已到期的那部分
create index if not exists idx_memory_evidence_expires_at
    on memory_evidence (expires_at)
    where expires_at is not null;
