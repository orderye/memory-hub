//! 证据持久化前的脱敏与最小化
//!
//! # 边界定义
//!
//! 「不可变证据」指的是**写入之后**证据行不可被悄悄改写（append-only、
//! 修订记录只追加不改历史），它**不**意味着写入之前不允许脱敏。
//!
//! 这一点必须写死：一旦 Token、密码、私钥或个人信息进了不可变表，
//! 之后再删除或"修订"也补不回来——原始副本可能已随备份扩散。
//! 因此凭证扫描与敏感片段处理必须发生在**持久化之前**，
//! 而不是寄望于写入之后再补救。
//!
//! # 三道处理
//!
//! 1. **凭证扫描**：私钥块、API Token、Bearer/JWT、连接串口令、
//!    `password=`/`api_key=` 一类赋值——全部替换为占位符；
//! 2. **敏感片段处理**：邮箱、手机号、身份证号等个人信息做掩码；
//! 3. **最小化保存**：证据只保存定长摘要（[`EVIDENCE_EXCERPT_MAX_CHARS`]），
//!    完整对话永不落库。
//!
//! # 为什么不用 `\b` 词边界
//!
//! Rust 的 `regex` 不支持环视（look-around），且 `\b` 在中英混排文本里不可靠：
//! CJK 字符属于 Unicode 词字符，`"联系zhangsan@example.com"` 中 `系` 与 `z`
//! 之间**没有**词边界，邮箱会漏检。因此本模块改用"显式上下文组"模式：
//! 正则把边界字符放在捕获组**之外**，只替换值所在的捕获组，
//! 边界字符自然保留。

use regex::Regex;
use std::sync::OnceLock;

/// 证据摘要的最大字符数（最小化保存上限）。
///
/// 原始会话全文**不**入库：证据行只保留这个长度的摘要，
/// 供事后审计"当时说了什么"，而不是复制整段对话。
pub const EVIDENCE_EXCERPT_MAX_CHARS: usize = 500;

/// 脱敏类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionKind {
    /// 凭证 / 密钥 / 令牌
    Credential,
    /// 私钥块
    PrivateKey,
    /// 个人信息
    PersonalData,
}

impl RedactionKind {
    /// 占位符标签
    pub fn label(self) -> &'static str {
        match self {
            RedactionKind::Credential => "凭证",
            RedactionKind::PrivateKey => "私钥",
            RedactionKind::PersonalData => "个人信息",
        }
    }
}

/// 单类别的命中统计
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionFinding {
    pub kind: RedactionKind,
    /// 命中片段数量
    pub count: usize,
}

/// 脱敏结果
#[derive(Debug, Clone)]
pub struct RedactionReport {
    /// 脱敏后的文本
    pub text: String,
    /// 各类别的命中统计（为空表示未发现敏感内容）
    pub findings: Vec<RedactionFinding>,
}

impl RedactionReport {
    /// 是否发现任何敏感内容
    pub fn has_findings(&self) -> bool {
        self.findings.iter().any(|f| f.count > 0)
    }

    /// 命中片段总数
    pub fn total(&self) -> usize {
        self.findings.iter().map(|f| f.count).sum()
    }
}

/// 一条脱敏规则
struct Rule {
    re: Regex,
    /// Some(n)：只替换第 n 个捕获组的区间（保留组外的键名与边界字符）；
    /// None：替换整个匹配
    group: Option<usize>,
    kind: RedactionKind,
}

