//! 诊断探针：读取并解析网关 state.json，打印账号数。
//!
//! 用法：`probe_state <state.json 路径>`
//!
//! 路径刻意由命令行给出。早期版本把 A/B 对照夹具的绝对路径
//! （`D:\workbuddy2api\_ab\fixture\data\state.json`）写死在本文件里，
//! 换机器或换目录后必然 panic —— 该夹具目录本身也早已随 `_ab/` 一起删除。
use wb_switch_gateway::pool::StateFile;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("用法: probe_state <state.json 路径>");
        std::process::exit(2);
    };

    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("读取 {path} 失败: {e}");
            std::process::exit(1);
        }
    };

    println!("bytes={}", raw.len());
    match serde_json::from_slice::<StateFile>(&raw) {
        Ok(sf) => println!("OK accounts={}", sf.accounts.len()),
        Err(e) => println!("ERR {e}"),
    }
}
