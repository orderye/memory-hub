//! 记忆正文保守规范化与内容指纹
//!
//! 整合中枢的"规范化指纹"规则需要把**不影响语义**的书写差异折叠掉，
//! 让同一事实的不同排版落到同一个指纹上；但**必须保留**一切会改变事实的内容。
//!
//! 规范化只做三件事：
//! 1. **Unicode 规范形式（NFKC）**：全角/半角、兼容字符、连字归一
//!    （`Ａ`→`A`、`ﬁ`→`fi`、`㎏`→`kg`）。这些是兼容等价形式，不改变事实。
//! 2. **换行归一**：`\r\n` / `\r` → `\n`。
//! 3. **空白折叠**：连续 Unicode 空白（含换行）折叠为单个空格，并去首尾空白。
//!
//! **明确不做**（避免过度清洗导致错误合并）：
//! - 不做大小写折叠：`PostgreSQL` 与 `postgresql` 是不同指纹；
//! - 不删除否定词：`不是 X` 与 `X` 是不同指纹；
//! - 不归一数字、单位、日期、版本号：`v1` 与 `v2`、`15` 与 `16` 是不同指纹；
//! - 不归一路径、URL、代码标识符与大小写敏感令牌；
//! - 不删除标点：`A.` 与 `A` 是不同指纹。
//!
//! 指纹为规范化字节的 SHA-256 十六进制串。

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 生成保守规范化形式
pub fn canonical_form(content: &str) -> String {
    // 1) NFKC 归一（兼容等价字符折叠）
    let normalized: String = content.nfkc().collect();

    // 2) + 3) 换行归一与空白折叠
    let mut out = String::with_capacity(normalized.len());
    let mut pending_space = false;
    for ch in normalized.chars() {
        if ch == '\r' {
            // CRLF / CR 统一按 LF 处理
            pending_space = true;
            continue;
        }
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space {
            if !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// 计算内容指纹（规范化后的 SHA-256 十六进制串）
pub fn content_fingerprint(content: &str) -> String {
    let canonical = canonical_form(content);
    let digest = Sha256::digest(canonical.as_bytes());
    hex::encode(digest)
}

/// 判断两条正文是否落在同一个规范化指纹上
pub fn same_fingerprint(a: &str, b: &str) -> bool {
    // 先比长度可快速排除绝大多数不同正文
    let ca = canonical_form(a);
    let cb = canonical_form(b);
    if ca == cb {
        return true;
    }
    content_fingerprint(a) == content_fingerprint(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitespace_and_newline_variants_share_fingerprint() {
        let a = "PostgreSQL  是开源数据库";
        let b = "PostgreSQL\t是开源数据库\n";
        assert_eq!(content_fingerprint(a), content_fingerprint(b));
        assert!(same_fingerprint(a, b));
    }

    #[test]
    fn test_negation_is_preserved() {
        assert_ne!(
            content_fingerprint("这个方案不是最优解"),
            content_fingerprint("这个方案是最优解")
        );
    }

    #[test]
    fn test_version_and_number_differences_are_preserved() {
        assert_ne!(
            content_fingerprint("服务版本 v1"),
            content_fingerprint("服务版本 v2")
        );
        assert_ne!(
            content_fingerprint("超时 500ms"),
            content_fingerprint("超时 600ms")
        );
        assert_ne!(
            content_fingerprint("PostgreSQL 15"),
            content_fingerprint("PostgreSQL 16")
        );
    }

    #[test]
    fn test_case_is_preserved() {
        assert_ne!(
            content_fingerprint("PostgreSQL 集群"),
            content_fingerprint("postgresql 集群")
        );
    }

    #[test]
    fn test_paths_urls_and_identifiers_are_preserved() {
        assert_ne!(
            content_fingerprint("配置位于 /etc/app/conf.yaml"),
            content_fingerprint("配置位于 /etc/app/config.yaml")
        );
        assert_ne!(
            content_fingerprint("参考 https://example.com/a"),
            content_fingerprint("参考 https://example.com/b")
        );
    }

    #[test]
    fn test_punctuation_is_preserved() {
        assert_ne!(
            content_fingerprint("部署完成。"),
            content_fingerprint("部署完成")
        );
    }

    #[test]
    fn test_fullwidth_is_normalized() {
        // 全角字母与半角字母是兼容等价形式
        assert_eq!(
            content_fingerprint("ＰostgreSQL"),
            content_fingerprint("PostgreSQL")
        );
    }

    #[test]
    fn test_empty_content_has_stable_fingerprint() {
        assert_eq!(content_fingerprint(""), content_fingerprint("   \n\t "));
    }
}
