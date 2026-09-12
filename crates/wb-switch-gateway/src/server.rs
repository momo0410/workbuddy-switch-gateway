//! OpenAI 兼容 HTTP 接口层。
//!
//! 对应 Go 源文件 `internal/server/handler.go`（路由 / 鉴权 / healthz / status / models）
//! 与 `internal/server/logging.go`（请求级表格日志、TTFB / token 统计）。
//!
//! # 路由表（与 Go 完全一致，含方法限定）
//!
//! | 方法   | 路径                  | 鉴权 | 说明                     |
//! |--------|-----------------------|------|--------------------------|
//! | POST   | `/v1/chat/completions`| 是   | 聊天（流式/非流式）      |
//! | GET    | `/v1/models`          | 是   | 模型列表                 |
//! | GET    | `/status`             | 是   | 账号池状态               |
//! | GET    | `/healthz`            | **否** | 探活（宿主识别用）      |
//!
//! `/healthz` 恒无鉴权：负载均衡/编排探活只需 2xx/503 语义，
//! 身份靠 `service` 字段 + `X-Service` 头双保险。

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

use crate::pool::Pool;

/// 网关身份标识。
///
/// 经 `/healthz` 响应体 `service` 字段与 `X-Service` 头同时透出：
/// 宿主探测同端口的旧服务/其他服务时，对方即使返回 2xx 也不带本标识，
/// 宿主据此可识别"假成功"。
pub const SERVICE_NAME: &str = "workbuddy2api";

/// 网关共享状态。
pub struct AppState {
    /// 账号池。
    pub pool: Mutex<Pool>,
    /// API Key；空 = 不鉴权。
    pub api_key: String,
    /// 单请求最多换号次数。
    pub max_rotate: usize,
    /// 429 软冷却时长。
    pub soft_cooldown: std::time::Duration,
    /// token 提前刷新窗口。
    pub refresh_skew: std::time::Duration,
    /// Redis 观测模式字符串（`"upstash"` / `"noop"`），供 /status 透出。
    pub redis_mode: String,
    /// 粘性会话绑定数（无粘性路由时为 0）。
    pub sticky_count: usize,
}

/// 构建路由。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat_completions))
        .route("/v1/models", get(models))
        .route("/status", get(status))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// 统一 JSON 响应。
fn json_response(status: StatusCode, v: Value) -> Response {
    let mut res = Json(v).into_response();
    *res.status_mut() = status;
    res.headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    res
}

/// OpenAI 风格错误响应体。
///
/// 与 Go 侧 `writeOpenAIError` 一致：`{"error":{"message","type","code"}}`。
fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {
    json_response(
        status,
        json!({
            "error": {
                "message": msg,
                "type": "api_error",
                "code": code,
            }
        }),
    )
}

/// Bearer 鉴权校验。
///
/// 与 Go 侧 `withAuth` 一致：key 为空则放行；否则要求
/// `Authorization: Bearer <key>` 精确匹配（区分大小写）。
fn check_auth(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    if state.api_key.is_empty() {
        return None;
    }
    let expected = format!("Bearer {}", state.api_key);
    match headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(v) if v == expected => None,
        _ => Some(openai_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid API key",
        )),
    }
}

/// `GET /healthz` —— 恒无鉴权。
///
/// 用 `servable_now` 判定：healthy>0 但全占满在途时 chat 会 503，
/// 探活必须同口径，否则负载均衡器会把流量持续打进无法受理的实例。
async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let (total, healthy, _, _, _) = {
        let p = state.pool.lock().unwrap();
        p.counts_detailed()
    };
    let servable = {
        let p = state.pool.lock().unwrap();
        p.servable_now()
    };
    let status = if servable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let mut res = json_response(
        status,
        json!({
            "healthy": healthy,
            "total": total,
            "service": SERVICE_NAME,
        }),
    );
    res.headers_mut()
        .insert("X-Service", SERVICE_NAME.parse().unwrap());
    res
}