fn rules() -> &'static Vec<Rule> {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let mk = |pattern: &str, group: Option<usize>, kind: RedactionKind| Rule {
            re: Regex::new(pattern).expect("脱敏规则必须是合法正则"),
            group,
            kind,
        };

        vec![
            // ===== 私钥块（整块替换，含 BEGIN/END 之间的全部内容）=====
            mk(
                r"(?is)-----BEGIN[A-Z ]*PRIVATE KEY-----.*?-----END[A-Z ]*PRIVATE KEY-----",
                None,
                RedactionKind::PrivateKey,
            ),
            // ===== 通用凭证 =====
            // 上下文字符（^ 或非标识符字符）在捕获组之外，只替换值本身

            // OpenAI / 兼容厂商 sk-…、rk-…、pk-…
            mk(
                r"(?:^|[^A-Za-z0-9_])((?:sk|rk|pk)-[A-Za-z0-9_\-]{16,})",
                Some(1),
                RedactionKind::Credential,
            ),
            // GitHub token（ghp_/gho_/ghu_/ghs_/ghr_/github_pat_）
            mk(
                r"(?:^|[^A-Za-z0-9_])((?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{16,})",
                Some(1),
                RedactionKind::Credential,
            ),
            mk(
                r"(?:^|[^A-Za-z0-9_])(github_pat_[A-Za-z0-9_]{20,})",
                Some(1),
                RedactionKind::Credential,
            ),
            // GitLab token
            mk(
                r"(?:^|[^A-Za-z0-9_])(glpat-[A-Za-z0-9_\-]{16,})",
                Some(1),
                RedactionKind::Credential,
            ),
            // Slack token
            mk(
                r"(?:^|[^A-Za-z0-9_])(xox[baprs]-[A-Za-z0-9\-]{10,})",
                Some(1),
                RedactionKind::Credential,
            ),
            // AWS Access Key ID
            mk(
                r"(?:^|[^A-Za-z0-9_])((?:AKIA|ASIA)[0-9A-Z]{16})",
                Some(1),
                RedactionKind::Credential,
            ),
            // Google API Key
            mk(
                r"(?:^|[^A-Za-z0-9_])(AIza[0-9A-Za-z_\-]{35})",
                Some(1),
                RedactionKind::Credential,
            ),
            // JWT
            mk(
                r"(?:^|[^A-Za-z0-9_])(eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*)",
                Some(1),
                RedactionKind::Credential,
            ),
            // Authorization 头：保留 "Bearer" 关键字，只替换令牌
            mk(
                r"(?i)(?:^|[^A-Za-z0-9_])((?:bearer|basic)\s+)([A-Za-z0-9._\-+/=]{12,})",
                Some(2),
                RedactionKind::Credential,
            ),
            // 带口令的连接串：只替换口令部分，保留 scheme://user 与 @host
            mk(
                r"([A-Za-z][A-Za-z0-9+.\-]*://[^\s:/@]+:)([^\s:/@]+)(@[^\s]+)",
                Some(2),
                RedactionKind::Credential,
            ),
            // 赋值型密钥：保留键名与分隔符，只替换值
            mk(
                r#"(?i)\b(password|passwd|pwd|secret|secret_key|client_secret|api[_\-]?key|apikey|access[_\-]?key|access[_\-]?token|auth[_\-]?token|private[_\-]?key)([ \t]*[:=][ \t]*["']?)([^\s"',;]{6,})"#,
                Some(3),
                RedactionKind::Credential,
            ),
            // AWS secret access key（长 base64 变体）
            mk(
                r#"(?i)(aws_secret_access_key[ \t]*[:=][ \t]*["']?)([A-Za-z0-9/+=]{30,})"#,
                Some(2),
                RedactionKind::Credential,
            ),
            // ===== 个人信息 =====
            // 邮箱（边界字符在捕获组之外，中英混排同样命中）
            mk(
                r"(?:^|[^A-Za-z0-9._%+\-])([A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,})",
                Some(1),
                RedactionKind::PersonalData,
            ),
        ]
    })
}

/// 扫描数字串并脱敏手机号 / 身份证号
///
/// 不用正则环视（`regex` 不支持）也不用 `\b`（CJK 文本里不可靠），
/// 改为扫描**极大 ASCII 数字串**再按长度与前缀判定：
/// - 11 位且以 `1` 开头、次位 3~9 → 中国大陆手机号；
/// - 18 位（可带结尾 x/X）→ 身份证号。
fn redact_numeric_pii(text: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut count = 0usize;

    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();

        // 身份证号可能以 x/X 结尾
        let mut end = i;
        let mut body = run.clone();
        if run.len() == 18 && i < chars.len() && (chars[i] == 'x' || chars[i] == 'X') {
            body = format!("{run}x");
            end = i + 1;
        }

        let is_mobile = run.len() == 11
            && run.starts_with('1')
            && run.chars().nth(1).is_some_and(|c| ('3'..='9').contains(&c));
        let is_id = body.len() == 18;

        if is_mobile || is_id {
            out.push_str(&format!("[已脱敏:{}]", RedactionKind::PersonalData.label()));
            count += 1;
            i = end;
            continue;
        }
        out.push_str(&run);
    }
    (out, count)
}

