//! 账号凭证解析与原子写回。
//!
//! 对应 Go 源文件 `internal/auth/auth.go`。
//!
//! # 磁盘双形态
//!
//! | 形态   | 结构                                 | 来源               |
//! |--------|--------------------------------------|--------------------|
//! | 嵌套形 | `{"auth":{...},"account":{...}}`     | 插件 OAuth 输出    |
//! | 扁平形 | `{"accessToken":...,"uid":...}`      | 手写 / 旧版        |
//!
//! 判定依据是**顶层是否存在 `auth` 键**（与 Go 侧 `probe["auth"]` 一致），
//! 而不是逐个字段嗅探——避免半嵌套文件被误判成扁平形而丢字段。
//!
//! # 时间戳量级
//!
//! 上游混用秒/毫秒两种精度，[`normalize_epoch`] 按量级统一成秒。
//! 注意这是 Go 侧已有的启发式（阈值 `1e12`），不是精确判定。

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::error::{GatewayError, Result};

/// 归一化后的账号凭证。
#[derive(Debug, Clone, Default)]
pub struct Auth {
    /// 访问令牌。
    pub access_token: String,
    /// 刷新令牌。
    pub refresh_token: String,
    /// 过期时刻（Unix 秒）。
    pub expires_at: i64,
    /// 区域域名（国服 / 国际版）。
    pub domain: String,
    /// 账号 UID。
    pub uid: String,
    /// 企业 ID。
    pub enterprise_id: String,
    /// 昵称。
    pub nickname: String,
    /// 来源文件路径；refresh 后原子写回此处。
    pub file_path: String,
    /// 仍有剩余积分的套餐中最早到期时刻（Unix 秒）；0 = 未知。
    ///
    /// 由凭证文件的 `credit` 块带入（宿主查询积分后写入）或由签到任务刷新。
    /// 账号池据此做「先烧快过期额度」的分层选号。只读元数据，不参与 token 刷新写回判定。
    pub soonest_expire_at: i64,
}

impl Auth {
    /// 报告 token 是否将在 `within` 内过期（或已过期/无 expiry）。
    ///
    /// 与 Go 侧 `NeedsRefresh` 一致：`expires_at <= 0` 视为需要刷新
    ///（无过期信息时保守刷新，避免带着可能已失效的 token 发请求）。
    pub fn needs_refresh(&self, within: std::time::Duration) -> bool {
        if self.expires_at <= 0 {
            return true;
        }
        let now = chrono::Utc::now().timestamp();
        now + within.as_secs() as i64 >= self.expires_at
    }

    /// 是否国际版（workbuddy.ai）。
    ///
    /// 严格对齐 Go 侧 `upstream.IsIntl`：
    /// `strings.HasSuffix(strings.ToLower(strings.TrimSpace(domain)), ".ai")`。
    ///
    /// 注意是**域名后缀**判定而非包含匹配：`copilot.tencent.com` 不含 `.ai`，
    /// 而 `xxx.workbuddy.ai` 与 `workbuddy.ai` 都命中。大小写与首尾空白均不敏感。
    pub fn is_intl(&self) -> bool {
        self.domain.trim().to_ascii_lowercase().ends_with(".ai")
    }
}

/// 嵌套形凭证文件（插件 OAuth 输出）。
#[derive(Debug, Deserialize)]
struct NestedAuth {
    auth: Option<NestedAuthBlock>,
    account: Option<NestedAccountBlock>,
    credit: Option<CreditBlock>,
}

#[derive(Debug, Deserialize)]
struct NestedAuthBlock {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
    #[serde(default)]
    domain: String,
}

#[derive(Debug, Deserialize)]
struct NestedAccountBlock {
    #[serde(default)]
    uid: String,
    #[serde(rename = "enterpriseId", default)]
    enterprise_id: String,
    #[serde(default)]
    nickname: String,
}

/// 扁平形凭证文件（手写 / 旧版）。
#[derive(Debug, Deserialize)]
struct FlatAuth {
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
    #[serde(rename = "expiresAt", default)]
    expires_at: i64,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    uid: String,
    #[serde(rename = "enterpriseId", default)]
    enterprise_id: String,
    #[serde(default)]
    nickname: String,
    /// 扁平形把 credit 字段平铺在顶层（兼容旧版手写凭证）。
    #[serde(rename = "soonestExpireAt", default)]
    soonest_expire_at: i64,
}

/// 凭证文件里的积分到期元数据（宿主写入，网关只读）。
#[derive(Debug, Deserialize)]
struct CreditBlock {
    /// 最近到期时刻。上游混用秒/毫秒，按量级归一。
    #[serde(rename = "soonestExpireAt", default)]
    soonest_expire_at: i64,
}

