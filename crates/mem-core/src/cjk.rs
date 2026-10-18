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

/// 将待索引的正文切分为供 `to_tsvector('simple', ...)` 消费的词元流。
///
/// 策略：
/// 1. ASCII 连续字母与数字按词小写保留；
/// 2. CJK 连续汉字序列同时生成全部 1-gram（单字）和 2-gram（双字）；
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
            let cjk_slice = &chars[start..i];
            // 产生全部 1-gram
            for &ch in cjk_slice {
                tokens.push(ch.to_string());
            }
            // 产生全部 2-gram
            if cjk_slice.len() >= 2 {
                for window in cjk_slice.windows(2) {
                    let gram: String = window.iter().collect();
                    tokens.push(gram);
                }
            }
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

/// 将检索关键词切分为供 `plainto_tsquery('simple', ...)` 消费的查询词元流。
///
/// 策略：
/// 1. ASCII 连续字母与数字按词小写保留；
/// 2. CJK 字符段：若仅 1 字，发出单字 1-gram；若 >= 2 字，发出 2-gram（精准匹配连续词）。
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
            if cjk_slice.len() == 1 {
                tokens.push(cjk_slice[0].to_string());
            } else {
                for window in cjk_slice.windows(2) {
                    let gram: String = window.iter().collect();
                    tokens.push(gram);
                }
            }
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