/// `GET /status` —— 账号池状态（需鉴权）。
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    let (total, healthy, cooling, disabled, inflight_full) = {
        let p = state.pool.lock().unwrap();
        p.counts_detailed()
    };
    let accounts: Vec<Value> = {
        let p = state.pool.lock().unwrap();
        p.list().iter().map(status_to_json).collect()
    };
    let redis_mode = if state.redis_mode.is_empty() {
        "noop"
    } else {
        &state.redis_mode
    };

    json_response(
        StatusCode::OK,
        json!({
            "accounts": accounts,
            "total": total,
            "healthy": healthy,
            "cooling": cooling,
            "disabled": disabled,
            "in_flight_full": inflight_full,
            "sticky_sessions": state.sticky_count,
            "redis_mode": redis_mode,
        }),
    )
}

/// 把 pool Status 转成 /status 的 JSON 对象（字段名与 Go 侧 `Status` tag 一致）。
fn status_to_json(s: &crate::pool::Status) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("uid".into(), s.uid.clone().into());
    if !s.nickname.is_empty() {
        m.insert("nickname".into(), s.nickname.clone().into());
    }
    m.insert("credits".into(), s.credits.into());
    m.insert("cooling".into(), s.cooling.into());
    if !s.cool_kind.is_empty() {
        m.insert("cool_kind".into(), s.cool_kind.clone().into());
    }
    if s.cool_remaining_sec != 0 {
        m.insert("cool_remaining_sec".into(), s.cool_remaining_sec.into());
    }
    if let Some(u) = s.until {
        if let Ok(d) = u.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "until".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if !s.reason.is_empty() {
        m.insert("reason".into(), s.reason.clone().into());
    }
    m.insert("disabled".into(), s.disabled.into());
    if s.success_count != 0 {
        m.insert("success_count".into(), s.success_count.into());
    }
    if s.err_total != 0 {
        m.insert("err_total".into(), s.err_total.into());
    }
    if let Some(t) = s.last_success {
        if let Ok(d) = t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "last_success".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if let Some(t) = s.last_err {
        if let Ok(d) = t.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "last_err".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    // 运行态字段（Go 侧无 omitempty，恒输出）
    m.insert("in_flight".into(), s.in_flight.into());
    m.insert("breaker_fails".into(), s.breaker_fails.into());
    if let Some(b) = s.breaker_until {
        if let Ok(d) = b.duration_since(std::time::SystemTime::UNIX_EPOCH) {
            m.insert(
                "breaker_until".into(),
                chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_default()
                    .into(),
            );
        }
    }
    if s.soonest_expire_at != 0 {
        m.insert("soonest_expire_at".into(), s.soonest_expire_at.into());
    }
    if !s.expire_day.is_empty() {
        m.insert("expire_day".into(), s.expire_day.clone().into());
    }
    Value::Object(m)
}

/// 静态 CN 模型表（动态接口失败时的回退）。
///
/// 与 Go 侧 `staticModels` 完全一致（含 created / context_length 取值）。
fn static_models_cn() -> Vec<Value> {
    const IDS: &[&str] = &[
        "glm-5.2",
        "glm-5.1",
        "glm-5v-turbo",
        "kimi-k2.7",
        "minimax-m3",
        "hy3",
        "hy3-preview",
        "hy3-preview-agent",
        "deepseek-v4-pro",
        "deepseek-v4-flash",
    ];
    IDS.iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": 1753600000,
                "owned_by": "workbuddy",
                "context_length": 131072,
            })
        })
        .collect()
}

/// 国际版静态模型表。
///
/// 国际版的 `/console/enterprises/personal/models` 在当前版本返回 500，
/// 无法动态拉取，因此内置一份。
fn static_models_intl() -> Vec<Value> {
    const SHORT: &[(&str, i64)] = &[
        ("default-model", 200000),
        ("fast-model", 176000),
        ("balanced-model", 176000),
        ("primary-model", 176000),
        ("deep-model", 176000),
    ];
    const STD: &[&str] = &[
        "hy4-preview",
        "hy3",
        "deepseek-v4.1-flash",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
        "gemini-3.5-flash",
        "glm-5.3",
        "glm-5.2",
        "kimi-k3",
        "kimi-k2.6",
    ];
    let mut out = Vec::new();
    for (id, ctx) in SHORT {
        out.push(json!({
            "id": id, "object": "model", "created": 1753600000,
            "owned_by": "workbuddy-intl", "context_length": ctx,
        }));
    }
    for id in STD {
        out.push(json!({
            "id": id, "object": "model", "created": 1753600000,
            "owned_by": "workbuddy-intl", "context_length": 131072,
        }));
    }
    out
}

