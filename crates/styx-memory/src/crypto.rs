//! Nebula 密码学原语：与 `nebula-crypto` **逐位对齐**。
//!
//! 这不是"参考实现"而是"互通实现"——同样的 Argon2id 参数、同样的 HKDF info、
//! 同样的 HMAC、同样的 ChaCha20-Poly1305 装载顺序。任何一处不同都会导致
//! 握手失败或 AEAD 认证失败，因此这里刻意照抄 Nebula 的常量。
//!
//! | 原语 | 参数 |
//! |---|---|
//! | 主密钥 | Argon2id(m=19 MiB, t=2, p=1, out=32B) |
//! | 会话密钥 | HKDF-SHA256(master, info = `"nebula/session/v2"` ‖ challenge) |
//! | 认证证明 | HMAC-SHA256(session_key, challenge) |
//! | 帧加密 | ChaCha20-Poly1305，`nonce(12) ‖ ct ‖ tag(16)` |

use argon2::Argon2;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// 密钥 / 摘要长度（256 bit）。
pub const KEY_LEN: usize = 32;
/// AEAD nonce 长度。
pub const NONCE_LEN: usize = 12;
/// AEAD 认证标签长度。
pub const TAG_LEN: usize = 16;
/// 盐长度。
pub const SALT_LEN: usize = 16;

/// 从密码派生主密钥（Argon2id，OWASP 推荐参数）。
///
/// 参数与 `nebula-crypto::derive_master_key` 完全一致；
/// 派生一次约 50~200 ms，对交互式连接是可接受的成本。
pub fn derive_master_key(password: &str, salt: &[u8; SALT_LEN]) -> [u8; KEY_LEN] {
    let params =
        argon2::Params::new(19 * 1024, 2, 1, Some(KEY_LEN)).expect("固定的 argon2 参数一定合法");
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .expect("固定参数下 argon2 不会失败");
    key
}

/// HKDF-SHA256 派生（`salt = None`，即 RFC 5869 的零盐）。
pub fn hkdf_sha256(master: &[u8; KEY_LEN], info: &[u8]) -> [u8; KEY_LEN] {
    let hk = Hkdf::<Sha256>::new(None, master);
    let mut out = [0u8; KEY_LEN];
    hk.expand(info, &mut out)
        .expect("32 字节一定是合法的 HKDF 输出长度");
    out
}

/// HMAC-SHA256。
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; KEY_LEN] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut result = [0u8; KEY_LEN];
    result.copy_from_slice(&out);
    result
}

/// 常量时间比较（避免通过时序侧信道泄露 proof 差异）。
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 生成密码学安全随机 nonce。
pub fn random_nonce() -> [u8; NONCE_LEN] {
    let mut out = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut out);
    out
}

/// ChaCha20-Poly1305 加密，输出 `nonce ‖ ciphertext ‖ tag`。
pub fn seal(key: &[u8; KEY_LEN], plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let nonce = random_nonce();
    let ct = cipher
        .encrypt(
            &nonce.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .expect("AEAD 加密不会失败");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

/// ChaCha20-Poly1305 解密，输入 `nonce ‖ ciphertext ‖ tag`。
pub fn open(key: &[u8; KEY_LEN], blob: &[u8], aad: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err("密文过短".into());
    }
    let (nonce_bytes, rest) = blob.split_at(NONCE_LEN);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(nonce_bytes);
    let cipher = ChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(&nonce.into(), Payload { msg: rest, aad })
        .map_err(|_| "AEAD 认证失败".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let key = [7u8; KEY_LEN];
        let aad = b"nebula/frame/up\x00\x00\x00\x00\x00\x00\x00\x00";
        let blob = seal(&key, b"hello nebula", aad);
        assert_eq!(open(&key, &blob, aad).unwrap(), b"hello nebula");
    }

    #[test]
    fn wrong_aad_or_key_fails_authentication() {
        let key = [7u8; KEY_LEN];
        let blob = seal(&key, b"payload", b"aad-1");
        assert!(open(&key, &blob, b"aad-2").is_err());
        let other = [8u8; KEY_LEN];
        assert!(open(&other, &blob, b"aad-1").is_err());
    }

    #[test]
    fn nonce_is_random_per_call() {
        let key = [1u8; KEY_LEN];
        let a = seal(&key, b"x", b"");
        let b = seal(&key, b"x", b"");
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN]);
        assert_ne!(a, b);
    }

    #[test]
    fn short_blob_rejected() {
        assert!(open(&[0u8; KEY_LEN], &[0u8; 10], b"").is_err());
    }

    #[test]
    fn ct_eq_is_correct() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn derivation_is_deterministic_and_salt_sensitive() {
        let salt = [3u8; SALT_LEN];
        let k1 = derive_master_key("pw", &salt);
        let k2 = derive_master_key("pw", &salt);
        assert_eq!(k1, k2);
        let k3 = derive_master_key("pw", &[4u8; SALT_LEN]);
        assert_ne!(k1, k3);
        let k4 = derive_master_key("pw2", &salt);
        assert_ne!(k1, k4);
    }

    #[test]
    fn hkdf_domain_separation() {
        let master = [9u8; KEY_LEN];
        let a = hkdf_sha256(&master, b"nebula/session/v2");
        let b = hkdf_sha256(&master, b"nebula/frame/up");
        assert_ne!(a, b);
        assert_eq!(a, hkdf_sha256(&master, b"nebula/session/v2"));
    }

    #[test]
    fn hmac_matches_rfc4231_style_vector() {
        // 用已知向量确认我们的 HMAC-SHA256 是标准的
        let mac = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}