/// 解析凭证字节，兼容嵌套形与扁平形。
///
/// 返回 [`GatewayError::AuthParse`] 变体对应 Go 侧的
/// `storage_parse_error` / `parse_error: missing accessToken`。
pub fn parse(raw: &[u8]) -> Result<Auth> {
    if raw.is_empty() {
        return Err(GatewayError::AuthParse {
            path: String::new(),
            msg: "empty auth storage".into(),
        });
    }

    // 先用宽松结构探查顶层是否存在 "auth" 键 —— 与 Go 侧 probe 判定一致。
    let probe: serde_json::Value = serde_json::from_slice(raw).map_err(|e| {
        GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        }
    })?;

    let a = if probe.get("auth").is_some() {
        let n: NestedAuth = serde_json::from_value(probe).map_err(|e| GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        })?;
        let (at, rt, ea, dom) = match n.auth {
            Some(b) => (b.access_token, b.refresh_token, b.expires_at, b.domain),
            None => (String::new(), String::new(), 0, String::new()),
        };
        let (uid, eid, nick) = match n.account {
            Some(b) => (b.uid, b.enterprise_id, b.nickname),
            None => (String::new(), String::new(), String::new()),
        };
        Auth {
            access_token: at,
            refresh_token: rt,
            expires_at: ea,
            domain: dom,
            uid,
            enterprise_id: eid,
            nickname: nick,
            soonest_expire_at: normalize_epoch(n.credit.map(|c| c.soonest_expire_at).unwrap_or(0)),
            ..Default::default()
        }
    } else {
        let f: FlatAuth = serde_json::from_value(probe).map_err(|e| GatewayError::AuthParse {
            path: String::new(),
            msg: format!("storage_parse_error: {e}"),
        })?;
        Auth {
            access_token: f.access_token,
            refresh_token: f.refresh_token,
            expires_at: f.expires_at,
            domain: f.domain,
            uid: f.uid,
            enterprise_id: f.enterprise_id,
            nickname: f.nickname,
            soonest_expire_at: normalize_epoch(f.soonest_expire_at),
            ..Default::default()
        }
    };

    if a.access_token.trim().is_empty() {
        return Err(GatewayError::AuthParse {
            path: String::new(),
            msg: "parse_error: missing accessToken".into(),
        });
    }
    Ok(a)
}

/// 把秒/毫秒 epoch 统一成秒（上游混用两种精度）。
///
/// 阈值 1e12：小于它视为秒（2001 年后、2286 年前的秒级时间戳都在此区间），
/// 大于等于它视为毫秒。与 Go 侧 `normalizeEpoch` 同阈值、同语义。
pub fn normalize_epoch(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    if n > 1_000_000_000_000 {
        return n / 1000;
    }
    n
}

impl Auth {
    /// 以嵌套形原子写回 [`Auth::file_path`]（tmp + rename），保持插件可读格式。
    ///
    /// 防御：access_token 为空时拒绝写回，避免误用空凭证覆盖有效文件。
    /// 与 Go 侧 `SaveAtomic` 一致（Go 版全程持 mutex；Rust 版由调用方
    /// 通过 `&mut self` / 上层锁保证独占，见 `pool` 模块的写回路径）。
    pub fn save_atomic(&self) -> Result<()> {
        if self.access_token.trim().is_empty() {
            return Err(GatewayError::AuthParse {
                path: self.file_path.clone(),
                msg: format!("save refused: empty accessToken (uid={})", self.uid),
            });
        }
        let path = self.file_path.clone();
        if path.is_empty() {
            return Err(GatewayError::AuthSave {
                path,
                msg: "no FilePath set".into(),
            });
        }

        let mut doc = serde_json::Map::new();
        let mut auth = serde_json::Map::new();
        auth.insert("accessToken".into(), self.access_token.clone().into());
        auth.insert("refreshToken".into(), self.refresh_token.clone().into());
        auth.insert("expiresAt".into(), self.expires_at.into());
        auth.insert("domain".into(), self.domain.clone().into());
        let mut account = serde_json::Map::new();
        account.insert("uid".into(), self.uid.clone().into());
        account.insert("enterpriseId".into(), self.enterprise_id.clone().into());
        account.insert("nickname".into(), self.nickname.clone().into());
        doc.insert("auth".into(), auth.into());
        doc.insert("account".into(), account.into());

        // 积分到期元数据必须原样保留：它由宿主或签到任务写入，而 token 刷新
        // 会重写整个文件。若这里丢掉，一次保活就会抹掉选号依据
        //（表现为分层均衡静默退化成原来的三因子随机）。
        if self.soonest_expire_at > 0 {
            let mut credit = serde_json::Map::new();
            credit.insert("soonestExpireAt".into(), self.soonest_expire_at.into());
            doc.insert("credit".into(), credit.into());
        }

        let value = serde_json::Value::Object(doc);
        let raw = serde_json::to_string_pretty(&value)
            .map_err(|e| GatewayError::AuthSave { path: path.clone(), msg: e.to_string() })?;

        atomic_write(Path::new(&path), raw.as_bytes())
            .map_err(|e| GatewayError::AuthSave { path: path.clone(), msg: e })?;
        Ok(())
    }
}

