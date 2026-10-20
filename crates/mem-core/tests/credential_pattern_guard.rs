//! 公开仓库的"凭证字面量"门禁
//!
//! # 为什么需要这道门禁
//!
//! 本仓库在 GitHub 上是**公开仓库**，且已开启 secret scanning 与 push protection。
//! 源码里只要出现**连续**的厂商密钥字面量，扫描器就会判定为疑似真实泄露：
//! 轻则在 Security 页开出公开告警（2026-10-19 就因此误报了 Google API Key 与
//! AWS 临时密钥两条），重则 push protection 直接拦下推送。
//!
//! 脱敏模块（`mem_core::redaction`）必须用"形似真凭证"的输入才能验证正则覆盖面，
//! 因此约定：**厂商形态的凭证样本一律用 `concat!` 拆成两段再拼接**。运行时取值与
//! 拆分前逐字节相同，脱敏行为完全不变，而源码中不存在可被扫描器命中的连续字面量。
//!
//! # 覆盖范围
//!
//! 只覆盖**厂商形态**（Google / AWS / GitHub / GitLab / Slack / Stripe / OpenAI /
//! Anthropic / JWT / 私钥块）——这些是真实密钥会呈现的形状。通用的
//! `password=xxx`、`client_secret=xxx` 一类赋值不在范围内：它们熵值低、厂商不识别，
//! 且 `.env.example` 等示例文件里本就该出现占位值。
//!
//! # 判定范围
//!
//! 扫描 `git ls-files` 列出的已跟踪文件——与推送内容、GitHub 扫描内容同源。

use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;

/// 厂商密钥形态。
///
/// 长度下界取"足以被当成真凭证"的保守值：真实密钥只会更长，
/// 因此下界放宽只会多拆几处测试样本，不会漏掉真凭证。
///
/// 这些模式**自身**不会命中源码里的模式文本——模式里 `AIza`、`ghp_` 后面紧跟的
/// 是 `[` 而非密钥字符，所以门禁文件本身是自洽的。
const VENDOR_PATTERNS: &[(&str, &str)] = &[
    // Google API Key
    ("Google API Key", r"AIza[0-9A-Za-z\-_]{30,}"),
    // AWS Access Key ID（含临时凭证的 ASIA 前缀）
    (
        "AWS Access Key ID",
        r"(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}",
    ),
    // AWS Secret Access Key：出现在引号包裹的赋值里
    (
        "AWS Secret Access Key",
        r#"(?i)aws(.{0,20})?['"][0-9a-zA-Z/+]{40}['"]"#,
    ),
    // GitHub token 全家族（classic / OAuth / user / server / refresh）
    ("GitHub token", r"gh[pousr]_[0-9a-zA-Z]{36}"),
    // GitHub fine-grained PAT
    ("GitHub fine-grained PAT", r"github_pat_[0-9a-zA-Z_]{22,}"),
    // GitLab PAT
    ("GitLab PAT", r"glpat-[0-9a-zA-Z_\-]{20}"),
    // Slack token
    (
        "Slack token",
        r"xox[baprs]-[0-9]{10,13}-[0-9]{10,13}-[a-zA-Z0-9\-]{24,}",
    ),
    // Stripe key
    (
        "Stripe key",
        r"(?:sk|rk)_(?:test|live|prod)_[0-9a-zA-Z]{10,}",
    ),
    // OpenAI 传统 key 与 project key
    ("OpenAI key", r"sk-[a-zA-Z0-9]{48}"),
    ("OpenAI project key", r"sk-proj-[A-Za-z0-9_\-]{20,}"),
    // Anthropic key
    ("Anthropic key", r"sk-ant-[A-Za-z0-9_\-]{20,}"),
    // JSON Web Token
    (
        "JSON Web Token",
        r"ey[a-zA-Z0-9]{17,}\.ey[a-zA-Z0-9/\\_\-]{17,}\.[a-zA-Z0-9/\\_\-]{10,}",
    ),
    // Authorization: Bearer <token>
    ("Bearer token", r"Bearer [A-Za-z0-9\-._~+/]{20,}"),
    // PEM 私钥块（正文需达到真实密钥的量级）
    (
        "PEM private key",
        r"-----BEGIN[A-Z ]*PRIVATE KEY-----[\s\S-]{64,}?-----END",
    ),
];

/// 仓库根目录：`crates/mem-core` 往上两层。
fn repo_root() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent()?.parent()?;
    root.join("Cargo.toml")
        .is_file()
        .then(|| root.to_path_buf())
}

/// 已跟踪文件列表（与推送内容、GitHub 扫描内容同源）。
fn tracked_files(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("需要 git 才能列出已跟踪文件");
    assert!(
        output.status.success(),
        "git ls-files 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| root.join(s))
        .collect()
}

/// 命中位置（1 基行号）。
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|b| *b == b'\n').count() + 1
}

#[test]
fn no_contiguous_vendor_credential_literals() {
    let Some(root) = repo_root() else {
        panic!("无法定位仓库根目录");
    };

    let patterns: Vec<(&str, Regex)> = VENDOR_PATTERNS
        .iter()
        .map(|(name, regex)| (*name, Regex::new(regex).expect("模式必须可编译")))
        .collect();

    let mut violations: Vec<String> = Vec::new();
    for path in tracked_files(&root) {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        // 二进制文件（图片等）跳过
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (name, regex) in &patterns {
            for m in regex.find_iter(&text) {
                violations.push(format!(
                    "{}:{}: [{}]",
                    path.strip_prefix(&root).unwrap_or(&path).display(),
                    line_of(&text, m.start()),
                    name
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "以下位置出现了连续的厂商凭证字面量。\n\
         本仓库是公开仓库且开启了 secret scanning / push protection：\n\
         连续字面量会被判定为真实泄露——公开仓库的疑似密钥会被直接公示告警，\
         push protection 还会拦下推送。\n\
         请改成用 concat! 拆成两段再拼接（运行时取值不变，检测逻辑不受影响）。\n\n{}",
        violations.join("\n")
    );
}
