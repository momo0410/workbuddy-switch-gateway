//! 构建脚本：把网关可执行文件压缩后内嵌，实现单文件分发。
//!
//! 查找顺序（任一命中即内嵌）：
//!   1. 环境变量 WB_SWITCH_GATEWAY_BIN
//!   2. crates/wb-switch-core/embedded/gateway.exe（约定目录）
//!   3. 仓库根 dist/gateway.exe
//!
//! 都找不到时生成 `None`，程序仍可编译，只是不内嵌网关
//!（此时回退到用户自备 gateway.exe 的旧方式）。

use std::io::Write;
use std::path::{Path, PathBuf};

fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("WB_SWITCH_GATEWAY_BIN") {
        if !p.trim().is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let name = if cfg!(windows) { "gateway.exe" } else { "gateway" };
    out.push(manifest.join("embedded").join(name));
    // 仓库根 dist/（crates/wb-switch-core → ../..）
    if let Some(root) = manifest.parent().and_then(Path::parent) {
        out.push(root.join("dist").join(name));
    }
    out
}

fn main() {
    println!("cargo:rerun-if-env-changed=WB_SWITCH_GATEWAY_BIN");
    // 无条件注册所有候选路径的监听：这样「先构建时没有网关、后来补上」
    // 也能触发 build.rs 重跑。只在命中路径上注册会导致永远内嵌不进去。
    for c in candidate_paths() {
        println!("cargo:rerun-if-changed={}", c.display());
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let gen_file = out_dir.join("gateway_embed.rs");

    let mut chosen: Option<PathBuf> = None;
    for c in candidate_paths() {
        if c.is_file() {
            if let Ok(meta) = std::fs::metadata(&c) {
                if meta.len() > 1024 * 1024 {
                    chosen = Some(c);
                    break;
                }
            }
        }
    }

    let Some(path) = chosen else {
        std::fs::write(
            &gen_file,
            "// 未内嵌网关二进制（构建时未找到 gateway 可执行文件）\nNone::<&[u8]>",
        )
        .expect("写入 gateway_embed.rs 失败");
        println!("cargo:warning=未找到网关二进制，本次构建不内嵌（可设 WB_SWITCH_GATEWAY_BIN 指定）");
        return;
    };

    println!("cargo:rerun-if-changed={}", path.display());
    let raw = std::fs::read(&path).expect("读取网关二进制失败");

    // gzip 压缩：体积可降约 60%
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&raw).expect("压缩网关失败");
    let gz = enc.finish().expect("压缩网关失败");

    std::fs::write(
        &gen_file,
        format!(
            "// 由 build.rs 生成：内嵌网关（原始 {} 字节 → 压缩 {} 字节）\nSome(&{:?})",
            raw.len(),
            gz.len(),
            gz
        ),
    )
    .expect("写入 gateway_embed.rs 失败");

    println!(
        "cargo:warning=已内嵌网关: {} ({} MB → {} MB)，仅需分发单个可执行文件",
        path.display(),
        raw.len() / 1048576,
        gz.len() / 1048576
    );
}

