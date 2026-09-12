//! 网关配置：JSON 文件 + `WB2A_*` 环境变量覆盖 + normalize 回落。
//!
//! 对应 Go 源文件 `cmd/server/config.go`，行为逐条对齐，包括几处非显然语义：
//!
//! 1. **bool 开关「缺省 true」**：Go 侧依赖「先 `Default()` 再 `json.Unmarshal` 覆盖，
//!    键缺席时保留 true」。serde 的 `#[serde(default = "...")]` 在**键缺席**时也生效，
//!    因此这里对每个开关都显式标注默认构造函数，做到同样的缺席/显式-false 区分。
//! 2. **空数组 = 未配置 → 回落默认**：JSON 里的 `[]` / `null` 会把 serde 默认值覆盖掉，
//!    与 Go 侧「空数组被 unmarshal 覆盖」一致，回落逻辑放在 [`Config::normalize`]。
//! 3. **`<=0` 一律视为未设置**：timeouts / breaker threshold 等沿用 Go 侧口径，
//!    「0」不是「禁用」，而是「走默认」。

use serde::Deserialize;
use std::time::Duration;

use crate::error::{GatewayError, Result};

/// 默认监听地址。
fn default_listen() -> String {
    ":7863".into()
}
/// 默认凭证目录。
fn default_auth_dir() -> String {
    "./auths".into()
}
/// 默认状态文件。
fn default_state_file() -> String {
    "./data/state.json".into()
}
/// 开关类「缺省 true」的 serde 默认构造。
fn default_true() -> bool {
    true
}

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 监听地址，如 `":7863"`。裸端口（无冒号）时 normalize 阶段补前导冒号。
    #[serde(default = "default_listen")]
    pub listen: String,
    /// API Key，空 = 不鉴权。
    pub api_key: String,
    /// 凭证目录。
    #[serde(default = "default_auth_dir")]
    pub auth_dir: String,
    /// 池状态文件路径。
    #[serde(default = "default_state_file")]
    pub state_file: String,

    /// 冷却相关。
    pub cooldown: CooldownConfig,
    /// 定时任务相关。
    pub schedule: ScheduleConfig,
    /// 上游请求参数。
    pub upstream: UpstreamConfig,
    /// 特性开关。
    pub features: FeaturesConfig,
    /// Upstash Redis 快照。
    pub upstash: UpstashConfig,
    /// 账号池参数。
    pub pool: PoolConfig,
    /// 会话粘性。
    #[serde(rename = "session_sticky")]
    pub session_sticky: SessionStickyConfig,

    /// normalize 后的解析值（不参与反序列化）。
    #[serde(skip)]
    pub parsed: ParsedConfig,
}

/// duration 字符串在 normalize 后的解析结果。
#[derive(Debug, Clone, Default)]
pub struct ParsedConfig {
    /// `cooldown.soft_rate` 解析结果。
    pub soft_rate: Duration,
    /// `pool.breaker_cooldown` 解析结果。
    pub breaker_cooldown: Duration,
    /// `pool.breaker_cooldown_max` 解析结果。
    pub breaker_cooldown_max: Duration,
    /// `session_sticky.ttl` 解析结果。
    pub session_ttl: Duration,
    /// `session_sticky.gc_interval` 解析结果。
    pub session_gc_interval: Duration,
    /// `pool.credit_refresh_interval` 解析结果。
    pub credit_refresh_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            api_key: String::new(),
            auth_dir: default_auth_dir(),
            state_file: default_state_file(),
            cooldown: CooldownConfig::default(),
            schedule: ScheduleConfig::default(),
            upstream: UpstreamConfig::default(),
            features: FeaturesConfig::default(),
            upstash: UpstashConfig::default(),
            pool: PoolConfig::default(),
            session_sticky: SessionStickyConfig::default(),
            parsed: ParsedConfig::default(),
        }
    }
}

/// 冷却配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CooldownConfig {
    /// 429 软冷却时长字符串，默认 `"60s"`。
    #[serde(rename = "soft_rate")]
    pub soft_rate: String,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self { soft_rate: "60s".into() }
    }
}

