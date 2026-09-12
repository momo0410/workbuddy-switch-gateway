//! OAuth 登录采集（复刻 cockpit 流程）。
//!
//! 对照 server.py `oauth_start` / `oauth_poll`。
//!
//! 国服与国际版共用同一套端点，但**域名与平台标识都不同**：
//!   - 域名：`www.codebuddy.cn` / `www.workbuddy.ai`
//!   - 平台：`workbuddy` / `workbuddy-ai`
//! 两者都由 [`Region`] 决定：发起登录时选定，轮询时从会话里回读。
//!
//! **登录方式两区域不同**：国服是微信 / 企业微信扫码；国际版没有扫码，走
//! Keycloak realm 的 Google / GitHub / X 联合登录（另有账号密码、企业 SSO、
//! Tencent OneID）。本模块只负责「申请 state → 轮询 token」，具体怎么授权由
//! 浏览器里的官方登录页决定，因此两区域共用同一段代码。
//!
//! 实测（2026-09）：国际版 `platform=workbuddy-ai` 走完联合登录后，
//! `/v2/plugin/auth/token` 正常返回 accessToken / refreshToken
//! （`domain=www.workbuddy.ai`），`login/account` 取到 uid。
//! 注意该 token 端点**一次性消费**：取到一次后再次轮询会退回 `11217 login ing`，
//! 因此 `oauth_poll` 必须拿到结果后立即入库，不能重复拉取。
//!
//! 手动验证脚本见 `examples/oauth_intl_probe.rs`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::modules::account;
use crate::modules::config::{
    http_request, norm_ts, now_ms, now_secs, Region, OAUTH_TIMEOUT_SECONDS, WORKBUDDY_API_PREFIX,
};

/// 取到 token 后拉账号信息的尝试次数。
///
/// token 端点一次性消费，重试是唯一能补救一次网络抖动的手段。
const ACCOUNT_FETCH_ATTEMPTS: usize = 3;
const ACCOUNT_FETCH_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(800);

#[derive(Default)]
struct OAuthInfo {
    state: String,
    /// 发起登录时选定的区域；轮询取 token 与账号信息都按它选域名。
    region: Option<Region>,
    expires_at: i64,
    done: bool,
    result: Option<Value>,
    error: Option<String>,
}

static OAUTH_STATES: OnceLock<Mutex<HashMap<String, OAuthInfo>>> = OnceLock::new();

fn oauth_states() -> &'static Mutex<HashMap<String, OAuthInfo>> {
    OAUTH_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 申请 state 的端点（区域决定域名与平台标识）。
fn oauth_state_url(region: Region) -> String {
    format!(
        "{}{WORKBUDDY_API_PREFIX}/auth/state?platform={}",
        region.api_endpoint(),
        region.oauth_platform()
    )
}

/// 轮询 token 的端点。
fn oauth_token_url(region: Region, state: &str) -> String {
    format!(
        "{}{WORKBUDDY_API_PREFIX}/auth/token?state={state}",
        region.api_endpoint()
    )
}

/// 拉取账号信息的端点。
fn oauth_account_url(region: Region, state: &str) -> String {
    format!(
        "{}{WORKBUDDY_API_PREFIX}/login/account?state={state}",
        region.api_endpoint()
    )
}

/// 区域对应的凭据域（`https://www.workbuddy.ai` → `www.workbuddy.ai`）。
fn region_domain(region: Region) -> String {
    region
        .api_endpoint()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// 发起登录：向官方申请 state，返回 loginId / verificationUri / expiresIn。
pub async fn oauth_start(region: Region) -> Result<Value, String> {
    let login_id = format!("wb_{}", uuid::Uuid::new_v4().simple());
    let url = oauth_state_url(region);
    let resp = http_request(&url, "POST", Some(json!({})), None).await;
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let state = data
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if state.is_empty() {
        let snippet = serde_json::to_string(&resp)
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect::<String>();
        return Err(format!("auth/state 响应缺少 state: {snippet}"));
    }
    let auth_url = data
        .get("authUrl")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("auth_url").and_then(|v| v.as_str()))
        .or_else(|| data.get("url").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{}/login?platform={}&state={state}",
                region.api_endpoint(),
                region.oauth_platform()
            )
        });

    let mut map = oauth_states().lock().unwrap();
    map.insert(
        login_id.clone(),
        OAuthInfo {
            state,
            region: Some(region),
            expires_at: now_secs() + OAUTH_TIMEOUT_SECONDS,
            ..Default::default()
        },
    );
    drop(map);

    Ok(json!({
        "loginId": login_id,
        "verificationUri": auth_url,
        "expiresIn": OAUTH_TIMEOUT_SECONDS,
        "region": region.key(),
    }))
}

