-- 13. FTS 重分词标记：修复迁移 005 对存量行的原文回填
--
-- 背景：迁移 005 的 `update memories set tsv_tokens = content where tsv_tokens = ''`
-- 把未分词原文写进了 tsv_tokens。该列设计上存放 Rust 侧预分词流
-- （mem-core 的 CJK 2-gram/1-gram + 英文原词），`to_tsvector('simple', ...)`
-- 对无空格中文整句只产生一个超长词元，导致迁移时已存在的中文行全文检索
-- 实际失效，直到该行下次被写路径（insert / apply_update_on，均调用
-- mem_core::tokenize_for_fts）重写 tsv_tokens。
--
-- 设计决策（为什么是标记列而不是纯 SQL 重分词或启动全表比对）：
-- - 分词只能在 Rust 侧做：bigram 策略、功能字/停用词剔除在 mem-core，
--   纯 SQL 无法复刻，复制一份分词逻辑必然与写路径漂移；
-- - 启动时全表拉 content 逐行比对"tsv_tokens 是否等于重分词结果"每次启动
--   都全表扫描，代价高且无法区分"恰好等于"的行；标记列是一次性的。
--
-- 标记条件 `tsv_tokens = content and content <> ''` 精确对齐迁移 005 的
-- 回填特征（005 执行后受害行必有 tsv_tokens = content；content 非空由
-- 表约束保证，此处防御性重申）。若某行原文恰好等于其分词结果（如纯英文
-- 单词句），会被误标记——无害，重分词幂等回写一次即清标记。
--
-- 部分索引决策：回填查询按 `where fts_retokenize_pending and id > $1
-- order by id limit N` 键集分页，无索引则每批全表扫描；部分索引只含
-- 待处理行，回填收敛后为空索引——INSERT 走列默认值 false 不进索引，
-- UPDATE 仅在回填自身与 apply_update_on 置 false 时发生（谓词不满足，
-- 不产生索引条目），日常维护成本近零，值得保留。
--
-- 幂等：列与索引 if not exists；update 在重复执行时对已回填的行不再命中
-- （重分词后 tsv_tokens <> content）。
--
-- 回填由 connect() 在迁移完成后调用 backfill_fts_tokens 完成，分批短事务，
-- 回填期间读写不受影响（行级锁）。

alter table memories add column if not exists fts_retokenize_pending boolean not null default false;

update memories set fts_retokenize_pending = true
  where tsv_tokens = content and content <> '';

create index if not exists idx_memories_fts_retokenize_pending
  on memories (id)
  where fts_retokenize_pending;
