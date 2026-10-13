-- 5. 中文零扩展 2-gram / 1-gram 全文检索增强
--
-- 增加 tsv_tokens 列，保存 Rust 侧预切分的词元流（CJK 2-gram/1-gram 与英文原词）；
-- 并建立 to_tsvector('simple', tsv_tokens) 的 GIN 索引。

alter table memories add column if not exists tsv_tokens text not null default '';

update memories set tsv_tokens = content where tsv_tokens = '';

create index if not exists idx_memories_tsv_tokens on memories using gin (to_tsvector('simple', tsv_tokens));