/// 定时任务配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// 签到时点（小时），默认 `[9, 21]`。
    #[serde(rename = "checkin_hours")]
    pub checkin_hours: Vec<i32>,
    /// token 保活时点（小时），默认 `[22]`。
    #[serde(rename = "keepalive_hours")]
    pub keepalive_hours: Vec<i32>,
    /// 签到开关，缺省 true。
    #[serde(rename = "checkin_enabled", default = "default_true")]
    pub checkin_enabled: bool,
    /// 保活开关，缺省 true。
    #[serde(rename = "keepalive_enabled", default = "default_true")]
    pub keepalive_enabled: bool,
    /// 签到区域范围：`"cn"`（缺省，仅国服）/ `"all"`。
    #[serde(rename = "checkin_scope")]
    pub checkin_scope: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            checkin_hours: vec![9, 21],
            keepalive_hours: vec![22],
            checkin_enabled: true,
            keepalive_enabled: true,
            checkin_scope: "cn".into(),
        }
    }
}

/// 上游请求配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// 短 RPC 总时长上限（秒），默认 120。
    #[serde(rename = "timeout_seconds")]
    pub timeout_seconds: i64,
    /// SSE 首字节前上限（秒）；`<=0` 回落 timeout_seconds。
    #[serde(rename = "header_timeout_seconds")]
    pub header_timeout_seconds: i64,
    /// SSE 流中空闲上限（秒）；`<=0` 回落 300。
    #[serde(rename = "idle_timeout_seconds")]
    pub idle_timeout_seconds: i64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            timeout_seconds: 120,
            header_timeout_seconds: 0,
            idle_timeout_seconds: 0,
        }
    }
}

/// 特性开关。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FeaturesConfig {
    /// 出站请求体黑名单指纹脱敏，默认 true。
    #[serde(rename = "sanitize_blacklist_fingerprints", default = "default_true")]
    pub sanitize_blacklist_fingerprints: bool,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            sanitize_blacklist_fingerprints: true,
        }
    }
}

/// Upstash Redis 配置。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct UpstashConfig {
    /// 连接串或 host；空 = 纯内存模式。
    pub url: String,
    /// url 非完整连接串时用于组装 `rediss://default:<token>@<host>:6379`。
    pub token: String,
}

/// 账号池配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// 单账号最大在途请求数，0 = 不限，默认 3。
    #[serde(rename = "max_in_flight")]
    pub max_in_flight: i64,
    /// 连续失败触发熔断阈值，默认 3。
    #[serde(rename = "breaker_threshold")]
    pub breaker_threshold: i64,
    /// 基础熔断时长字符串，默认 `"30m"`。
    #[serde(rename = "breaker_cooldown")]
    pub breaker_cooldown: String,
    /// 指数退避封顶字符串，默认 `"6h"`。
    #[serde(rename = "breaker_cooldown_max")]
    pub breaker_cooldown_max: String,
    /// 闲置补偿：每小时未用增加的权重，默认 0.5。
    #[serde(rename = "idle_weight_per_hour")]
    pub idle_weight_per_hour: f64,
    /// 闲置补偿封顶，默认 5.0。
    #[serde(rename = "idle_weight_max")]
    pub idle_weight_max: f64,
    /// 积分到期巡检周期字符串，默认 `"15m"`。
    #[serde(rename = "credit_refresh_interval")]
    pub credit_refresh_interval: String,
    /// 积分到期巡检开关，缺省 true。
    #[serde(rename = "credit_refresh_enabled", default = "default_true")]
    pub credit_refresh_enabled: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 3,
            breaker_threshold: 3,
            breaker_cooldown: "30m".into(),
            breaker_cooldown_max: "6h".into(),
            idle_weight_per_hour: 0.5,
            idle_weight_max: 5.0,
            credit_refresh_interval: "15m".into(),
            credit_refresh_enabled: true,
        }
    }
}

/// 会话粘性配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SessionStickyConfig {
    /// 会话粘性开关，缺省 true。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 绑定 TTL 字符串，默认 `"30m"`。
    pub ttl: String,
    /// GC 周期字符串，默认 `"5m"`。
    #[serde(rename = "gc_interval")]
    pub gc_interval: String,
}

impl Default for SessionStickyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: "30m".into(),
            gc_interval: "5m".into(),
        }
    }
}