/// 合并两个区域的模型（按 id 去重，国服优先）。
///
/// `/v1/models` 没有账号上下文，因此返回并集：客户端据此得知全部可用名称。
fn static_models_all() -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in static_models_cn().into_iter().chain(static_models_intl()) {
        if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
            if id.is_empty() || !seen.insert(id.to_string()) {
                continue;
            }
            out.push(m);
        }
    }
    out
}

/// `GET /v1/models` —— 模型列表（需鉴权）。
async fn models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    json_response(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": static_models_all(),
        }),
    )
}

/// `POST /v1/chat/completions` —— 占位实现（流式/非流式在后续步骤接入）。
///
/// 当前返回 503 `no_healthy_account`：先将路由/鉴权/响应骨架打通并对齐，
/// 避免在 upstream 客户端就绪前引入不可验证的转发逻辑。
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = check_auth(&state, &headers) {
        return r;
    }
    openai_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_healthy_account",
        "all accounts unavailable (cooling/disabled)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state(api_key: &str) -> Arc<AppState> {
        Arc::new(AppState {
            pool: Mutex::new(Pool::new(String::new())),
            api_key: api_key.into(),
            max_rotate: 3,
            soft_cooldown: std::time::Duration::from_secs(60),
            refresh_skew: std::time::Duration::from_secs(600),
            redis_mode: String::new(),
            sticky_count: 0,
        })
    }

    async fn body_json(res: Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn healthz_is_unauthenticated_and_reports_service() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // 无账号 → 503（ServableNow=false），但必须带身份标识
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get("X-Service").unwrap(), SERVICE_NAME);
        let v = body_json(res).await;
        assert_eq!(v["service"], SERVICE_NAME);
        assert_eq!(v["total"], 0);
        assert_eq!(v["healthy"], 0);
    }

    #[tokio::test]
    async fn healthz_200_when_account_available() {
        let state = test_state("");
        {
            let mut p = state.pool.lock().unwrap();
            p.add(crate::auth::Auth {
                uid: "u1".into(),
                access_token: "at".into(),
                ..Default::default()
            });
        }
        let app = router(state);
        let res = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["healthy"], 1);
        assert_eq!(v["total"], 1);
    }

    #[tokio::test]
    async fn status_requires_auth_when_key_set() {
        // 无 Authorization 头 → 401
        let app = router(test_state("secret"));
        let res = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let v = body_json(res).await;
        assert_eq!(v["error"]["code"], "invalid_api_key");
        assert_eq!(v["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn status_accepts_valid_bearer() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("Authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert!(v.get("accounts").is_some());
        assert_eq!(v["redis_mode"], "noop"); // 空 → 回落 noop
        assert_eq!(v["sticky_sessions"], 0);
    }

    #[tokio::test]
    async fn status_rejects_wrong_key() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("Authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_requires_auth() {
        let app = router(test_state("secret"));
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn models_returns_union_with_cn_first() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = body_json(res).await;
        assert_eq!(v["object"], "list");
        let data = v["data"].as_array().unwrap();
        // 国服优先去重：glm-5.2 应出现且 owned_by 为 workbuddy
        let glm = data.iter().find(|m| m["id"] == "glm-5.2").unwrap();
        assert_eq!(glm["owned_by"], "workbuddy");
        assert_eq!(glm["context_length"], 131072);
        assert_eq!(glm["created"], 1753600000);
        // id 唯一
        let mut ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "模型 id 应去重");
    }

    #[tokio::test]
    async fn no_auth_when_key_empty() {
        let app = router(test_state(""));
        let res = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
