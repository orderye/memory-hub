//! CJK 2-gram / 1-gram 零扩展全文检索预分词器。
//!
//! PostgreSQL 内置的 `'simple'` 字典只按空白与标点切词，中文在没有空格时
//! 会被整体当作一个超长词元，导致普通子串搜索永远无法命中。
//!
//! 本模块实现零外部 C 扩展依赖的纯 Rust 预分词：
//! - 英文、数字及 ASCII 词汇保留原词（全小写）；
//! - 连续 CJK 汉字切分为 2-gram（相邻双字）与 1-gram（单字）；
//! - 生成以单个空格隔开的词元流，供 PostgreSQL 的 `to_tsvector('simple', tsv_tokens)`
//!   与 `plainto_tsquery('simple', query_tokens)` 进行索引与查询。

/// 低信息量 CJK 功能词（2-gram 形态）。
///
/// 零依赖 bigram 切分会把 “我们的方案” 切成 `我们 / 们的 / 的方 / 方案`：
/// 其中 `我们 / 们的` 这类功能词 bigram 判别力极低，命中它们不代表语义相关。
/// 文档、查询与打分三处统一剔除，保证三套词法语义一致。
pub const CJK_STOP_BIGRAMS: &[&str] = &[
    "我们", "你们", "他们", "她们", "它们", "咱们", "什么", "怎么", "哪些", "这个", "那个", "这些",
    "那些", "这里", "那里", "的话", "是的", "可以", "不是", "就是", "也是", "还是", "但是", "可是",
    "因为", "所以", "如果", "虽然", "然而", "而且", "并且", "或者", "以及", "由于", "为了", "关于",
    "已经", "正在", "一个", "一些", "一下", "一样", "之后", "之前", "的时", "的问", "的方", "的了",
    "们的", "的数", "的说",
];

/// 判断词元是否为低信息量功能词
pub fn is_stop_token(token: &str) -> bool {
    CJK_STOP_BIGRAMS.contains(&token)
}

/// 判定单个字符是否为常用 CJK 字符
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'     // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'   // CJK Extension A
        | '\u{F900}'..='\u{FAFF}'   // CJK Compatibility Ideographs
        | '\u{20000}'..='\u{2A6DF}' // CJK Extension B
        | '\u{2A700}'..='\u{2B73F}' // CJK Extension C
        | '\u{2B740}'..='\u{2B81F}' // CJK Extension D
        | '\u{2B820}'..='\u{2CEAF}' // CJK Extension E
        | '\u{2CEB0}'..='\u{2EBEF}' // CJK Extension F
        | '\u{30000}'..='\u{3134F}' // CJK Extension G
    )
}

/// 高频 CJK 功能字：单字词元本身没有检索价值，由功能字组成的 bigram
/// 也几乎一定是功能词片段（"不能" "的了" "是在"）。
const CJK_FUNCTION_CHARS: &str =
    "的了们是在和与及或也就都而其之于以为对从到把被会能要有不这那些么呢吧啊哦呀很太更最我你他她它谁什吗着过再又还只等让使给向同并但若则且因由当咱您";

/// 判断单字是否为低信息量功能字
pub fn is_function_char(c: char) -> bool {
    CJK_FUNCTION_CHARS.contains(c)
}

/// 判断 bigram 是否为低信息量词元（显式停用词，或两字均为功能字）
pub fn is_low_signal_bigram(gram: &str) -> bool {
    if is_stop_token(gram) {
        return true;
    }
    let mut chars = gram.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) => is_function_char(a) && is_function_char(b),
        _ => false,
    }
}

/// CJK 段落的词元入栈（`with_singles` 决定是否发出单字词元）
fn push_cjk_tokens(cjk_slice: &[char], with_singles: bool, tokens: &mut Vec<String>) {
    if with_singles {
        for &ch in cjk_slice {
            if !is_function_char(ch) {
                tokens.push(ch.to_string());
            }
        }
    }
    if cjk_slice.len() >= 2 {
        for window in cjk_slice.windows(2) {
            let gram: String = window.iter().collect();
            if is_low_signal_bigram(&gram) {
                continue;
            }
            tokens.push(gram);
        }
    }
}

/// 将待索引的正文切分为供 `to_tsvector('simple', ...)` 消费的词元流。
///
/// 策略：
/// 1. ASCII 连续字母与数字按词小写保留；
/// 2. CJK 连续汉字序列同时生成全部 1-gram（单字）和 2-gram（双字），
///    剔除功能字单字与低信息量 bigram；
/// 3. 以单空格连接，形成可在 PostgreSQL 'simple' 字典下建 GIN 索引的文本。
pub fn tokenize_for_fts(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if is_cjk_char(c) {
            // 收集连续 CJK 字符段
            let start = i;
            while i < chars.len() && is_cjk_char(chars[i]) {
                i += 1;
            }
            push_cjk_tokens(&chars[start..i], true, &mut tokens);
        } else if c.is_ascii_alphanumeric() || c == '_' {
            // 收集连续 ASCII 字母数字词
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            tokens.push(word.to_ascii_lowercase());
        } else {
            // 空格、标点等分隔符直接跳过
            i += 1;
        }
    }

    tokens.join(" ")
}

