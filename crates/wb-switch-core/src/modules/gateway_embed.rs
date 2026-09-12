//! 内嵌网关可执行文件（单文件分发）。
//!
//! 目的：让用户只需要一个 `wb-switch.exe`，不必额外携带 `gateway.exe`。
//!
//! 做法：
//!   1. 构建期由 `build.rs` 把网关二进制 gzip 压缩后放到 OUT_DIR；
//!   2. 本模块用 `include_bytes!` 把它编进主程序；
//!   3. 运行时首次启动解压到 `~/.wb-switch/gateway/bin/gateway-<指纹>.exe`；
//!   4. 指纹取「内嵌数据长度 + 内容哈希」，因此升级后会自动落地新文件，
//!      不会复用旧版本。
//!
//! 若构建时未提供网关二进制，`EMBEDDED` 为 None，此时回退到
//! 「用户自备 gateway.exe / WB_SWITCH_GATEWAY_BIN」的旧路径。

use std::io::Read;
use std::path::PathBuf;

use crate::modules::config::store_dir;

/// 构建期写入的压缩数据；未提供网关时为 None。
/// 内嵌的压缩数据；由 build.rs 生成。
static EMBEDDED_GZ: Option<&'static [u8]> = include!(concat!(env!("OUT_DIR"), "/gateway_embed.rs"));

/// 是否内嵌了网关二进制。
pub fn has_embedded() -> bool {
    EMBEDDED_GZ.is_some_and(|d| !d.is_empty())
}

/// 内嵌数据的内容指纹（长度 + FNV-1a 哈希）。
///
/// 用于缓存文件名：内容变化 → 指纹变化 → 落地到新文件，
/// 避免升级后仍运行旧网关，也避免反复覆盖同一文件导致杀软反复扫描。
fn fingerprint(data: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{:x}-{:x}", data.len(), hash)
}

/// 内嵌网关可执行文件的落地目录。
pub fn embed_dir() -> PathBuf {
    store_dir().join("gateway").join("bin")
}

/// 把内嵌的压缩数据解压为可执行文件，返回路径。
///
/// 已存在且大小匹配时直接复用，不重复写盘。
pub fn materialize() -> Result<PathBuf, String> {
    let Some(gz) = EMBEDDED_GZ else {
        return Err("本构建未内嵌网关二进制".to_string());
    };
    if gz.is_empty() {
        return Err("内嵌网关数据为空".to_string());
    }

    let dir = embed_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;

    let target = dir.join(format!("gateway-{}.exe", fingerprint(gz)));

    // 已落地且大小合理 → 直接复用
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() > 1024 * 1024 {
            return Ok(target);
        }
    }

    let mut decoder = flate2::read::GzDecoder::new(gz);
    let mut buf = Vec::with_capacity(gz.len() * 3);
    decoder
        .read_to_end(&mut buf)
        .map_err(|e| format!("解压内嵌网关失败: {e}"))?;
    if buf.len() < 1024 * 1024 {
        return Err(format!("解压后的网关异常（仅 {} 字节）", buf.len()));
    }

    // 先写临时文件再改名，避免半成品被当成可执行文件使用
    let tmp = target.with_extension("exe.tmp");
    std::fs::write(&tmp, &buf).map_err(|e| format!("写入网关失败: {e}"))?;
    let _ = std::fs::remove_file(&target);
    std::fs::rename(&tmp, &target).map_err(|e| format!("替换网关失败: {e}"))?;

    Ok(target)
}

/// 清理历史版本的落地文件（保留当前指纹对应的那个）。
pub fn cleanup_old() {
    let Some(gz) = EMBEDDED_GZ else { return };
    let keep = format!("gateway-{}.exe", fingerprint(gz));
    let dir = embed_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with("gateway-") && name != keep && name.ends_with(".exe") {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_content_sensitive() {
        let a = fingerprint(b"hello");
        assert_eq!(a, fingerprint(b"hello"), "相同内容必须得到相同指纹");
        assert_ne!(a, fingerprint(b"hellp"), "内容不同必须得到不同指纹");
        assert_ne!(a, fingerprint(b"hello "), "长度不同必须得到不同指纹");
    }

    #[test]
    fn materialize_reports_when_nothing_embedded() {
        // 测试构建通常没有内嵌网关：应给出明确错误而不是 panic
        if !has_embedded() {
            assert!(materialize().is_err());
        }
    }
}