/// 轮询一次官方 token 接口。成功则拉取账号信息并入库。
pub async fn oauth_poll(login_id: &str) -> Value {
    let (state, region) = {
        let mut map = oauth_states().lock().unwrap();
        let Some(info) = map.get_mut(login_id) else {
            return json!({"done": true, "error": "登录请求不存在"});
        };
        if info.done {
            return json!({"done": true, "result": info.result.clone(), "error": info.error.clone()});
        }
        if now_secs() > info.expires_at {
            info.done = true;
            info.error = Some("登录超时".to_string());
            return json!({"done": true, "error": "登录超时"});
        }
        // 会话缺失区域时按国服继续，保持历史行为
        (info.state.clone(), info.region.unwrap_or(Region::Cn))
    };

    let url = oauth_token_url(region, &state);
    let resp = http_request(&url, "GET", None, None).await;
    let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 && code != 200 {
        return json!({"done": false});
    }
    let data = resp.get("data").cloned().unwrap_or_else(|| json!({}));
    let access_token = data
        .get("accessToken")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("access_token").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return json!({"done": false});
    }

    // 拉取账号信息。
    //
    // token 端点**一次性消费**（取到一次后再轮询会退回 11217），所以这里必须
    // 尽量把账号信息取到手：失败就重试几次，实在拿不到也要带着 token 入库，
    // 不能让已经到手的凭证因为一次网络抖动而丢掉。
    let account_url = oauth_account_url(region, &state);
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!("Bearer {access_token}"),
    );
    // 优先用官方回传的 domain；缺失时按本次会话的区域补一个 —— 国际版少了
    // X-Domain 会被上游直接拒绝。
    let domain = data
        .get("domain")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| region_domain(region));
    headers.insert("X-Domain".to_string(), domain.clone());
    let mut acc_data = json!({});
    for attempt in 0..ACCOUNT_FETCH_ATTEMPTS {
        let acc_resp = http_request(&account_url, "GET", None, Some(&headers)).await;
        let code = acc_resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if (code == 0 || code == 200) && acc_resp.get("data").is_some_and(|d| d.is_object()) {
            acc_data = acc_resp.get("data").cloned().unwrap_or_else(|| json!({}));
            break;
        }
        if attempt + 1 < ACCOUNT_FETCH_ATTEMPTS {
            tokio::time::sleep(ACCOUNT_FETCH_RETRY_DELAY).await;
        }
    }

    let uid = acc_data
        .get("uid")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let nickname = acc_data
        .get("nickname")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let email = oauth_profile_email(&acc_data);

    let expires_at = norm_ts(data.get("expiresAt").or_else(|| data.get("expires_at")));
    let expires_at = match expires_at {
        Some(v) => Some(v),
        None => data
            .get("expiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };
    let refresh_expires_at = norm_ts(
        data.get("refreshExpiresAt")
            .or_else(|| data.get("refresh_expires_at")),
    );
    let refresh_expires_at = match refresh_expires_at {
        Some(v) => Some(v),
        None => data
            .get("refreshExpiresIn")
            .and_then(|v| v.as_i64())
            .map(|e| now_ms() + e * 1000),
    };

    let account = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "uid": uid,
        "nickname": nickname,
        "email": email,
        "enterpriseName": acc_data.get("enterpriseName"),
        "enterpriseId": acc_data.get("enterpriseId"),
        "access_token": access_token,
        "refresh_token": data.get("refreshToken").and_then(|v| v.as_str())
            .or_else(|| data.get("refresh_token").and_then(|v| v.as_str()))
            .map(|s| s.to_string()),
        "token_type": data.get("tokenType").and_then(|v| v.as_str())
            .or_else(|| data.get("token_type").and_then(|v| v.as_str()))
            .unwrap_or("Bearer")
            .to_string(),
        "domain": domain.to_string(),
        "expiresAt": expires_at,
        "refreshExpiresAt": refresh_expires_at,
        "auth_raw": data,
        "profile_raw": acc_data,
        "createdAt": now_ms(),
    });

    let account = match account::save_collected_account(account) {
        Ok(saved) => saved,
        Err(error) => {
            let error = format!("保存账号失败: {error}");
            let mut map = oauth_states().lock().unwrap();
            if let Some(info) = map.get_mut(login_id) {
                info.done = true;
                info.error = Some(error.clone());
            }
            return json!({"done": true, "error": error});
        }
    };

    let result = account::account_meta(&account);
    let mut map = oauth_states().lock().unwrap();
    if let Some(info) = map.get_mut(login_id) {
        info.done = true;
        info.result = Some(result.clone());
    }
    drop(map);

    json!({"done": true, "result": result})
}

