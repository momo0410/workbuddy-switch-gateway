//! CodeBuddy CN IDE Safe Storage 注入（仅 CN）。
//!
//! 把账号会话 JSON 加密写入 `state.vscdb` 的 ItemTable：
//! `secret://{"extensionId":"tencent-cloud.coding-copilot","key":"planning-genie.new.accessTokencn"}`
//!
//! 加密模型对齐 Chromium/Electron Safe Storage（Windows）：
//! Local State `os_crypt.encrypted_key` + DPAPI → AES-256-GCM `v10`

use std::path::{Path, PathBuf};

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, AeadCore, OsRng};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose, Engine as _};
use rusqlite::Connection;

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

const V10_PREFIX: &[u8] = b"v10";

pub const SECRET_EXTENSION_ID: &str = "tencent-cloud.coding-copilot";
pub const SECRET_KEY: &str = "planning-genie.new.accessTokencn";

/// ItemTable 完整 key。
pub fn secret_storage_item_key() -> String {
    format!(
        r#"secret://{{"extensionId":"{}","key":"{}"}}"#,
        SECRET_EXTENSION_ID, SECRET_KEY
    )
}

pub fn codebuddy_cn_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("CodeBuddy CN"))
}

pub fn codebuddy_cn_state_db_path() -> Option<PathBuf> {
    codebuddy_cn_data_dir().map(|d| d.join("User").join("globalStorage").join("state.vscdb"))
}

pub fn resolve_state_db_path(user_data_dir: Option<&Path>) -> Result<PathBuf, String> {
    let root = match user_data_dir {
        Some(p) => p.to_path_buf(),
        None => codebuddy_cn_data_dir().ok_or_else(|| "无法定位 CodeBuddy CN 数据目录".to_string())?,
    };
    let candidates = [
        root.join("User").join("globalStorage").join("state.vscdb"),
        root.join("globalStorage").join("state.vscdb"),
        root.join("state.vscdb"),
    ];
    if let Some(path) = candidates.iter().find(|p| p.exists()) {
        return Ok(path.clone());
    }
    let preferred = candidates[0].clone();
    if let Some(parent) = preferred.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 globalStorage 失败: {e}"))?;
    }
    Ok(preferred)
}

fn data_root_from_db(db_path: &Path) -> Result<&Path, String> {
    db_path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or_else(|| format!("无法从 db 路径推断数据目录: {}", db_path.display()))
}

fn decode_buffer_data(buffer: &serde_json::Value) -> Result<Vec<u8>, String> {
    let data_arr = buffer["data"]
        .as_array()
        .ok_or_else(|| "Secret data is not in Buffer format".to_string())?;
    let mut encrypted_bytes = Vec::with_capacity(data_arr.len());
    for (idx, v) in data_arr.iter().enumerate() {
        let n = v
            .as_u64()
            .ok_or_else(|| format!("Secret data element at index {idx} is not an integer"))?;
        if n > 255 {
            return Err(format!(
                "Secret data element at index {idx} is out of range ({n} > 255)"
            ));
        }
        encrypted_bytes.push(n as u8);
    }
    Ok(encrypted_bytes)
}

fn encode_secret_buffer(encrypted: Vec<u8>) -> Result<String, String> {
    let buffer_json = serde_json::json!({
        "type": "Buffer",
        "data": encrypted
    });
    serde_json::to_string(&buffer_json).map_err(|e| format!("Failed to serialize Buffer: {e}"))
}

fn detect_prefix(encrypted: &[u8]) -> Option<&'static str> {
    if encrypted.starts_with(V10_PREFIX) {
        Some("v10")
    } else {
        None
    }
}

fn get_local_state_path(data_root: &Path) -> Result<PathBuf, String> {
    let path = data_root.join("Local State");
    if path.exists() {
        Ok(path)
    } else {
        Err(format!("未找到 CodeBuddy CN Local State: {}", path.display()))
    }
}

