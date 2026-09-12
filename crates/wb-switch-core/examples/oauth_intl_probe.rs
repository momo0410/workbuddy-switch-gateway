//! 国际版 OAuth 链路一次性验证（**只读，不写入账号库**）。
//!
//! 用途：确认「国际版登录页授权后，会不会把 token 回写到插件缓存
//! `/v2/plugin/auth/token`」——这是 `oauth.rs` 支持国际版的唯一未验证前提。
//!
//! 运行：
//!   cargo run -p wb-switch-core --example oauth_intl_probe
//!   # 自定义轮询窗口（秒），默认 1800
//!   $env:WB_PROBE_TIMEOUT_SEC = "3600"; cargo run -p wb-switch-core --example oauth_intl_probe
//!
//! **注意登录方式**：国际版（workbuddy.ai）没有扫码，走的是 Keycloak realm 的
//! 联合登录——Google / GitHub / X 三方授权，外加账号密码、企业 SSO 与 Tencent OneID。
//! 因此需要在浏览器里**完成一次三方授权**，不是扫码。
//!
//! **token 端点一次性消费**：取到一次后再次轮询会退回 `11217 login ing`。
//! 本探针只会轮询到第一次成功为止，之后立刻停手（不会把凭证烧掉）。
//!
//! 它只做三件事：
//!   1. `POST /v2/plugin/auth/state?platform=workbuddy-ai` 申请 state 并打印授权页
//!   2. 轮询 `GET /v2/plugin/auth/token`（原始响应，不做入库）
//!   3. 拿到 token 后调一次 `GET /v2/plugin/login/account` 验证 uid 可取
//!
//! 与 `oauth::oauth_poll` 的区别：本探针**不调用 `save_collected_account`**，
//! 因此不会改动 `~/.wb-switch/accounts.json`。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wb_switch_core::modules::config::{http_request, Region, WORKBUDDY_API_PREFIX};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_TIMEOUT_SEC: u64 = 1800;

fn probe_timeout() -> Duration {
    let secs = std::env::var("WB_PROBE_TIMEOUT_SEC")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SEC);
    Duration::from_secs(secs)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let region = Region::Intl;
    let timeout = probe_timeout();
    println!("=== 国际版 OAuth 链路验证（只读，不写账号库）===");

    // 1. 申请 state
    let state_url = format!(
        "{}{WORKBUDDY_API_PREFIX}/auth/state?platform={}",
        region.api_endpoint(),
        region.oauth_platform()
    );
    println!("1. POST {state_url}");
    let resp = http_request(&state_url, "POST", Some(serde_json::json!({})), None).await;
    let state = resp
        .get("data")
        .and_then(|d| d.get("state"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if state.is_empty() {
        println!("   ✗ 未取到 state，原始响应：{resp}");
        return;
    }
    let auth_url = resp
        .get("data")
        .and_then(|d| d.get("authUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("(无 authUrl)");
    println!("   ✓ state = {state}");
    println!();
    println!("   请打开下面的链接，用 Google / GitHub / X 完成授权（国际版无扫码）：");
    println!("   {auth_url}");
    println!();

    // 2. 轮询 token（原始响应，不入库）
    let token_url = format!(
        "{}{WORKBUDDY_API_PREFIX}/auth/token?state={state}",
        region.api_endpoint()
    );
    println!("2. 轮询 {token_url}（最多 {} 秒）", timeout.as_secs());
    let started = Instant::now();
    let data = loop {
        if started.elapsed() > timeout {
            println!("   ✗ 超时：{} 秒内未拿到 token", timeout.as_secs());
            println!("   → 国际版登录页没有把授权结果回写插件 token 缓存，");
            println!("     说明该区域需要走 /console/auth/login 那条链路。");
            return;
        }
        let resp = http_request(&token_url, "GET", None, None).await;
        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code == 0 || code == 200 {
            if let Some(data) = resp.get("data").filter(|d| d.is_object()) {
                let has_token = data
                    .get("accessToken")
                    .or_else(|| data.get("access_token"))
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.is_empty());
                if has_token {
                    println!("   ✓ 第 {} 秒拿到 token", started.elapsed().as_secs());
                    break data.clone();
                }
            }
        }
        println!(
            "   [{:>3}s] code={code} {}",
            started.elapsed().as_secs(),
            resp.get("msg").and_then(|v| v.as_str()).unwrap_or("")
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    // 3. 只打印结构，不打印 token 本体
    let at = data
        .get("accessToken")
        .or_else(|| data.get("access_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let rt = data
        .get("refreshToken")
        .or_else(|| data.get("refresh_token"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let domain = data.get("domain").and_then(|v| v.as_str()).unwrap_or("");
    println!("   accessToken : 长度 {}，JWT={}", at.len(), at.starts_with("eyJ"));
    println!("   refreshToken: 长度 {}", rt.len());
    for key in ["expiresIn", "refreshExpiresIn", "tokenType", "domain"] {
        if let Some(v) = data.get(key) {
            println!("   {key:<12}: {v}");
        }
    }
    println!("   其余字段    : {:?}", data.as_object().map(|o| o.keys().collect::<Vec<_>>()));

    // 4. 用 token 换账号信息
    println!();
    println!("3. GET {WORKBUDDY_API_PREFIX}/login/account");
    let account_url = format!(
        "{}{WORKBUDDY_API_PREFIX}/login/account?state={state}",
        region.api_endpoint()
    );
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {at}"));
    let x_domain = if domain.is_empty() {
        "www.workbuddy.ai".to_string()
    } else {
        domain.to_string()
    };
    headers.insert("X-Domain".to_string(), x_domain);
    let acc = http_request(&account_url, "GET", None, Some(&headers)).await;
    let acc_data = acc.get("data").cloned().unwrap_or(serde_json::json!({}));
    println!(
        "   uid={:?} nickname={:?} enterpriseId={:?}",
        acc_data.get("uid").and_then(|v| v.as_str()),
        acc_data.get("nickname").and_then(|v| v.as_str()),
        acc_data.get("enterpriseId").and_then(|v| v.as_str()),
    );

    if acc_data.get("uid").and_then(|v| v.as_str()).is_some() {
        println!();
        println!("=== 结论：国际版 OAuth 链路可用，App 侧接线正确 ===");
    } else {
        println!();
        println!("=== token 已拿到，但账号信息异常，需检查 login/account 的请求头 ===");
        println!("   原始响应：{acc}");
    }
}