/// 将检索关键词切分为供 `to_tsquery('simple', ...)` 消费的查询词元流。
///
/// 策略：
/// 1. ASCII 连续字母与数字按词小写保留；
/// 2. CJK 字符段：若仅 1 字，发出单字 1-gram；若 >= 2 字，发出 2-gram，
///    同样剔除低信息量词元；
/// 3. 词元流与文档侧保持一致，保证"文档/查询/打分"三处词法语义统一。
pub fn tokenize_query_for_fts(query: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if is_cjk_char(c) {
            let start = i;
            while i < chars.len() && is_cjk_char(chars[i]) {
                i += 1;
            }
            let cjk_slice = &chars[start..i];
            push_cjk_tokens(cjk_slice, cjk_slice.len() == 1, &mut tokens);
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            tokens.push(word.to_ascii_lowercase());
        } else {
            i += 1;
        }
    }

    tokens.join(" ")
}

/// 文档词元列表（统一词法入口，供存储层与打分复用）
pub fn fts_doc_tokens(content: &str) -> Vec<String> {
    tokenize_for_fts(content)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// 查询词元列表（统一词法入口）
pub fn fts_query_tokens(query: &str) -> Vec<String> {
    tokenize_query_for_fts(query)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// 词元是否可以安全嵌入 `to_tsquery('simple', ...)` 表达式
fn is_tsquery_safe_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || is_cjk_char(c))
}

/// 构造 `to_tsquery` 的 OR 表达式：任一查询词元命中即可召回。
///
/// 采用部分匹配（OR）而非全词元 AND：AND 语义下"命中即全中"，打分器无法
/// 区分相关度，且多词查询会因单个词元缺失而整条遗漏，召回被系统性压低。
/// 相关度由 `ts_rank_cd` 排序与打分层的 IDF 加权共同承担。
pub fn fts_or_query(query: &str) -> Option<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let tokens: Vec<String> = fts_query_tokens(query)
        .into_iter()
        .filter(|t| is_tsquery_safe_token(t))
        .filter(|t| seen.insert(t.clone()))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(" | "))
}

/// 查询词元在文档词元中的命中数（部分匹配，供内存存储做相关性排序）
pub fn fts_query_hits(content: &str, query: &str) -> usize {
    let doc_tokens: std::collections::HashSet<String> =
        fts_doc_tokens(content).into_iter().collect();
    fts_query_tokens(query)
        .into_iter()
        .filter(|t| doc_tokens.contains(t))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cjk_bigram_tokenize_document() {
        let text = "服务部署在东京区域";
        let tokens = tokenize_for_fts(text);
        // 单字包含
        assert!(tokens.contains("东"));
        assert!(tokens.contains("京"));
        // 双字包含
        assert!(tokens.contains("服务"));
        assert!(tokens.contains("部署"));
        assert!(tokens.contains("东京"));
        assert!(tokens.contains("区域"));
    }

    #[test]
    fn test_cjk_bigram_tokenize_mixed() {
        let text = "PostgreSQL 15 数据库在 Tokyo 部署";
        let tokens = tokenize_for_fts(text);
        assert!(tokens.contains("postgresql"));
        assert!(tokens.contains("15"));
        assert!(tokens.contains("tokyo"));
        assert!(tokens.contains("数据"));
        assert!(tokens.contains("据库"));
        assert!(tokens.contains("部署"));
    }

    #[test]
    fn test_stopword_bigrams_are_dropped() {
        // “我们的方案” 曾产生 我们/们的/的方 等无判别力词元
        let tokens = tokenize_query_for_fts("我们的方案");
        assert!(!tokens.split_whitespace().any(|t| t == "我们"), "{tokens}");
        assert!(!tokens.split_whitespace().any(|t| t == "们的"), "{tokens}");
        assert!(
            tokens.split_whitespace().any(|t| t == "方案"),
            "有效词元必须保留：{tokens}"
        );

        // 文档侧同样剔除（保持文档/查询一致）
        let doc = tokenize_for_fts("我们的数据库方案");
        assert!(!doc.split_whitespace().any(|t| t == "们"), "{doc}");
    }

    #[test]
    fn test_fts_or_query_is_partial_match() {
        // 多词查询应为 OR 表达式：任一词元命中即可召回（部分匹配）
        let q = fts_or_query("Redis 缓存集群").expect("应有可用词元");
        assert!(q.contains(" | "), "必须是 OR 语义：{q}");
        assert!(q.contains("redis"));
        assert!(q.contains("缓存"));

        // 全是停用词 → 无词法信号
        assert!(fts_or_query("我们的").is_none());
    }

    #[test]
    fn test_fts_query_hits_partial() {
        let doc = "服务部署在东京区域";
        assert_eq!(
            fts_query_hits(doc, "东京 架构"),
            1,
            "部分匹配应计数命中词元"
        );
        assert_eq!(fts_query_hits(doc, "东京"), 1);
        assert_eq!(fts_query_hits(doc, "架构"), 0);
    }

    #[test]
    fn test_cjk_bigram_tokenize_query() {
        // 单字查询
        assert_eq!(tokenize_query_for_fts("东"), "东");
        // 双字查询
        assert_eq!(tokenize_query_for_fts("东京"), "东京");
        // 四字查询 -> 3 个 2-gram
        assert_eq!(tokenize_query_for_fts("东京区域"), "东京 京区 区域");
        // 混排查询
        assert_eq!(tokenize_query_for_fts("Tokyo 东京"), "tokyo 东京");
    }
}