fn oauth_profile_email(profile: &Value) -> Option<String> {
    profile
        .get("email")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_profile_without_email_does_not_use_nickname_or_uid() {
        let profile = json!({"uid": "u-1", "nickname": "同名用户"});
        assert_eq!(oauth_profile_email(&profile), None);
    }

    #[test]
    fn oauth_profile_keeps_factual_email() {
        let profile = json!({"email": " user@example.com "});
        assert_eq!(
            oauth_profile_email(&profile).as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn oauth_endpoints_follow_region() {
        assert_eq!(
            oauth_state_url(Region::Cn),
            "https://www.codebuddy.cn/v2/plugin/auth/state?platform=workbuddy"
        );
        assert_eq!(
            oauth_state_url(Region::Intl),
            "https://www.workbuddy.ai/v2/plugin/auth/state?platform=workbuddy-ai"
        );
        assert_eq!(
            oauth_token_url(Region::Intl, "s-1"),
            "https://www.workbuddy.ai/v2/plugin/auth/token?state=s-1"
        );
        assert_eq!(
            oauth_account_url(Region::Intl, "s-1"),
            "https://www.workbuddy.ai/v2/plugin/login/account?state=s-1"
        );
    }

    #[test]
    fn oauth_state_url_never_sends_cn_platform_to_intl() {
        // 国际版登录页按 platform 分派登录服务：`workbuddy` 会落到 Web 分支，
        // 拿不到插件 token 缓存，轮询会一直停在 login ing。
        assert!(!oauth_state_url(Region::Intl).contains("platform=workbuddy&"));
        assert!(oauth_state_url(Region::Intl).contains("platform=workbuddy-ai"));
    }

    #[test]
    fn region_domain_strips_scheme_for_x_domain_header() {
        assert_eq!(region_domain(Region::Cn), "www.codebuddy.cn");
        assert_eq!(region_domain(Region::Intl), "www.workbuddy.ai");
    }

    /// 端到端：发起一次国际版登录，验证官方确实按 `platform=workbuddy-ai`
    /// 签发 state，且授权页指向 workbuddy.ai。
    ///
    /// 只读操作（不改账号库），但会真实打官方接口，故默认忽略：
    ///   cargo test -p wb-switch-core --lib oauth_start_intl -- --ignored --nocapture
    #[tokio::test(flavor = "current_thread")]
    #[ignore = "需要联网访问官方接口"]
    async fn oauth_start_intl_hits_workbuddy_ai_with_plugin_platform() {
        let started = oauth_start(Region::Intl)
            .await
            .expect("国际版 auth/state 应当成功");
        let uri = started["verificationUri"].as_str().unwrap_or_default();
        println!("intl verificationUri = {uri}");
        assert!(
            uri.starts_with("https://www.workbuddy.ai/login"),
            "国际版授权页应指向 workbuddy.ai: {uri}"
        );
        assert!(
            uri.contains("platform=workbuddy-ai"),
            "国际版授权页应带插件平台标识: {uri}"
        );
        assert_eq!(started["region"], "intl");

        // 尚未授权时应停在 pending，且不报错
        let login_id = started["loginId"].as_str().expect("loginId");
        let polled = oauth_poll(login_id).await;
        println!("intl first poll = {polled}");
        assert_eq!(polled["done"], false, "未授权前不应判定完成: {polled}");
    }
}