/// 原子写文件：同目录写 `.tmp` 再 rename。
///
/// 与 Go 侧 `os.WriteFile(tmp) + os.Rename` 同策略：同目录保证 rename 不跨设备。
pub fn atomic_write(path: &Path, data: &[u8]) -> std::result::Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("write tmp {}: {e}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 清理残留 tmp，避免堆积
            let _ = std::fs::remove_file(&tmp);
            Err(format!("rename {} -> {}: {e}", tmp.display(), path.display()))
        }
    }
}

/// 扫描并解析 `dir` 下 `workbuddy*.json`；解析失败的文件静默跳过。
///
/// 返回顺序与 Go 侧 `filepath.Glob` 一致（字典序，由 `read_dir` + sort 保证，
/// 避免依赖平台返回顺序导致启动选号不确定）。
pub fn load_dir(dir: &str) -> Result<Vec<Auth>> {
    let d = Path::new(dir);
    if !d.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(d)
        .map_err(|e| GatewayError::Other(format!("read_dir {dir}: {e}")))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("workbuddy") && n.ends_with(".json"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let raw = match std::fs::read(&f) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let mut a = match parse(&raw) {
            Ok(a) => a,
            Err(_) => continue,
        };
        a.file_path = f.to_string_lossy().to_string();
        out.push(a);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NESTED: &str = r#"{
      "auth": {
        "accessToken": "at-123",
        "refreshToken": "rt-456",
        "expiresAt": 1700000000,
        "domain": "https://copilot.tencent.com"
      },
      "account": {
        "uid": "u1",
        "enterpriseId": "e1",
        "nickname": "猫猫"
      },
      "credit": { "soonestExpireAt": 1735689600000 }
    }"#;

    const FLAT: &str = r#"{
      "accessToken": "at-flat",
      "refreshToken": "rt-flat",
      "expiresAt": 1700000000,
      "domain": "https://copilot.tencent.com",
      "uid": "u2",
      "enterpriseId": "e2",
      "nickname": "flat",
      "soonestExpireAt": 1735689600
    }"#;

    #[test]
    fn parse_nested_shape() {
        let a = parse(NESTED.as_bytes()).unwrap();
        assert_eq!(a.access_token, "at-123");
        assert_eq!(a.refresh_token, "rt-456");
        assert_eq!(a.expires_at, 1700000000);
        assert_eq!(a.uid, "u1");
        assert_eq!(a.enterprise_id, "e1");
        assert_eq!(a.nickname, "猫猫");
        // 毫秒 → 秒
        assert_eq!(a.soonest_expire_at, 1735689600);
    }

    #[test]
    fn parse_flat_shape() {
        let a = parse(FLAT.as_bytes()).unwrap();
        assert_eq!(a.access_token, "at-flat");
        assert_eq!(a.uid, "u2");
        assert_eq!(a.nickname, "flat");
        assert_eq!(a.soonest_expire_at, 1735689600);
    }

    #[test]
    fn shape_detection_uses_top_level_auth_key() {
        // 没有 "auth" 键 → 扁平形，即使内部字段长得很像嵌套形
        let weird = r#"{"accessToken":"x","uid":"u9"}"#;
        let a = parse(weird.as_bytes()).unwrap();
        assert_eq!(a.uid, "u9");
        assert_eq!(a.access_token, "x");
    }

    #[test]
    fn missing_access_token_rejected() {
        let bad = r#"{"uid":"u1"}"#;
        let err = parse(bad.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("missing accessToken"), "{err}");

        // 嵌套形缺 accessToken 同样拒绝
        let bad2 = r#"{"auth":{"refreshToken":"r"},"account":{"uid":"u1"}}"#;
        let err2 = parse(bad2.as_bytes()).unwrap_err();
        assert!(err2.to_string().contains("missing accessToken"), "{err2}");
    }

    #[test]
    fn empty_and_invalid_input_rejected() {
        assert!(parse(b"").is_err());
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn normalize_epoch_handles_seconds_and_millis() {
        assert_eq!(normalize_epoch(0), 0);
        assert_eq!(normalize_epoch(-5), 0);
        assert_eq!(normalize_epoch(1735689600), 1735689600);
        assert_eq!(normalize_epoch(1735689600000), 1735689600);
    }

    #[test]
    fn is_intl_uses_trimmed_lowercased_ai_suffix() {
        // 对齐 Go：HasSuffix(ToLower(TrimSpace(domain)), ".ai")
        let cases: &[(&str, bool)] = &[
            ("https://copilot.tencent.com", false), // 国服
            ("https://workbuddy.ai", true),
            ("https://WORKUDDY.AI", true),   // 大小写不敏感
            ("  https://workbuddy.ai  ", true), // 首尾空白不敏感
            ("https://api.workbuddy.ai", true), // 子域名同样命中
            ("", false),
            ("https://copilot.tencent.com.ai", true), // 后缀判定，非包含匹配
        ];
        for (domain, want) in cases {
            let a = Auth { domain: (*domain).into(), ..Default::default() };
            assert_eq!(a.is_intl(), *want, "domain={domain:?} want={want}");
        }
    }

    #[test]
    fn needs_refresh_semantics() {
        use std::time::Duration;
        let mut a = Auth::default();
        // expires_at<=0 → 需要刷新（无过期信息，保守）
        a.expires_at = 0;
        assert!(a.needs_refresh(Duration::from_secs(600)));

        // 已过期 → 需要刷新
        a.expires_at = 1;
        assert!(a.needs_refresh(Duration::from_secs(600)));

        // 远期 → 不需要
        a.expires_at = chrono::Utc::now().timestamp() + 7200;
        assert!(!a.needs_refresh(Duration::from_secs(600)));

        // 落在 skew 窗口内 → 需要
        a.expires_at = chrono::Utc::now().timestamp() + 60;
        assert!(a.needs_refresh(Duration::from_secs(600)));
    }

    #[test]
    fn save_atomic_roundtrip_preserves_credit() {
        let dir = std::env::temp_dir().join(format!("wbswg-auth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("workbuddy-u1.json");

        let mut a = Auth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1700000000,
            uid: "u1".into(),
            soonest_expire_at: 1735689600,
            ..Default::default()
        };
        a.file_path = fp.to_string_lossy().to_string();
        a.save_atomic().unwrap();

        let raw = std::fs::read(&fp).unwrap();
        let back = parse(&raw).unwrap();
        assert_eq!(back.access_token, "at");
        assert_eq!(back.refresh_token, "rt");
        assert_eq!(back.uid, "u1");
        // credit 块必须保留
        assert_eq!(back.soonest_expire_at, 1735689600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_atomic_refuses_empty_token() {
        let mut a = Auth::default();
        a.file_path = r"C:\whatever.json".into();
        let err = a.save_atomic().unwrap_err();
        assert!(err.to_string().contains("empty accessToken"), "{err}");
    }

    #[test]
    fn save_atomic_requires_file_path() {
        let a = Auth {
            access_token: "at".into(),
            ..Default::default()
        };
        let err = a.save_atomic().unwrap_err();
        assert!(err.to_string().contains("no FilePath"), "{err}");
    }

    #[test]
    fn load_dir_scans_and_skips_bad_files() {
        let dir = std::env::temp_dir().join(format!("wbswg-dirtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(
            dir.join("workbuddy-a.json"),
            r#"{"accessToken":"a","uid":"a"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("workbuddy-b.json"),
            r#"{"accessToken":"b","uid":"b"}"#,
        )
        .unwrap();
        // 坏文件：缺 accessToken → 跳过
        std::fs::write(dir.join("workbuddy-bad.json"), r#"{"uid":"x"}"#).unwrap();
        // 非 workbuddy 前缀 → 不扫
        std::fs::write(dir.join("other.json"), r#"{"accessToken":"o","uid":"o"}"#).unwrap();

        let loaded = load_dir(&dir.to_string_lossy()).unwrap();
        assert_eq!(loaded.len(), 2, "应扫到 2 个有效凭证: {loaded:?}");
        let mut uids: Vec<_> = loaded.iter().map(|a| a.uid.as_str()).collect();
        uids.sort();
        assert_eq!(uids, vec!["a", "b"]);
        // file_path 应被回填
        assert!(loaded.iter().all(|a| !a.file_path.is_empty()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_missing_directory_returns_empty() {
        let out = load_dir("/nonexistent/path/for/wbswg").unwrap();
        assert!(out.is_empty());
    }
}
