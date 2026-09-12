//! 参考网关：最小可运行实例，仅用于 A/B 对照与手工验证。
//!
//! 生产分发走 `wb-switch-core` 的 `build.rs` 内嵌路径（见该 crate 文档）。
//! 本二进制单独存在是为了能在不启动 Tauri 桌面的情况下，
//! 与 Go 版 gateway.exe 做同配置的 HTTP 行为对比。
//!
//! 用法：`wb2g-ref [port]`，并用 `WB2A_AUTH_DIR` / `WB2A_API_KEY` 指定配置，
//! 以便与 Go 网关（`-config`）指向同一份凭证目录。
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wb_switch_gateway::auth;
use wb_switch_gateway::pool::Pool;
use wb_switch_gateway::server::{router, AppState};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(7864);

    // 与 Go 参考网关同配置：同一份 auths、同一份 state.json、同一个 api_key。
    let auth_dir = std::env::var("WB2A_AUTH_DIR").unwrap_or_else(|_| "./auths".into());
    let state_file = std::env::var("WB2A_STATE_FILE").unwrap_or_else(|_| "./data/state.json".into());
    let api_key = std::env::var("WB2A_API_KEY").unwrap_or_default();

    // 顺序与 Go 侧 main() 一致：先建池并载入 state（恢复 credits/冷却），
    // 再 SyncToDir 灌入完整凭证 —— 这样 add() 只换凭证、保留已恢复的运行态。
    let mut pool = Pool::new(state_file);
    pool.load_state();
    let loaded_state = pool.loaded_from_state();

    let accounts = auth::load_dir(&auth_dir).unwrap_or_default();
    let n = accounts.len();
    for a in accounts {
        pool.add(a);
    }
    if n == 0 {
        eprintln!("warning: 未从 {auth_dir} 加载到任何凭证");
    }
    println!(
        "state restored={loaded_state} (state_file={})",
        std::env::var("WB2A_STATE_FILE").unwrap_or_else(|_| "./data/state.json".into())
    );

    let state = Arc::new(AppState {
        pool: Mutex::new(pool),
        api_key,
        max_rotate: 3,
        soft_cooldown: Duration::from_secs(60),
        refresh_skew: Duration::from_secs(600),
        redis_mode: String::new(),
        sticky_count: 0,
    });

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind failed");
    println!("rust gateway listening on http://127.0.0.1:{port} (accounts={n})");
    axum::serve(listener, app).await.unwrap();
}