/// 脱敏文本中的凭证与敏感片段，返回脱敏结果与命中统计。
///
/// 命中的片段整体替换为 `[已脱敏:<类别>]` 占位符；赋值型规则
/// （`password=…`）只替换**值**部分，保留键名，便于事后审计
/// "这里曾经有过一个口令"而不泄露口令本身。
pub fn redact(text: &str) -> RedactionReport {
    let mut current = text.to_string();
    let mut findings: Vec<RedactionFinding> = Vec::new();

    let record = |kind: RedactionKind, count: usize, findings: &mut Vec<RedactionFinding>| {
        if count == 0 {
            return;
        }
        match findings.iter_mut().find(|f| f.kind == kind) {
            Some(f) => f.count += count,
            None => findings.push(RedactionFinding { kind, count }),
        }
    };

    for rule in rules() {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for caps in rule.re.captures_iter(&current) {
            let whole = caps.get(0).expect("捕获组 0 必然存在");
            let target = match rule.group {
                Some(idx) => caps.get(idx),
                None => Some(whole),
            };
            if let Some(t) = target {
                if t.start() < t.end() {
                    spans.push((t.start(), t.end()));
                }
            }
        }
        if spans.is_empty() {
            continue;
        }

        let count = spans.len();
        // 逆序替换，避免前面的替换让后面的偏移失效
        let mut out = current.clone();
        for (start, end) in spans.iter().rev() {
            out.replace_range(*start..*end, &format!("[已脱敏:{}]", rule.kind.label()));
        }
        current = out;

        record(rule.kind, count, &mut findings);
    }

    // 数字类个人信息（手机号 / 身份证号）单独扫描
    let (out, count) = redact_numeric_pii(&current);
    if count > 0 {
        current = out;
        record(RedactionKind::PersonalData, count, &mut findings);
    }

    RedactionReport {
        text: current,
        findings,
    }
}

