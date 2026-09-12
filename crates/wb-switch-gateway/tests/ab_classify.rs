//! A/B 对照：上游错误分类必须与 Go 侧 `upstream.Classify` 完全一致。
//!
//! 用例矩阵与 Go 侧 `internal/upstream/zz_classify_probe` 保持一致，
//! 由 `tests/ab_classify.rs` 逐行比对输出。

/// 用例矩阵：(status, body, 期望 kind 名)。
pub const CASES: &[(u16, &str, &str)] = &[
    (200, "", "none"),
    (400, "", "client"),
    (401, "", "client"),
    (402, "", "hard_credit"),
    (404, "", "not_found"),
    (429, "", "soft_rate"),
    (500, "", "server"),
    (503, "", "server"),
    (200, "insufficient credit", "hard_credit"),
    (200, "Insufficient Credit", "hard_credit"),
    (500, "quota exceeded", "hard_credit"),
    (200, "余额不足", "hard_credit"),
    (200, "您的积分不足", "hard_credit"),
    (200, "额度用尽", "hard_credit"),
    (200, "没有积分", "hard_credit"),
    (200, "no credit", "hard_credit"),
    (200, "credit exhausted", "hard_credit"),
    (200, "out of credit", "hard_credit"),
    (200, "quota exhaust", "hard_credit"),
    (200, "payment required", "hard_credit"),
    (200, "credit not enough", "hard_credit"),
    (200, "not enough credit", "hard_credit"),
    (200, "积分用完", "hard_credit"),
    (200, "额度不足", "hard_credit"),
    (401, r#"{"code":12153,"msg":"Offline user session not found"}"#, "session_dead"),
    (200, "12153", "session_dead"),
    (401, "12153 余额不足", "hard_credit"),
    (403, "", "client"),
    (418, "", "client"),
    (502, "", "server"),
    (504, "", "server"),
    (200, r#"{"choices":[]}"#, "none"),
];

#[test]
fn classify_matches_go_reference_matrix() {
    for (status, body, want) in CASES {
        let got = wb_switch_gateway::upstream::classify(*status, body).as_str();
        assert_eq!(
            got, *want,
            "classify({status}, {body:?}) = {got}, Go 侧为 {want}"
        );
    }
}
