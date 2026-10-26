//! Agent `client_secret` 的落库摘要。
//!
//! 历史实现是 `sha256(agent_id:secret)`：无盐、单次哈希的快速摘要，且用 `==` 比对。
//! `agents` 表一旦泄露，攻击者可用 GPU 对每个 Agent 逐条离线爆破——这与账户口令
//! 早已采用 Argon2id 的处置标准不一致。现改为 Argon2id（随机盐 + 内存硬度参数），
//! 并让"注册令牌/口令/密钥"三类凭据在散列强度上对齐。
//!
//! 存量行存的是旧摘要（64 位小写 hex）。库里没有明文，无法离线重写，
//! 因此采用业界标准的**校验即升级**：旧格式校验通过后，调用方据
//! [`is_legacy_digest`] 立刻把新摘要回写，下一次起就是 Argon2id。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// 生成新摘要（Argon2id，PHC 字符串）的异步包装。
///
/// Argon2 是 64MiB/t=3 的纯 CPU 计算，直接跑在 async fn 里会占死一个 tokio
/// worker 线程；这里丢进 `spawn_blocking` 线程池。参数用 `String` 拥有所有权
/// 传入以满足 `Send + 'static`，避免借用进闭包。
pub async fn hash_secret_blocking(agent_id: String, secret: String) -> String {
    tokio::task::spawn_blocking(move || hash_secret(&agent_id, &secret))
        .await
        .expect("spawn_blocking 任务不应被取消")
}

/// 校验凭据的异步包装（同 [`hash_secret_blocking`] 的理由）。
pub async fn verify_secret_blocking(agent_id: String, secret: String, stored: String) -> bool {
    tokio::task::spawn_blocking(move || verify_secret(&agent_id, &secret, &stored))
        .await
        .expect("spawn_blocking 任务不应被取消")
}

/// OWASP 基准参数的 Argon2id 实例：64MiB 内存硬度、3 轮迭代、1 并行度
pub fn owasp_argon2() -> Argon2<'static> {
    let params = Params::new(65536, 3, 1, None).expect("Valid OWASP Argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// 旧摘要形态：`sha256(agent_id:secret)` 的小写 hex
pub fn is_legacy_digest(stored: &str) -> bool {
    stored.len() == 64
        && stored
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 生成新摘要（Argon2id，PHC 字符串）
pub fn hash_secret(agent_id: &str, secret: &str) -> String {
    let mut raw = [0u8; 16];
    getrandom::getrandom(&mut raw).expect("CSPRNG failure");
    let salt = SaltString::encode_b64(&raw).expect("16 字节盐必然可 base64 编码");
    owasp_argon2()
        .hash_password(material(agent_id, secret).as_bytes(), &salt)
        .expect("Argon2 散列失败")
        .to_string()
}

/// 校验凭据。旧格式走常量时间的摘要比对，其余按 PHC 解析校验；
/// 结构非法一律判否（不 panic、不接受"空哈希=通过"）。
pub fn verify_secret(agent_id: &str, secret: &str, stored: &str) -> bool {
    if is_legacy_digest(stored) {
        return legacy_digest(agent_id, secret)
            .as_bytes()
            .ct_eq(stored.as_bytes())
            .unwrap_u8()
            == 1;
    }
    match PasswordHash::new(stored) {
        Ok(parsed) => Argon2::default()
            .verify_password(material(agent_id, secret).as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 把 agent_id 折进散列输入，避免不同 Agent 之间相同口令产生相同摘要（防跨行对照）
fn material(agent_id: &str, secret: &str) -> String {
    format!("{}:{}", agent_id, secret)
}

fn legacy_digest(agent_id: &str, secret: &str) -> String {
    let mut h = Sha256::new();
    h.update(agent_id.as_bytes());
    h.update(b":");
    h.update(secret.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_digest_is_argon2_and_verifies() {
        let stored = hash_secret("agent-a", "s3cret-value");
        assert!(
            stored.starts_with("$argon2id$"),
            "应为 PHC 字符串: {}",
            stored
        );
        assert!(
            stored.contains("m=65536,t=3,p=1"),
            "应符合 OWASP 基准参数: {}",
            stored
        );
        assert!(!is_legacy_digest(&stored));
        assert!(verify_secret("agent-a", "s3cret-value", &stored));
        assert!(!verify_secret("agent-a", "wrong", &stored));
        // 同一口令换 Agent 不复用摘要
        assert!(!verify_secret("agent-b", "s3cret-value", &stored));
    }

    #[test]
    fn test_same_secret_yields_different_salts() {
        assert_ne!(hash_secret("a", "x"), hash_secret("a", "x"));
    }

    #[test]
    fn test_legacy_digest_still_accepted_for_upgrade_path() {
        let legacy = legacy_digest("agent-a", "s3cret-value");
        assert!(is_legacy_digest(&legacy));
        assert!(verify_secret("agent-a", "s3cret-value", &legacy));
        assert!(!verify_secret("agent-a", "wrong", &legacy));
    }

    #[test]
    fn test_malformed_stored_hash_is_rejected_not_panic() {
        for junk in ["", "x", "$argon2id$garbage", "not-a-hash"] {
            assert!(!verify_secret("agent-a", "s3cret", junk));
        }
    }

    #[tokio::test]
    async fn test_blocking_wrappers_preserve_semantics() {
        let stored = hash_secret_blocking("agent-a".to_string(), "s3cret-value".to_string()).await;
        assert!(stored.starts_with("$argon2id$"));
        assert!(
            verify_secret_blocking(
                "agent-a".to_string(),
                "s3cret-value".to_string(),
                stored.clone()
            )
            .await
        );
        assert!(!verify_secret_blocking("agent-a".to_string(), "wrong".to_string(), stored).await);
    }

    /// 并发多个 verify 不死锁：spawn_blocking 闭包不持锁、不回进 async 上下文，
    /// 池容量耗尽时也只是排队而不是互等。
    #[tokio::test]
    async fn test_concurrent_verify_no_deadlock() {
        let stored = hash_secret("agent-a", "s3cret-value");
        let mut handles = Vec::new();
        for _ in 0..8 {
            handles.push(tokio::spawn(verify_secret_blocking(
                "agent-a".to_string(),
                "s3cret-value".to_string(),
                stored.clone(),
            )));
        }
        for h in handles {
            assert!(h.await.unwrap());
        }
    }
}