/// 脱敏并按上限截断，得到可安全落库的证据摘要。
///
/// 这是证据持久化的唯一入口：先脱敏（凭证 / 个人信息），再最小化（定长截断）。
/// 顺序不可颠倒——先截断再脱敏会漏掉被截断掉的尾部凭证。
pub fn sanitize_evidence_excerpt(text: &str, max_chars: usize) -> RedactionReport {
    let redacted = redact(text);
    let limit = max_chars.max(1);
    let mut excerpt: String = redacted.text.chars().take(limit).collect();
    if redacted.text.chars().count() > limit {
        excerpt.push('…');
    }
    RedactionReport {
        text: excerpt,
        findings: redacted.findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_no_secret(text: &str, secret: &str) {
        assert!(
            !text.contains(secret),
            "脱敏后仍残留敏感内容 [{}]: {}",
            secret,
            text
        );
    }

    #[test]
    fn test_redacts_common_credential_shapes() {
        let cases = [
            "sk-proj-1234567890abcdef1234567890abcdef",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "github_pat_11ABCDEFG0abcdefghijklmnop",
            "glpat-abcdefghij1234567890",
            "xoxb-123456789012-abcdefghijkl",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyA1234567890abcdefghijklmnopqrstuv",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
            "Bearer abcdefghijklmnopqrstuvwxyz012345",
            "postgres://app:sup3rs3cret@db.internal:5432/memory_hub",
            "password = hunter2000",
            "api_key: AKIAIOSFODNN7EXAMPLE",
            "client_secret=abcdefgh12345678",
        ];
        for c in cases {
            let out = redact(c);
            assert!(out.has_findings(), "未检出敏感内容: {c}");
            assert!(
                out.text.contains("[已脱敏"),
                "未生成占位符: {c} -> {}",
                out.text
            );
        }
    }

    #[test]
    fn test_redacts_private_key_block_entirely() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA1234\nabcdef+/=\n-----END RSA PRIVATE KEY-----";
        let out = redact(pem);
        assert!(!out.text.contains("MIIEowIBAAKCAQEA1234"));
        assert!(!out.text.contains("abcdef+/="));
        assert!(out.text.contains("[已脱敏:私钥]"));
    }

    #[test]
    fn test_keeps_key_name_but_masks_value() {
        let out = redact("password: hunter2000");
        assert!(out.text.contains("password"), "键名应保留: {}", out.text);
        assert_no_secret(&out.text, "hunter2000");
    }

    #[test]
    fn test_keeps_connection_string_host_and_user() {
        let out = redact("postgres://app:sup3rs3cret@db.internal:5432/memory_hub");
        assert!(out.text.contains("postgres://app:"), "{}", out.text);
        assert!(out.text.contains("@db.internal"), "{}", out.text);
        assert_no_secret(&out.text, "sup3rs3cret");
    }

    #[test]
    fn test_redacts_personal_data() {
        let out =
            redact("联系人 zhangsan@example.com，手机 13800138000，身份证 110101199003072516");
        assert!(!out.text.contains("zhangsan@example.com"));
        assert!(!out.text.contains("13800138000"));
        assert!(!out.text.contains("110101199003072516"));
        assert!(out
            .findings
            .iter()
            .any(|f| f.kind == RedactionKind::PersonalData));
    }

    #[test]
    fn test_redacts_inside_chinese_text_without_word_boundaries() {
        // 中英混排：CJK 是 Unicode 词字符，\b 在这里不可用
        let out = redact("我的密钥是sk-proj-abcdefghijklmnopqrstuvwxyz012345请保密");
        assert!(
            !out.text
                .contains("sk-proj-abcdefghijklmnopqrstuvwxyz012345"),
            "中英混排下的凭证必须被脱敏: {}",
            out.text
        );
        assert!(
            out.text.contains("请保密"),
            "中文上下文应保留: {}",
            out.text
        );
    }

    #[test]
    fn test_clean_text_is_untouched() {
        let clean = "用户偏好简洁的中文回答，周五下午不安排会议。";
        let out = redact(clean);
        assert_eq!(out.text, clean);
        assert!(!out.has_findings());
    }

    #[test]
    fn test_plain_numbers_are_not_redacted() {
        // 订单号 / 版本号等不该被当成手机号误伤
        let out = redact("订单号 202610190001 数量 12 单价 99.5");
        assert_eq!(out.text, "订单号 202610190001 数量 12 单价 99.5");
    }

    #[test]
    fn test_sanitize_truncates_after_redaction() {
        // 凭证位于截断窗口之外：摘要里当然没有它，但整段文本已被脱敏
        let text = format!(
            "{}{}",
            "前缀".repeat(400),
            "sk-abcdefghijklmnopqrstuvwxyz012345"
        );
        let out = sanitize_evidence_excerpt(&text, 100);
        assert!(out.text.chars().count() <= 101, "摘要未超长");
        assert!(
            !out.text.contains("sk-abcdefghijklmnopqrstuvwxyz012345"),
            "尾部凭证不得进入摘要: {}",
            out.text
        );
        assert!(out.has_findings(), "超长文本尾部的凭证也必须被检出");

        // 凭证位于截断窗口之内：摘要里必须是占位符
        let near = format!("sk-abcdefghijklmnopqrstuvwxyz012345{}", "后缀".repeat(200));
        let out = sanitize_evidence_excerpt(&near, 100);
        assert!(
            out.text.contains("[已脱敏"),
            "窗口内的凭证必须被脱敏: {}",
            out.text
        );
        assert!(!out.text.contains("sk-abcdefghijklmnopqrstuvwxyz012345"));
    }

    #[test]
    fn test_redaction_does_not_leak_secret_into_findings() {
        let out = redact("token=abcdefghijklmnop");
        // 统计信息只含类别与数量，绝不含被脱敏的原文
        for f in &out.findings {
            assert!(f.count > 0);
        }
        assert!(!format!("{:?}", out.findings).contains("abcdefghijklmnop"));
    }
}
