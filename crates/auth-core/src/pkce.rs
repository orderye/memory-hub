//! PKCE (Proof Key for Code Exchange) S256 强制校验实现（RFC 7636）

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

/// 校验 S256 PKCE 验证码
///
/// 要求：
/// - 强制使用 S256，拒绝 plain 纯文本方式
/// - 算法：`BASE64URL_NOPAD(SHA256(code_verifier)) == code_challenge`
pub fn verify_pkce_s256(code_verifier: &str, code_challenge: &str) -> bool {
    if code_verifier.len() < 43 || code_verifier.len() > 128 {
        return false;
    }

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();

    let expected_challenge = URL_SAFE_NO_PAD.encode(hash);
    use subtle::ConstantTimeEq;
    expected_challenge.len() == code_challenge.len()
        && expected_challenge
            .as_bytes()
            .ct_eq(code_challenge.as_bytes())
            .unwrap_u8()
            == 1
}

/// 为客户端或测试生成 S256 Challenge
pub fn compute_code_challenge_s256(code_verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let hash = hasher.finalize();
    URL_SAFE_NO_PAD.encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pkce_s256_verification() {
        // 标准 RFC 7636 示例测试向量
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_code_challenge_s256(verifier);

        assert!(verify_pkce_s256(verifier, &challenge));
        assert!(!verify_pkce_s256(
            "wrong_verifier_string_that_is_at_least_43_characters_long",
            &challenge
        ));
        assert!(!verify_pkce_s256("too_short", &challenge));
    }
}
