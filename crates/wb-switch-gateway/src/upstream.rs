//! 上游错误分类，驱动 pool 冷却状态机。
//!
//! 对应 Go 源文件 `internal/upstream/client.go` 的 `ErrKind` / `Classify`。
//!
//! # 分类优先级（必须与 Go 逐条一致）
//!
//! 1. `402` → 硬冷却（余额）
//! 2. body 命中 hardMarkers（小写 + 中文原文双通道）→ 硬冷却
//! 3. body 命中 sessionDeadMarkers → 禁用
//! 4. `429` → 软冷却
//! 5. `404` → 短冷却
//! 6. `>=500` → 服务端错误（喂熔断）
//! 7. `>=400` → 客户端错误（只换号不罚）
//! 8. 其余 → 无错误
//!
//! 注意第 2 步在状态码判断**之前**：HTTP 200 但业务 code 非 0 且含余额关键词
//! 的情况也要判成硬冷却。

/// 错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrKind {
    /// 成功。
    #[default]
    None,
    /// 余额不足（402 或 body 关键词）→ 长冷却。
    HardCredit,
    /// 429 软限流 → 短冷却。
    SoftRate,
    /// session 失效（401 + 12153）→ 禁用。
    SessionDead,
    /// 404 上游偶发 → 短冷却，防雪崩。
    NotFound,
    /// 5xx 上游故障。
    Server,
    /// 其他 4xx / 业务错误。
    Client,
}

impl ErrKind {
    /// 序列化名，与 Go 侧 `String()` 一致。
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::HardCredit => "hard_credit",
            ErrKind::SoftRate => "soft_rate",
            ErrKind::SessionDead => "session_dead",
            ErrKind::NotFound => "not_found",
            ErrKind::Server => "server",
            ErrKind::Client => "client",
        }
    }
}

impl std::fmt::Display for ErrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 带分类的上游错误。
///
/// `Display` 与 Go 侧 `(*Error).Error()` 同格式：
/// `upstream {kind} (http {status}): {msg}`
#[derive(Debug, Clone)]
pub struct UpstreamError {
    /// 错误分类。
    pub kind: ErrKind,
    /// HTTP 状态码。
    pub status: u16,
    /// 原始响应体。
    pub msg: String,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "upstream {} (http {}): {}", self.kind, self.status, self.msg)
    }
}

impl std::error::Error for UpstreamError {}

/// 余额不足关键词（小写比较 + 中文原文比较双通道）。
const HARD_MARKERS: &[&str] = &[
    "insufficient credit",
    "no credit",
    "credit exhausted",
    "out of credit",
    "quota exceeded",
    "quota exhaust",
    "payment required",
    "credit not enough",
    "not enough credit",
    "积分不足",
    "额度不足",
    "余额不足",
    "积分用完",
    "额度用尽",
    "没有积分",
];

/// session 失效标记。
const SESSION_DEAD_MARKERS: &[&str] = &["Offline user session not found", "12153"];

/// 按 HTTP 状态码 + body 判定错误类别。
///
/// 与 Go 侧 `Classify` 逐条对齐，包括「body 关键词判定先于状态码」的顺序。
pub fn classify(status: u16, body: &str) -> ErrKind {
    if status == 402 {
        return ErrKind::HardCredit;
    }
    let lower = body.to_lowercase();
    for m in HARD_MARKERS {
        // 双通道：小写比较（英文）或原文比较（中文，大小写不敏感但中文无大小写）
        if lower.contains(&m.to_lowercase()) || body.contains(m) {
            return ErrKind::HardCredit;
        }
    }
    for m in SESSION_DEAD_MARKERS {
        if body.contains(m) {
            return ErrKind::SessionDead;
        }
    }
    if status == 429 {
        return ErrKind::SoftRate;
    }
    if status == 404 {
        return ErrKind::NotFound;
    }
    if status >= 500 {
        return ErrKind::Server;
    }
    if status >= 400 {
        return ErrKind::Client;
    }
    // HTTP 200 但业务 code 非 0 且含余额关键词的情况已被上面 HARD_MARKERS 捕获。
    ErrKind::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_based_classification() {
        assert_eq!(classify(402, ""), ErrKind::HardCredit);
        assert_eq!(classify(429, ""), ErrKind::SoftRate);
        assert_eq!(classify(404, ""), ErrKind::NotFound);
        assert_eq!(classify(500, ""), ErrKind::Server);
        assert_eq!(classify(503, ""), ErrKind::Server);
        assert_eq!(classify(400, ""), ErrKind::Client);
        assert_eq!(classify(200, ""), ErrKind::None);
    }

    #[test]
    fn hard_markers_override_status() {
        // 即使是 200，只要 body 含余额关键词也判硬冷却
        assert_eq!(classify(200, "insufficient credit"), ErrKind::HardCredit);
        assert_eq!(classify(200, "Insufficient Credit"), ErrKind::HardCredit); // 大小写不敏感
        assert_eq!(classify(500, "quota exceeded"), ErrKind::HardCredit);
        // 中文原文通道
        assert_eq!(classify(200, "余额不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "您的积分不足"), ErrKind::HardCredit);
        assert_eq!(classify(200, "额度用尽"), ErrKind::HardCredit);
    }

    #[test]
    fn session_dead_markers() {
        assert_eq!(
            classify(401, r#"{"code":12153,"msg":"Offline user session not found"}"#),
            ErrKind::SessionDead
        );
        assert_eq!(classify(200, "12153"), ErrKind::SessionDead);
    }

    #[test]
    fn hard_markers_take_priority_over_session_dead() {
        // hardMarkers 判定在 sessionDeadMarkers 之前
        assert_eq!(
            classify(401, "12153 余额不足"),
            ErrKind::HardCredit
        );
    }

    #[test]
    fn err_kind_display_matches_go() {
        assert_eq!(ErrKind::HardCredit.as_str(), "hard_credit");
        assert_eq!(ErrKind::SoftRate.as_str(), "soft_rate");
        assert_eq!(ErrKind::SessionDead.as_str(), "session_dead");
        assert_eq!(ErrKind::NotFound.as_str(), "not_found");
        assert_eq!(ErrKind::Server.as_str(), "server");
        assert_eq!(ErrKind::Client.as_str(), "client");
        assert_eq!(ErrKind::None.as_str(), "none");
    }

    #[test]
    fn upstream_error_display_format() {
        let e = UpstreamError {
            kind: ErrKind::SoftRate,
            status: 429,
            msg: "rate limited".into(),
        };
        assert_eq!(e.to_string(), "upstream soft_rate (http 429): rate limited");
    }
}