fn dpapi_decrypt(encrypted: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let mut data_in = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        CryptUnprotectData(
            &mut data_in,
            None,
            None,
            None,
            None,
            0,
            &mut data_out,
        )
        .map_err(|e| format!("DPAPI CryptUnprotectData failed: {e}"))?;
        if data_out.pbData.is_null() || data_out.cbData == 0 {
            return Err("DPAPI returned empty data".to_string());
        }
        let slice = std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize);
        let result = slice.to_vec();
        let _ = LocalFree(HLOCAL(data_out.pbData as _));
        Ok(result)
    }
}

fn get_windows_encryption_key(data_root: &Path) -> Result<Vec<u8>, String> {
    let local_state = get_local_state_path(data_root)?;
    let text = std::fs::read_to_string(&local_state)
        .map_err(|e| format!("读取 Local State 失败: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("解析 Local State 失败: {e}"))?;
    let encrypted_key_b64 = json["os_crypt"]["encrypted_key"]
        .as_str()
        .ok_or_else(|| "Local State 缺少 os_crypt.encrypted_key".to_string())?;
    let encrypted_key_bytes = general_purpose::STANDARD
        .decode(encrypted_key_b64)
        .map_err(|e| format!("Base64 decode failed for encrypted_key: {e}"))?;
    if encrypted_key_bytes.len() < 6 {
        return Err("encrypted_key data too short".to_string());
    }
    let prefix = String::from_utf8_lossy(&encrypted_key_bytes[..5]);
    if prefix != "DPAPI" {
        return Err(format!("encrypted_key prefix is not DPAPI, got: {prefix}"));
    }
    dpapi_decrypt(&encrypted_key_bytes[5..])
}

fn decrypt_windows_gcm_v10(key: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, String> {
    if encrypted.len() < 31 {
        return Err("ciphertext too short for AES-GCM".to_string());
    }
    if &encrypted[..3] != V10_PREFIX {
        return Err(format!(
            "Unexpected ciphertext prefix: {:?}",
            &encrypted[..3]
        ));
    }
    let nonce_bytes = &encrypted[3..15];
    let ciphertext = &encrypted[15..];
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("AES-GCM decryption failed: {e}"))
}

fn encrypt_windows_gcm_v10(key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(GenericArray::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| format!("AES-GCM encryption failed: {e}"))?;
    let mut result = Vec::with_capacity(3 + nonce.len() + ciphertext.len());
    result.extend_from_slice(V10_PREFIX);
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

fn decrypt_secret_payload(encrypted: &[u8], data_root: &Path) -> Result<Vec<u8>, String> {
    let key = get_windows_encryption_key(data_root)?;
    decrypt_windows_gcm_v10(&key, encrypted)
}

fn encrypt_secret_payload(
    plaintext: &[u8],
    preferred_prefix: Option<&str>,
    data_root: &Path,
) -> Result<Vec<u8>, String> {
    let _ = preferred_prefix;
    let key = get_windows_encryption_key(data_root)?;
    encrypt_windows_gcm_v10(&key, plaintext)
}

fn decode_secret_storage_value(raw_value: &str, data_root: &Path) -> Result<String, String> {
    let parsed: serde_json::Value = match serde_json::from_str(raw_value) {
        Ok(value) => value,
        Err(_) => return Ok(raw_value.to_string()),
    };
    if parsed.get("data").is_some() {
        let encrypted_bytes = decode_buffer_data(&parsed)?;
        let decrypted = decrypt_secret_payload(&encrypted_bytes, data_root)?;
        return String::from_utf8(decrypted)
            .map_err(|e| format!("Decrypted data is not valid UTF-8: {e}"));
    }
    if let Some(value) = parsed.as_str() {
        return Ok(value.to_string());
    }
    Ok(raw_value.to_string())
}

/// 读取并解密 CodeBuddy CN 当前登录 secret（明文 JSON 字符串）。
pub fn read_codebuddy_cn_secret(user_data_dir: Option<&Path>) -> Result<Option<String>, String> {
    let db_path = resolve_state_db_path(user_data_dir)?;
    if !db_path.exists() {
        return Ok(None);
    }
    let data_root = data_root_from_db(&db_path)?.to_path_buf();
    let conn = Connection::open(&db_path)
        .map_err(|e| format!("打开 state.vscdb 失败: {e}"))?;
    let key = secret_storage_item_key();
    let raw_value: Option<String> = match conn.query_row(
        "SELECT value FROM ItemTable WHERE key = ?1",
        [key.as_str()],
        |row| row.get(0),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(format!("查询 CodeBuddy CN secret 失败: {err}")),
    };
    match raw_value {
        Some(value) => decode_secret_storage_value(&value, &data_root).map(Some),
        None => Ok(None),
    }
}

/// 加密并写入 CodeBuddy CN secret。
pub fn inject_codebuddy_cn_secret(
    plaintext: &str,
    user_data_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let db_path = resolve_state_db_path(user_data_dir)?;
    let data_root = data_root_from_db(&db_path)?.to_path_buf();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 state.vscdb 父目录失败: {e}"))?;
    }
    let conn = Connection::open(&db_path).map_err(|e| format!("打开 state.vscdb 失败: {e}"))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)",
        [],
    )
    .map_err(|e| format!("初始化 ItemTable 失败: {e}"))?;

    let db_key = secret_storage_item_key();
    let existing_prefix: Option<String> = match conn.query_row(
        "SELECT value FROM ItemTable WHERE key = ?",
        [db_key.as_str()],
        |row| row.get::<_, String>(0),
    ) {
        Ok(val) => {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&val) {
                if let Ok(bytes) = decode_buffer_data(&parsed) {
                    detect_prefix(&bytes).map(|s| s.to_string())
                } else {
                    None
                }
            } else {
                None
            }
        }
        Err(_) => None,
    };

    let encrypted =
        encrypt_secret_payload(plaintext.as_bytes(), existing_prefix.as_deref(), &data_root)?;
    let buffer_str = encode_secret_buffer(encrypted)?;
    conn.execute(
        "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?, ?)",
        rusqlite::params![db_key, buffer_str],
    )
    .map_err(|e| format!("写入 state.vscdb 失败: {e}"))?;

    // 写后校验：行存在且为 Buffer JSON
    let written: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?",
            [db_key.as_str()],
            |row| row.get(0),
        )
        .map_err(|e| format!("写后校验失败: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&written).map_err(|e| format!("写后校验 JSON 失败: {e}"))?;
    if parsed.get("type").and_then(|v| v.as_str()) != Some("Buffer") {
        return Err("写后校验失败：value 不是 Buffer".to_string());
    }
    Ok(db_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_matches_verified_format() {
        let key = secret_storage_item_key();
        assert_eq!(
            key,
            r#"secret://{"extensionId":"tencent-cloud.coding-copilot","key":"planning-genie.new.accessTokencn"}"#
        );
    }

    #[test]
    fn data_dir_contains_codebuddy_cn() {
        let Some(dir) = codebuddy_cn_data_dir() else {
            return;
        };
        let s = dir.to_string_lossy();
        assert!(
            s.contains("CodeBuddy CN"),
            "data dir should contain CodeBuddy CN: {s}"
        );
    }

    #[test]
    fn state_db_path_ends_with_state_vscdb() {
        let Some(db) = codebuddy_cn_state_db_path() else {
            return;
        };
        assert!(db.ends_with("state.vscdb"));
        assert!(db.to_string_lossy().contains("globalStorage"));
    }

    #[test]
    fn resolve_prefers_existing_candidate() {
        let dir = std::env::temp_dir().join(format!(
            "wb-cn-ide-path-test-{}",
            uuid::Uuid::new_v4()
        ));
        let db = dir.join("User").join("globalStorage").join("state.vscdb");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        std::fs::write(&db, b"").unwrap();
        let resolved = resolve_state_db_path(Some(&dir)).unwrap();
        assert_eq!(resolved, db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