impl Config {
    /// 从文件加载，再用环境变量覆盖，最后 normalize。
    pub fn load(path: &str) -> Result<Self> {
        let mut c = if path.is_empty() {
            Self::default()
        } else {
            let raw = std::fs::read(path)
                .map_err(|e| GatewayError::ConfigRead(format!("{path}: {e}")))?;
            serde_json::from_slice::<Self>(&raw)
                .map_err(|e| GatewayError::ConfigParse(format!("{path}: {e}")))?
        };
        c.apply_env();
        c.normalize()?;
        Ok(c)
    }

    /// 从 JSON 字节加载（测试用，不读文件系统、不读环境变量）。
    pub fn from_json(raw: &[u8]) -> Result<Self> {
        let mut c = serde_json::from_slice::<Self>(raw)
            .map_err(|e| GatewayError::ConfigParse(e.to_string()))?;
        c.normalize()?;
        Ok(c)
    }

    /// `WB2A_*` 环境变量覆盖。
    ///
    /// 与 Go 侧 `applyEnv` 一致：**只有非空且解析成功才覆盖**，
    /// 非法值静默保留原值（不报错），避免环境变量笔误导致网关起不来。
    pub fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("WB2A_LISTEN") {
            if !v.is_empty() {
                self.listen = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_API_KEY") {
            if !v.is_empty() {
                self.api_key = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_AUTH_DIR") {
            if !v.is_empty() {
                self.auth_dir = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_STATE_FILE") {
            if !v.is_empty() {
                self.state_file = v;
            }
        }
        if let Ok(v) = std::env::var("WB2A_SOFT_RATE") {
            if !v.is_empty() {
                self.cooldown.soft_rate = v;
            }
        }
        // 数值类：解析失败保持原值（与 Go 侧 strconv 失败不动字段一致）。
        Self::env_i64("WB2A_TIMEOUT_SECONDS", &mut self.upstream.timeout_seconds);
        Self::env_i64(
            "WB2A_HEADER_TIMEOUT_SECONDS",
            &mut self.upstream.header_timeout_seconds,
        );
        Self::env_i64(
            "WB2A_IDLE_TIMEOUT_SECONDS",
            &mut self.upstream.idle_timeout_seconds,
        );
        if let Ok(v) = std::env::var("WB2A_SANITIZE_FINGERPRINTS") {
            if let Ok(b) = v.parse::<bool>() {
                self.features.sanitize_blacklist_fingerprints = b;
            }
        }
    }

    /// 读取整数环境变量，仅在存在且解析成功时写入。
    fn env_i64(key: &str, slot: &mut i64) {
        if let Ok(v) = std::env::var(key) {
            if let Ok(n) = v.trim().parse::<i64>() {
                *slot = n;
            }
        }
    }

    /// 解析 Go 风格的 duration 字符串（`"60s"` / `"1m30s"` / `"2h45m"` / `"100ms"` / `"0"`）。
    ///
    /// 与 Go `time.ParseDuration` 的语义对齐点：
    ///   - 必须有单位（裸 `"60"` 报错），唯一例外是 `"0"`（Go 允许零值无单位）；
    ///   - 支持复合表达式（按「数字 + 单位」分段累加）；
    ///   - 不支持小数秒以外的科学计数法，也不支持负时长（配置场景无意义）。
    ///
    /// 单位集合：`ns` / `us` / `µs` / `ms` / `s` / `m` / `h`。
    pub fn parse_go_duration(s: &str) -> Result<Duration> {
        let s = s.trim();
        if s.is_empty() {
            return Err(GatewayError::ConfigInvalid(format!("empty duration")));
        }
        // Go 允许 "0" 无单位（time.ParseDuration 的特例）。
        if s == "0" {
            return Ok(Duration::ZERO);
        }

        let bytes = s.as_bytes();
        let mut total_nanos: u128 = 0;
        let mut i = 0usize;
        let mut saw_unit = false;

        while i < bytes.len() {
            // 1) 读数字段（含可选前导 +/- 与小数点）
            let start = i;
            if matches!(bytes[i], b'+' | b'-') {
                i += 1;
            }
            let digits_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            if i == digits_start {
                return Err(GatewayError::ConfigInvalid(format!(
                    "missing number at position {start} in {s:?}"
                )));
            }
            if i == start {
                return Err(GatewayError::ConfigInvalid(format!(
                    "missing number at position {start} in {s:?}"
                )));
            }
            let num_str = &s[start..i];

            // 2) 读单位段：优先匹配两字符单位，再退一字符
            let unit = if s[i..].starts_with("ns") {
                i += 2;
                "ns"
            } else if s[i..].starts_with("us") || s[i..].starts_with("µs") {
                // "µs" 是 2 字节 UTF-8，按字符切片处理
                if s[i..].starts_with("µs") {
                    i += "µs".len();
                    "us"
                } else {
                    i += 2;
                    "us"
                }
            } else if s[i..].starts_with("ms") {
                i += 2;
                "ms"
            } else {
                match bytes.get(i) {
                    Some(b's') => {
                        i += 1;
                        "s"
                    }
                    Some(b'm') => {
                        i += 1;
                        "m"
                    }
                    Some(b'h') => {
                        i += 1;
                        "h"
                    }
                    _ => {
                        return Err(GatewayError::ConfigInvalid(format!(
                            "missing or unknown unit at position {i} in {s:?}"
                        )));
                    }
                }
            };
            saw_unit = true;

            // 3) 累加
            let value: f64 = num_str.parse().map_err(|_| {
                GatewayError::ConfigInvalid(format!("bad number {num_str:?} in {s:?}"))
            })?;
            if value < 0.0 {
                return Err(GatewayError::ConfigInvalid(format!(
                    "negative duration {s:?}"
                )));
            }
            let nanos = unit_nanos(unit)
                .ok_or_else(|| {
                    GatewayError::ConfigInvalid(format!("unknown unit {unit:?} in {s:?}"))
                })?;
            let scaled = value * nanos as f64;
            if scaled < 0.0 || !scaled.is_finite() || scaled > u128::MAX as f64 {
                return Err(GatewayError::ConfigInvalid(format!(
                    "duration overflow {s:?}"
                )));
            }
            total_nanos += scaled.round() as u128;
        }

        if !saw_unit {
            return Err(GatewayError::ConfigInvalid(format!(
                "missing unit in duration {s:?}"
            )));
        }

        let nanos = u64::try_from(total_nanos).map_err(|_| {
            GatewayError::ConfigInvalid(format!("duration out of range {s:?}"))
        })?;
        Ok(Duration::from_nanos(nanos))
    }

    /// normalize：解析 duration、补默认值、校验合法性。
    ///
    /// 与 Go 侧 `Config.normalize` 逐条对应。
    pub fn normalize(&mut self) -> Result<()> {
        self.parsed.soft_rate =
            Self::parse_go_duration(&self.cooldown.soft_rate).map_err(|e| {
                GatewayError::ConfigInvalid(format!("cooldown.soft_rate: {e}"))
            })?;
        self.parsed.breaker_cooldown =
            Self::parse_go_duration(&self.pool.breaker_cooldown).map_err(|e| {
                GatewayError::ConfigInvalid(format!("pool.breaker_cooldown: {e}"))
            })?;
        self.parsed.breaker_cooldown_max =
            Self::parse_go_duration(&self.pool.breaker_cooldown_max).map_err(|e| {
                GatewayError::ConfigInvalid(format!("pool.breaker_cooldown_max: {e}"))
            })?;
        self.parsed.session_ttl =
            Self::parse_go_duration(&self.session_sticky.ttl).map_err(|e| {
                GatewayError::ConfigInvalid(format!("session_sticky.ttl: {e}"))
            })?;
        self.parsed.session_gc_interval =
            Self::parse_go_duration(&self.session_sticky.gc_interval).map_err(|e| {
                GatewayError::ConfigInvalid(format!("session_sticky.gc_interval: {e}"))
            })?;

        if self.pool.breaker_threshold <= 0 {
            self.pool.breaker_threshold = 3;
        }
        if self.pool.idle_weight_per_hour <= 0.0 {
            self.pool.idle_weight_per_hour = 0.5;
        }
        if self.pool.idle_weight_max <= 0.0 {
            self.pool.idle_weight_max = 5.0;
        }

        // 积分巡检周期：空/非法一律回落默认（不报错，避免老 config 启动失败）。
        self.parsed.credit_refresh_interval =
            Self::parse_go_duration(&self.pool.credit_refresh_interval)
                .unwrap_or(Duration::from_secs(15 * 60));
        if self.parsed.credit_refresh_interval <= Duration::ZERO {
            self.parsed.credit_refresh_interval = Duration::from_secs(15 * 60);
        }

        if self.upstream.timeout_seconds <= 0 {
            self.upstream.timeout_seconds = 120;
        }
        if self.upstream.header_timeout_seconds <= 0 {
            self.upstream.header_timeout_seconds = self.upstream.timeout_seconds;
        }
        if self.upstream.idle_timeout_seconds <= 0 {
            self.upstream.idle_timeout_seconds = 300;
        }

        // 裸端口（无冒号）补前导冒号。
        if !self.listen.contains(':') {
            self.listen = format!(":{}", self.listen);
        }

        // 空数组 = 未配置 → 回落默认。
        if self.schedule.checkin_hours.is_empty() {
            self.schedule.checkin_hours = vec![9, 21];
        }
        if self.schedule.keepalive_hours.is_empty() {
            self.schedule.keepalive_hours = vec![22];
        }
        self.schedule.checkin_scope = normalize_checkin_scope(&self.schedule.checkin_scope);

        self.validate_schedule_hours()?;
        Ok(())
    }

    /// 校验排程小时落在 0-23，错误信息指向正确的开关（与 Go 侧一致）。
    fn validate_schedule_hours(&self) -> Result<()> {
        check_hour_range("schedule.checkin_hours", "checkin_enabled", &self.schedule.checkin_hours)?;
        check_hour_range(
            "schedule.keepalive_hours",
            "keepalive_enabled",
            &self.schedule.keepalive_hours,
        )
    }

    /// 签到作用域是否覆盖全部区域。
    pub fn checkin_scope_all(&self) -> bool {
        normalize_checkin_scope(&self.schedule.checkin_scope) == "all"
    }
}

/// 归一化签到区域范围；无法识别一律回落 `"cn"`。
pub fn normalize_checkin_scope(v: &str) -> String {
    if v.trim().eq_ignore_ascii_case("all") {
        return "all".into();
    }
    "cn".into()
}

/// 校验小时范围，非法时报错并提示改用显式开关。
fn check_hour_range(field: &str, switch_key: &str, hours: &[i32]) -> Result<()> {
    for &h in hours {
        if !(0..=23).contains(&h) {
            return Err(GatewayError::ConfigInvalid(format!(
                "{field}: {h} 不是合法小时（0-23）；如要关闭该任务请设 schedule.{switch_key}=false"
            )));
        }
    }
    Ok(())
}

/// Go duration 单位对应的纳秒数。
fn unit_nanos(unit: &str) -> Option<u128> {
    Some(match unit {
        "ns" => 1,
        "us" | "µs" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60 * 1_000_000_000,
        "h" => 60 * 60 * 1_000_000_000,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_go_default() {
        let c = Config::default();
        assert_eq!(c.listen, ":7863");
        assert_eq!(c.auth_dir, "./auths");
        assert_eq!(c.state_file, "./data/state.json");
        assert_eq!(c.cooldown.soft_rate, "60s");
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
        assert_eq!(c.schedule.keepalive_hours, vec![22]);
        assert!(c.schedule.checkin_enabled);
        assert!(c.schedule.keepalive_enabled);
        assert_eq!(c.schedule.checkin_scope, "cn");
        assert_eq!(c.upstream.timeout_seconds, 120);
        assert!(c.features.sanitize_blacklist_fingerprints);
        assert_eq!(c.pool.max_in_flight, 3);
        assert_eq!(c.pool.breaker_threshold, 3);
        assert_eq!(c.pool.breaker_cooldown, "30m");
        assert_eq!(c.pool.breaker_cooldown_max, "6h");
        assert!((c.pool.idle_weight_per_hour - 0.5).abs() < f64::EPSILON);
        assert!((c.pool.idle_weight_max - 5.0).abs() < f64::EPSILON);
        assert!(c.pool.credit_refresh_enabled);
        assert!(c.session_sticky.enabled);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(
            Config::parse_go_duration("60s").unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(
            Config::parse_go_duration("30m").unwrap(),
            Duration::from_secs(1800)
        );
        assert_eq!(
            Config::parse_go_duration("6h").unwrap(),
            Duration::from_secs(21600)
        );
        assert_eq!(
            Config::parse_go_duration("0").unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            Config::parse_go_duration("1m30s").unwrap(),
            Duration::from_secs(90)
        );
        assert_eq!(
            Config::parse_go_duration("2h45m").unwrap(),
            Duration::from_secs(9900)
        );
        assert_eq!(
            Config::parse_go_duration("100ms").unwrap(),
            Duration::from_millis(100)
        );
        // 缺少单位 → 报错（与 Go time.ParseDuration 一致）
        assert!(Config::parse_go_duration("60").is_err());
        assert!(Config::parse_go_duration("abc").is_err());
        assert!(Config::parse_go_duration("").is_err());
    }

    #[test]
    fn normalize_fills_defaults_and_falls_back() {
        let raw = br#"{"listen":"7863"}"#;
        let c = Config::from_json(raw).unwrap();
        // 裸端口补冒号
        assert_eq!(c.listen, ":7863");
        // 开关缺席 → true
        assert!(c.schedule.checkin_enabled);
        assert!(c.pool.credit_refresh_enabled);
        // header 回落 timeout
        assert_eq!(c.upstream.header_timeout_seconds, 120);
        // idle 回落 300
        assert_eq!(c.upstream.idle_timeout_seconds, 300);
        // duration 解析
        assert_eq!(c.parsed.soft_rate, Duration::from_secs(60));
        assert_eq!(c.parsed.breaker_cooldown, Duration::from_secs(1800));
        assert_eq!(c.parsed.breaker_cooldown_max, Duration::from_secs(21600));
        assert_eq!(c.parsed.session_ttl, Duration::from_secs(1800));
        assert_eq!(c.parsed.session_gc_interval, Duration::from_secs(300));
        assert_eq!(c.parsed.credit_refresh_interval, Duration::from_secs(900));
    }

    #[test]
    fn explicit_false_disables_switch() {
        let raw = br#"{"schedule":{"checkin_enabled":false,"keepalive_enabled":false},"features":{"sanitize_blacklist_fingerprints":false}}"#;
        let c = Config::from_json(raw).unwrap();
        assert!(!c.schedule.checkin_enabled);
        assert!(!c.schedule.keepalive_enabled);
        assert!(!c.features.sanitize_blacklist_fingerprints);
        // 显式 false 不应把小时也清掉（默认仍补齐）
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
    }

    #[test]
    fn empty_array_falls_back_to_default_hours() {
        let raw = br#"{"schedule":{"checkin_hours":[],"keepalive_hours":[]}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.schedule.checkin_hours, vec![9, 21]);
        assert_eq!(c.schedule.keepalive_hours, vec![22]);
    }

    #[test]
    fn checkin_scope_normalization() {
        let c = Config::from_json(br#"{"schedule":{"checkin_scope":"ALL"}}"#).unwrap();
        assert_eq!(c.schedule.checkin_scope, "all");
        assert!(c.checkin_scope_all());
        let c2 = Config::from_json(br#"{"schedule":{"checkin_scope":"weird"}}"#).unwrap();
        assert_eq!(c2.schedule.checkin_scope, "cn");
        assert!(!c2.checkin_scope_all());
    }

    #[test]
    fn invalid_hours_rejected_with_switch_hint() {
        let raw = br#"{"schedule":{"checkin_hours":[-1]}}"#;
        let err = Config::from_json(raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("不是合法小时"), "msg={msg}");
        assert!(msg.contains("checkin_enabled"), "应提示正确开关: {msg}");

        let raw = br#"{"schedule":{"keepalive_hours":[24]}}"#;
        let err = Config::from_json(raw).unwrap_err();
        assert!(err.to_string().contains("keepalive_enabled"));
    }

    #[test]
    fn invalid_duration_rejected_with_field_name() {
        let raw = br#"{"cooldown":{"soft_rate":"not-a-duration"}}"#;
        let err = Config::from_json(raw).unwrap_err();
        assert!(err.to_string().contains("cooldown.soft_rate"), "{}", err);
    }

    #[test]
    fn bad_json_returns_parse_error() {
        let err = Config::from_json(b"{not json").unwrap_err();
        assert!(matches!(err, GatewayError::ConfigParse(_)), "{err:?}");
    }

    #[test]
    fn less_than_zero_ints_treated_as_unset() {
        let raw = br#"{"upstream":{"timeout_seconds":-5},"pool":{"breaker_threshold":0,"idle_weight_per_hour":0}}"#;
        let c = Config::from_json(raw).unwrap();
        assert_eq!(c.upstream.timeout_seconds, 120);
        assert_eq!(c.pool.breaker_threshold, 3);
        assert!((c.pool.idle_weight_per_hour - 0.5).abs() < f64::EPSILON);
    }
}
