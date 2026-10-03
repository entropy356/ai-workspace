//! age 密钥管理（规格 §1、§5）：daemon 进程内生成 age 密钥对，私钥永不落盘

use age::x25519::Identity;
use std::str::FromStr;

const SECRET_KEY_HRP: &str = "age-secret-key-";

/// 生成密钥对：返回（堆上 Identity 对象用于解密，其内部 Secret 在 drop 时 zeroize；
/// 原始 32 字节镜像存入 SensitivePage；公钥字符串交给用户加密 PAT）
pub fn generate() -> Result<(Identity, [u8; 32], String), String> {
    let id = Identity::generate();
    let pubkey = id.to_public().to_string();
    let secret = id.to_string(); // SecretString（bech32，大写）
    use age::secrecy::ExposeSecret;
    let raw = raw_from_bech32(secret.expose_secret()).ok_or("age 私钥 bech32 解码失败")?;
    Ok((id, raw, pubkey))
}

/// bech32 串 → 原始 32 字节（与 age util::parse_bech32 逻辑一致）
pub fn raw_from_bech32(s: &str) -> Option<[u8; 32]> {
    use bech32::FromBase32;
    let (hrp, data, variant) = bech32::decode(s).ok()?;
    if variant != bech32::Variant::Bech32 || hrp != SECRET_KEY_HRP {
        return None;
    }
    let bytes: Vec<u8> = Vec::from_base32(&data).ok()?;
    bytes.try_into().ok()
}

/// 原始 32 字节 → Identity（用于从 SensitivePage 重建，测试与恢复路径）
pub fn identity_from_raw(raw: &[u8; 32]) -> Result<Identity, String> {
    use bech32::ToBase32;
    let enc = bech32::encode(SECRET_KEY_HRP, raw.to_base32(), bech32::Variant::Bech32)
        .map_err(|e| format!("bech32 编码失败: {e}"))?;
    Identity::from_str(&enc.to_uppercase()).map_err(|e| e.to_string())
}

/// 用公钥加密明文（测试辅助；生产路径由用户使用 age CLI 自行加密）
#[cfg_attr(not(test), allow(dead_code))]
pub fn encrypt_for_test(pubkey: &str, plaintext: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let recipient: age::x25519::Recipient = pubkey.parse().map_err(|e| format!("{e}"))?;
    let encryptor =
        age::Encryptor::with_recipients(vec![Box::new(recipient)]).ok_or("无接收者")?;
    let mut out = Vec::new();
    let mut w = encryptor.wrap_output(&mut out).map_err(|e| format!("{e}"))?;
    w.write_all(plaintext).map_err(|e| format!("{e}"))?;
    w.finish().map_err(|e| format!("{e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let (_id, raw, pubkey) = generate().unwrap();
        assert_eq!(pubkey.len() >= 20 && pubkey.starts_with("age1"), true);
        let id2 = identity_from_raw(&raw).unwrap();
        assert_eq!(id2.to_public().to_string(), pubkey);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let (id, _raw, pubkey) = generate().unwrap();
        let enc = encrypt_for_test(&pubkey, b"ghp_test123").unwrap();
        let decryptor = match age::Decryptor::new(&enc[..]) {
            Ok(age::Decryptor::Recipients(d)) => d,
            _ => panic!("应为 Recipients 格式"),
        };
        use std::io::Read;
        let mut pt = Vec::new();
        decryptor
            .decrypt(std::iter::once(&id as &dyn age::Identity))
            .unwrap()
            .read_to_end(&mut pt)
            .unwrap();
        assert_eq!(pt, b"ghp_test123");
    }
}
