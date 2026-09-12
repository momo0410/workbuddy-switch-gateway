//! 账号池：单一状态机（健康/冷却/熔断）+ 在途租约 + 到期分层 + 加权挑选。
//!
//! 对应 Go 源文件 `internal/pool/pool.go`。
//!
//! # 状态模型
//!
//! 每个账号有三个正交维度：
//! 1. **健康维度**（唯一权威）：`healthy = !disabled && !until 生效 && !breaker_until 生效`
//!    - `until`：按错误类型的即时冷却（软 429 / 硬 余额耗尽）
//!    - `breaker_until`：连续失败累计触发熔断的指数退避截止
//! 2. **并发维度**：`in_flight`（在途租约，运行态）
//! 3. **统计维度**：`success_count` / `err_total` / `last_used` / `last_success` / `last_err`
//!
//! # 挑选策略（两级）
//!
//! 1. **到期分层**：先按「最近到期积分」的到期日把 healthy 候选分组，只保留最早到期的一档
//!    ——优先消耗快过期的额度；到期日未知的账号排最后。
//! 2. **档内挑选**：同档账号按「闲置补偿 + 成功率」加权取 Top5，再加权随机抽签。
//!
//! 所有账号都没有到期信息时自动退回原三因子口径
//!（credits 占比 ×10 + 闲置补偿 + 成功率 ×3）。
//!
//! # 与 Go 实现的关键差异
//!
//! - Go 用 `&entry` 指针在锁内原地改；Rust 用 `Vec<Entry>` + 索引，
//!   以避免 `Arc<RwLock<Entry>>` 的复杂度和额外分配。
//! - **保留了 Go 版修复后的 LRU 平局处理**（[`Entry::last_used_seq`]），
//!   这是 Go 侧 Windows 时钟粒度导致惊群的修复，迁移时必须带上。

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::auth::Auth;

/// 冷却类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoolKind {
    /// 余额不足 → 冷却到次日 04:00（等签到恢复）。
    #[default]
    Soft,
    /// 429 → 短冷却。
    Hard,
}

impl CoolKind {
    /// 序列化名，与 Go 侧 `String()` 一致（供 /status 透出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            CoolKind::Soft => "soft_rate",
            CoolKind::Hard => "hard_credit",
        }
    }
}

/// 单账号对外暴露的状态（脱敏），对应 Go 侧 `pool.Status`。
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// 账号 UID。
    pub uid: String,
    /// 昵称。
    pub nickname: String,
    /// 剩余积分。
    pub credits: i64,
    /// 是否冷却中。
    pub cooling: bool,
    /// 冷却类型名（`"soft_rate"` / `"hard_credit"`）。
    pub cool_kind: String,
    /// 冷却剩余秒数。
    pub cool_remaining_sec: i64,
    /// 冷却截止时刻。
    pub until: Option<SystemTime>,
    /// 冷却原因。
    pub reason: String,
    /// 是否被禁用。
    pub disabled: bool,
    /// 累计成功数。
    pub success_count: i64,
    /// 累计错误数。
    pub err_total: i64,
    /// 最近成功时刻。
    pub last_success: Option<SystemTime>,
    /// 最近错误时刻。
    pub last_err: Option<SystemTime>,

    // 运行态（不持久化）
    /// 在途请求数。
    pub in_flight: i64,
    /// 熔断器连续失败计数。
    pub breaker_fails: i64,
    /// 熔断截止时刻。
    pub breaker_until: Option<SystemTime>,

    /// 「最近到期积分」的到期时刻（Unix 秒）；0 = 未知。
    pub soonest_expire_at: i64,
    /// 到期日（本地日期 `YYYY-MM-DD`），分层用的档位键。
    pub expire_day: String,
}

/// 池内部账号条目。
#[derive(Debug, Clone)]
struct Entry {
    a: Auth,

    credits: i64,
    success_count: i64,
    err_total: i64,
    last_err: Option<SystemTime>,
    last_success: Option<SystemTime>,
    cool_kind: Option<CoolKind>,
    /// 冷却截止（即时冷却）。
    until: Option<SystemTime>,
    disabled: bool,
    reason: String,
    /// 最近被选中时刻（防并发撞号）。
    last_used: Option<SystemTime>,

    /// 熔断截止（指数退避）。
    breaker_until: Option<SystemTime>,
    /// 连续失败计数（熔断用，唯一权威）。
    fails: i64,
    /// 已熔断次数（指数退避的指数）。
    retry_count: i64,

    /// 单账号在途请求数（运行态，不持久化）。
    in_flight: i64,

    /// 「最近到期积分」的到期时刻（Unix 秒）；0 = 未知。
    expire_at: i64,

    /// 单调序号：打破 `last_used` 的时钟粒度平局。
    ///
    /// 详见模块文档「与 Go 实现的关键差异」；对应 Go 侧 `entry.lastUsedSeq`。
    /// 0 表示从未被选中（与 `last_used = None` 对应）。
    last_used_seq: i64,
}

impl Entry {
    /// 返回「最近到期积分」的到期日（`YYYY-MM-DD`），空串表示未知。
    ///
    /// 用「日」而非精确时刻做分层键：上游额度按天失效，
    /// 同一天到期的账号应视为同一档。
    fn expiry_day_key(&self) -> String {
        if self.expire_at <= 0 {
            return String::new();
        }
        // Go 侧用 time.Unix(expireAt,0).In(time.Local).Format("2006-01-02")。
        expiry_day_key(self.expire_at)
    }

    /// 报告账号当前是否可选（未禁用、未处于任一冷却/熔断期）。
    fn healthy(&self, now: SystemTime) -> bool {
        if self.disabled {
            return false;
        }
        if let Some(u) = self.until {
            if now < u {
                return false;
            }
        }
        if let Some(b) = self.breaker_until {
            if now < b {
                return false;
            }
        }
        true
    }
}

/// 把 Unix 秒时间戳格式化为本地日期 `YYYY-MM-DD`。
///
/// 与 Go 侧 `time.Unix(n,0).In(time.Local).Format("2006-01-02")` 等价。
fn expiry_day_key(unix_secs: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d").to_string(),
        None => String::new(),
    }
}

/// 状态文件中的单个账号（与 Go 侧 `stateAccount` 同 tag）。
///
/// 只读入需要持久化的维度：冷却/禁用/统计/到期。
/// `last_used` / 在途 / 熔断的 fails/retry 是运行态，**不持久化**
///（与 Go 一致：重启后熔断计数归零，但 breaker_until 的冷却语义由 until 承担）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateAccount {
    /// 剩余积分。
    #[serde(default)]
    pub credits: i64,
    /// 是否被禁用。
    #[serde(default)]
    pub disabled: bool,
    /// 禁用/冷却原因。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// 冷却截止时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub until: Option<SystemTime>,
    /// 冷却类型编号（与 Go `CoolKind` 的 iota 一致：0=soft, 1=hard）。
    #[serde(default)]
    pub cool_kind: i32,
    /// 累计成功数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub success_count: i64,
    /// 累计错误数。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub err_total: i64,
    /// 旧版连续错误计数（仅作一次性迁移源，不再回写）。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub err_count: i64,
    /// 最近成功时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_success: Option<SystemTime>,
    /// 最近错误时刻。
    #[serde(
        default,
        deserialize_with = "de_go_time",
        serialize_with = "ser_go_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_err: Option<SystemTime>,
    /// 「最近到期积分」的到期时刻（Unix 秒）；0/缺省 = 未知。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub expire_at: i64,
}

/// 反序列化 Go `time.Time` 的 RFC3339 字符串。
///
/// 关键边界：Go 的**零值时间**序列化为 `"0001-01-01T00:00:00Z"`，
/// 直接交给 `SystemTime` 会在 `SystemTime::UNIX_EPOCH` 之前溢出而整文件解析失败。
/// 这里把零值（以及任何无法表示的远古时间）映射为 `None`，语义与 Go 侧
/// 「`Until.IsZero()` 表示不在冷却期」完全一致。
fn de_go_time<'de, D>(d: D) -> std::result::Result<Option<SystemTime>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let opt = Option::<String>::deserialize(d)?;
    let Some(s) = opt else { return Ok(None) };
    if s.is_empty() {
        return Ok(None);
    }
    // Go 零值时间（以及 1970 年之前的时间）→ None
    let parsed = chrono::DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom)?;
    let secs = parsed.timestamp();
    if secs <= 0 {
        return Ok(None);
    }
    Ok(Some(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
    ))
}

/// 把 `Option<SystemTime>` 序列化为 Go 可读的 RFC3339。
///
/// 保持与 Go 侧 `time.Time` 的 JSON 形态一致，便于两边互相读取 state.json。
fn ser_go_time<S>(v: &Option<SystemTime>, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        None => s.serialize_none(),
        Some(t) => {
            let d = t
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let dt = chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                .ok_or_else(|| serde::ser::Error::custom("timestamp out of range"))?;
            s.serialize_str(&dt.to_rfc3339())
        }
    }
}

/// `0` 不序列化（对齐 Go 的 `omitempty`）。
fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// 状态文件顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StateFile {
    /// uid → 账号状态。
    #[serde(default)]
    pub accounts: HashMap<String, StateAccount>,
}

impl Pool {
    /// 从 `state.json` 载入持久化状态（覆盖/插入）。
    ///
    /// 对应 Go 侧 `load()` + `applyAccountsLocked`：文件缺失或解析失败时静默返回
    ///（首次启动属正常情况），不阻断启动。
    ///
    /// 与 Go 一致的细节：
    /// - `err_total` 与旧版 `err_count` 取较大者（旧语义下 err_count 也真实发生过错误，不应丢）。
    /// - 载入的是**占位凭证**（仅 UID）；后续 `add()` 会换成完整凭证并保留这些运行态。
    pub fn load_state(&mut self) {
        let fp = self.state_fp.clone();
        if fp.is_empty() {
            return;
        }
        let Ok(raw) = std::fs::read(&fp) else {
            return;
        };
        let Ok(sf) = serde_json::from_slice::<StateFile>(&raw) else {
            return;
        };
        self.apply_accounts(sf.accounts);
        self.loaded_from_state = true;
    }

    /// 用持久化账号状态覆盖/插入。调用方语义同 Go `applyAccountsLocked`。
    fn apply_accounts(&mut self, accounts: HashMap<String, StateAccount>) {
        for (uid, s) in accounts {
            // err_total 优先；旧文件的 err_count 作一次性迁移源（取较大者）。
            let err_total = s.err_total.max(s.err_count);
            let cool_kind = match s.cool_kind {
                1 => Some(CoolKind::Hard),
                0 if s.until.is_some() => Some(CoolKind::Soft),
                _ => None,
            };
            let entry = Entry {
                a: Auth {
                    uid: uid.clone(),
                    ..Default::default()
                },
                credits: s.credits,
                success_count: s.success_count,
                err_total,
                last_err: s.last_err,
                last_success: s.last_success,
                cool_kind,
                until: s.until,
                disabled: s.disabled,
                reason: s.reason,
                last_used: None,
                breaker_until: None,
                fails: 0,
                retry_count: 0,
                in_flight: 0,
                expire_at: s.expire_at,
                last_used_seq: 0,
            };
            if let Some(&idx) = self.by_uid.get(&uid) {
                self.entries[idx] = entry;
            } else {
                let idx = self.entries.len();
                self.entries.push(entry);
                self.by_uid.insert(uid, idx);
            }
        }
    }

    /// 把当前状态落盘到 `state.json`。
    ///
    /// 仅在 `dirty` 时写（对应 Go 侧 `saveLocked` 的 dirty 门控）。
    /// 返回是否真的写了盘。
    pub fn flush(&mut self) -> bool {
        if !self.dirty || self.state_fp.is_empty() {
            return false;
        }
        let mut accounts = HashMap::new();
        for e in &self.entries {
            // 与 Go 侧 saveLocked 一致：只持久化有意义的维度
            let cool_kind = match e.cool_kind {
                Some(CoolKind::Soft) => 0,
                Some(CoolKind::Hard) => 1,
                None => 0,
            };
            accounts.insert(
                e.a.uid.clone(),
                StateAccount {
                    credits: e.credits,
                    disabled: e.disabled,
                    reason: e.reason.clone(),
                    until: e.until,
                    cool_kind,
                    success_count: e.success_count,
                    err_total: e.err_total,
                    err_count: 0,
                    last_success: e.last_success,
                    last_err: e.last_err,
                    expire_at: e.expire_at,
                },
            );
        }
        let sf = StateFile { accounts };
        let Ok(raw) = serde_json::to_string_pretty(&sf) else {
            return false;
        };
        let path = std::path::Path::new(&self.state_fp);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if crate::auth::atomic_write(path, raw.as_bytes()).is_ok() {
            self.dirty = false;
            true
        } else {
            false
        }
    }

    /// 是否曾经从 state.json 载入过（供启动日志判定恢复来源）。
    pub fn loaded_from_state(&self) -> bool {
        self.loaded_from_state
    }
}

/// 账号池。
pub struct Pool {
    by_uid: HashMap<String, usize>,
    entries: Vec<Entry>,
    /// 状态文件路径（空 = 不持久化）。
    state_fp: String,

    dirty: bool,

    // 可调参数
    breaker_threshold: i64,
    breaker_cooldown: Duration,
    breaker_cooldown_max: Duration,
    max_in_flight: i64,
    idle_weight_per_hour: f64,
    idle_weight_max: f64,

    /// 单调序号：每次选号自增，写入 [`Entry::last_used_seq`]。
    pick_seq: i64,
    /// 可选注入的确定性随机源（测试用）。
    rand: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>,
    /// 是否曾从 state.json 成功载入（启动日志用）。
    loaded_from_state: bool,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl Pool {
    /// 构建空池。`state_fp` 为空表示不持久化。
    pub fn new(state_fp: String) -> Self {
        Self {
            by_uid: HashMap::new(),
            entries: Vec::new(),
            state_fp,
            dirty: false,
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(30 * 60),
            breaker_cooldown_max: Duration::from_secs(6 * 60 * 60),
            max_in_flight: 0,
            idle_weight_per_hour: 0.5,
            idle_weight_max: 5.0,
            pick_seq: 0,
            rand: None,
            loaded_from_state: false,
        }
    }

    /// 注入熔断参数。
    pub fn set_breaker(&mut self, threshold: i64, cooldown: Duration, cooldown_max: Duration) {
        if threshold > 0 {
            self.breaker_threshold = threshold;
        }
        if cooldown > Duration::ZERO {
            self.breaker_cooldown = cooldown;
        }
        if cooldown_max > Duration::ZERO {
            self.breaker_cooldown_max = cooldown_max;
        }
    }

    /// 注入闲置补偿权重。
    pub fn set_weights(&mut self, idle_per_hour: f64, idle_max: f64) {
        if idle_per_hour > 0.0 {
            self.idle_weight_per_hour = idle_per_hour;
        }
        if idle_max > 0.0 {
            self.idle_weight_max = idle_max;
        }
    }

    /// 注入单账号最大在途请求数；0 = 不限。
    pub fn set_max_in_flight(&mut self, n: i64) {
        if n >= 0 {
            self.max_in_flight = n;
        }
    }

    /// 注入确定性随机源（仅供测试）；传 `None` 恢复真实随机。
    pub fn set_random_source(&mut self, f: Option<Box<dyn Fn(i64) -> i64 + Send + Sync>>) {
        self.rand = f;
    }

    /// 内部随机源：优先注入源，否则用进程级随机。
    fn rand_n(&self, n: i64) -> i64 {
        if let Some(f) = &self.rand {
            return f(n);
        }
        // 与 Go 侧 math/rand/v2 全局源语义一致：无需密码学强度。
        use std::cell::Cell;
        thread_local! {
            // xorshift64*，基于时间 + 地址扰动播种
            static STATE: Cell<u64> = Cell::new(seed_once());
        }
        STATE.with(|s| {
            let mut x = s.get();
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            s.set(x);
            let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
            if n <= 0 {
                return 0;
            }
            ((v >> 33) as i64) % n
        })
    }

    /// 新增或更新账号。
    pub fn add(&mut self, a: Auth) {
        if let Some(&idx) = self.by_uid.get(&a.uid) {
            // 已存在：保留运行态（在途/熔断/统计），只刷新凭证与元数据。
            let e = &mut self.entries[idx];
            let soonest = a.soonest_expire_at;
            let domain = a.domain.clone();
            e.a = a;
            if soonest > 0 {
                e.expire_at = soonest;
            }
            let _ = domain;
            return;
        }
        let uid = a.uid.clone();
        let expire_at = a.soonest_expire_at;
        let idx = self.entries.len();
        self.entries.push(Entry {
            a,
            credits: 0,
            success_count: 0,
            err_total: 0,
            last_err: None,
            last_success: None,
            cool_kind: None,
            until: None,
            disabled: false,
            reason: String::new(),
            last_used: None,
            breaker_until: None,
            fails: 0,
            retry_count: 0,
            in_flight: 0,
            expire_at,
            last_used_seq: 0,
        });
        self.by_uid.insert(uid, idx);
    }

    /// 报告账号是否已占满在途名额（max=0 不限 → 恒 false）。
    fn in_flight_full(&self, e: &Entry) -> bool {
        if self.max_in_flight <= 0 {
            return false;
        }
        e.in_flight >= self.max_in_flight
    }

    /// 占用在途名额；已满返回 false。
    pub fn acquire(&mut self, uid: &str) -> bool {
        let Some(&idx) = self.by_uid.get(uid) else {
            return false;
        };
        if self.in_flight_full(&self.entries[idx]) {
            return false;
        }
        self.entries[idx].in_flight += 1;
        true
    }

    /// 释放在途名额。
    pub fn release(&mut self, uid: &str) {
        if let Some(&idx) = self.by_uid.get(uid) {
            let e = &mut self.entries[idx];
            if e.in_flight > 0 {
                e.in_flight -= 1;
            }
        }
    }

    /// 记录一次成功：累加成功计数、清连续失败与熔断运行态。
    ///
    /// 与 Go 侧 `NoteSuccess` 一致：清 fails / retry_count / breaker_until，
    /// 不碰 until / cool_kind（那些是即时冷却，各自到期）。
    pub fn note_success(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.success_count += 1;
        e.last_success = Some(SystemTime::now());
        e.fails = 0;
        e.retry_count = 0;
        e.breaker_until = None;
        self.dirty = true;
    }

    /// 记录一次错误：喂入连续失败计数器，达到阈值触发熔断（指数退避）。
    ///
    /// 对应 Go 侧 `NoteError`：`errTotal++` 后调用 `recordBreakerFailureLocked`，
    /// 而 `fails++` 发生在 `recordBreakerFailureLocked` 内部（Go 版实现），
    /// 因此这里**不能**再额外自增，否则阈值会少一次即触发。
    pub fn note_error(&mut self, uid: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.err_total += 1;
        e.last_err = Some(SystemTime::now());
        self.record_breaker_failure(idx);
        self.dirty = true;
    }

    /// 累计一次熔断失败；达到阈值则按指数退避熔断。
    ///
    /// 熔断与冷却（until）解耦：冷却按错误类别给固定时长，
    /// 熔断则对「反复失败」逐次加长封禁。
    ///
    /// 与 Go 侧 `recordBreakerFailureLocked` 一致：**`fails++` 在本函数内完成**，
    /// 因此所有失败入口（`NoteError` / `Cooldown`）都只是调用它，不自行自增。
    fn record_breaker_failure(&mut self, idx: usize) {
        // fails++ 在此完成（与 Go 版一致）：失败入口不自行自增。
        self.entries[idx].fails += 1;
        let fails = self.entries[idx].fails;
        let retry = self.entries[idx].retry_count;
        if fails < self.breaker_threshold {
            return;
        }
        let mut d = self.breaker_cooldown;
        for _ in 0..retry {
            d = d.saturating_mul(2);
            if d >= self.breaker_cooldown_max {
                d = self.breaker_cooldown_max;
                break;
            }
        }
        let e = &mut self.entries[idx];
        e.fails = 0;
        e.retry_count += 1;
        e.breaker_until = Some(SystemTime::now() + d);
    }

    /// 即时冷却（软 429 / 硬 余额耗尽）。
    ///
    /// 冷却入口同时是熔断器的失败信号：喂入 fails，达到阈值按指数退避熔断。
    pub fn cooldown(&mut self, uid: &str, kind: CoolKind, d: Duration, reason: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.until = Some(SystemTime::now() + d);
        e.cool_kind = Some(kind);
        e.reason = reason.to_string();
        self.record_breaker_failure(idx);
        self.dirty = true;
    }

    /// 冷却到下一个 04:00（本地时区），用于余额耗尽等签到恢复。
    pub fn cooldown_until_tomorrow_4am(&mut self, uid: &str, reason: &str) {
        let now = chrono::Local::now();
        let d = next_day_4am_duration(now);
        self.cooldown(uid, CoolKind::Hard, d, reason);
    }

    /// 永久禁用（session 死亡），需人工重登。
    pub fn disable(&mut self, uid: &str, reason: &str) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.disabled = true;
        e.reason = reason.to_string();
        self.dirty = true;
    }

    /// 更新余额。
    pub fn set_credits(&mut self, uid: &str, credits: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        self.entries[idx].credits = credits;
        self.dirty = true;
    }

    /// 更新余额与到期时刻。
    pub fn set_credits_and_expiry(&mut self, uid: &str, credits: i64, soonest_expire_at: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        e.credits = credits;
        if soonest_expire_at > 0 {
            e.expire_at = soonest_expire_at;
        }
        self.dirty = true;
    }

    /// 签到后解冻：仅当 remain > 0 且账号非禁用时清冷却（余额恢复）。
    ///
    /// 注意：不碰熔断器——熔断到期或下次 chat 成功才恢复。
    pub fn reenable_if_credits(&mut self, uid: &str, remain: i64) {
        let Some(&idx) = self.by_uid.get(uid) else { return };
        let e = &mut self.entries[idx];
        if remain > 0 && !e.disabled {
            e.credits = remain;
            e.until = None;
            e.cool_kind = None;
            e.reason.clear();
        } else {
            e.credits = remain;
        }
        self.dirty = true;
    }

    /// 全量状态列表（按 UID 排序，稳定输出）。
    pub fn list(&self) -> Vec<Status> {
        let now = SystemTime::now();
        let mut out: Vec<Status> = self
            .entries
            .iter()
            .map(|e| self.status_of(e, now))
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    /// 构造单个账号的对外状态。
    fn status_of(&self, e: &Entry, now: SystemTime) -> Status {
        let cooling = e
            .until
            .map(|u| now < u)
            .unwrap_or(false)
            || e.breaker_until.map(|b| now < b).unwrap_or(false);
        let cool_remaining = e
            .until
            .filter(|u| now < *u)
            .and_then(|u| u.duration_since(now).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        Status {
            uid: e.a.uid.clone(),
            nickname: e.a.nickname.clone(),
            credits: e.credits,
            cooling,
            cool_kind: if e.cool_kind.is_some() {
                e.cool_kind.unwrap().as_str().to_string()
            } else {
                String::new()
            },
            cool_remaining_sec: cool_remaining,
            until: e.until,
            reason: e.reason.clone(),
            disabled: e.disabled,
            success_count: e.success_count,
            err_total: e.err_total,
            last_success: e.last_success,
            last_err: e.last_err,
            in_flight: e.in_flight,
            breaker_fails: e.fails,
            breaker_until: e.breaker_until,
            soonest_expire_at: e.expire_at,
            expire_day: e.expiry_day_key(),
        }
    }

    /// 五类计数：total / healthy / cooling / disabled / in_flight_full。
    pub fn counts_detailed(&self) -> (usize, usize, usize, usize, usize) {
        let now = SystemTime::now();
        let mut healthy = 0usize;
        let mut cooling = 0usize;
        let mut disabled = 0usize;
        let mut in_flight_full = 0usize;
        for e in &self.entries {
            if e.disabled {
                disabled += 1;
            }
            let is_cooling = e.until.map(|u| now < u).unwrap_or(false)
                || e.breaker_until.map(|b| now < b).unwrap_or(false);
            if is_cooling {
                cooling += 1;
            }
            if e.healthy(now) {
                healthy += 1;
                if self.in_flight_full(e) {
                    in_flight_full += 1;
                }
            }
        }
        (self.entries.len(), healthy, cooling, disabled, in_flight_full)
    }

    /// 当前是否有账号可受理（healthy 且未占满在途）。
    pub fn servable_now(&self) -> bool {
        let now = SystemTime::now();
        self.entries
            .iter()
            .any(|e| e.healthy(now) && !self.in_flight_full(e))
    }

    /// 返回当前 healthy 且未占满在途名额的 UID 列表（按 UID 排序，稳定输出）。
    pub fn available_uids(&self) -> Vec<String> {
        let now = SystemTime::now();
        let mut uids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| e.healthy(now) && !self.in_flight_full(e))
            .map(|e| e.a.uid.clone())
            .collect();
        uids.sort();
        uids
    }

    /// 按 UID 直取（若 healthy 且未占满在途），记录使用时间防撞号。
    pub fn pick_by_uid(&mut self, uid: &str) -> Option<Auth> {
        let idx = *self.by_uid.get(uid)?;
        let now = SystemTime::now();
        if !self.entries[idx].healthy(now) || self.in_flight_full(&self.entries[idx]) {
            return None;
        }
        self.mark_used(idx);
        Some(self.entries[idx].a.clone())
    }

    /// 挑号（无排除集）。
    pub fn pick(&mut self) -> Option<Auth> {
        self.pick_excluding(&[])
    }

    /// 挑号，跳过 `tried` 中的 UID。
    pub fn pick_excluding(&mut self, tried: &[String]) -> Option<Auth> {
        let now = SystemTime::now();

        // 1) 收集 healthy 且未在途占满、且不在 tried 中的候选（按 UID 排序保证确定性）
        let mut cands: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                !tried.iter().any(|t| t == &e.a.uid)
                    && e.healthy(now)
                    && !self.in_flight_full(e)
            })
            .collect();
        cands.sort_by(|&a, &b| self.entries[a].a.uid.cmp(&self.entries[b].a.uid));

        if cands.is_empty() {
            return self.pick_earliest_expiry(tried, now).map(|a| a);
        }

        // 2) 到期分层：只保留最早到期的一档
        let (cands, tiered) = self.earliest_expiry_tier(cands);

        // 3) top5 短名单：按权重降序截断（平局按 UID 升序，保证确定性）
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].credits)
            .max()
            .unwrap_or(0);
        let mut weighted: Vec<(usize, f64)> = cands
            .iter()
            .map(|&i| (i, self.weight_of(i, max_credits, now)))
            .collect();
        weighted.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| self.entries[a.0].a.uid.cmp(&self.entries[b.0].a.uid))
        });
        let mut cands: Vec<usize> = weighted.into_iter().map(|(i, _)| i).collect();
        if cands.len() > 5 {
            cands.truncate(5);
        }

        // 4) 并发防雪崩：跳过 last_used 距今 < min_pick_gap 的账号
        let eligible: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| match self.entries[i].last_used {
                None => true,
                Some(lu) => now.duration_since(lu).unwrap_or(Duration::MAX) >= MIN_PICK_GAP,
            })
            .collect();

        let chosen = if eligible.is_empty() {
            // 候选全部刚被用过：LRU 兜底。
            //
            // 必须用「时刻 + 单调序号」双键比较：时钟粒度会让并发写入的
            // last_used 完全相同，单靠时刻比较会因平局恒 false 而永远停在
            // 第一个候选，退化成惊群（Go 侧已修复，此处沿用）。
            let mut best = cands[0];
            for &c in &cands[1..] {
                if self.older_last_used(c, best) {
                    best = c;
                }
            }
            best
        } else {
            self.pick_weighted_mode(&eligible, tiered)
        };

        self.mark_used(chosen);
        Some(self.entries[chosen].a.clone())
    }

    /// 报告 a 是否比 b "更久未被选中"（时刻优先，平局用单调序号）。
    fn older_last_used(&self, a: usize, b: usize) -> bool {
        let ea = &self.entries[a];
        let eb = &self.entries[b];
        match (ea.last_used, eb.last_used) {
            (None, None) => ea.last_used_seq < eb.last_used_seq,
            (None, Some(_)) => true, // 从未使用 = 最久未用
            (Some(_), None) => false,
            (Some(x), Some(y)) => {
                if x != y {
                    x < y
                } else {
                    ea.last_used_seq < eb.last_used_seq
                }
            }
        }
    }

    /// 全冷却兜底：在非禁用的软冷却/熔断账号中选截止最早的一个。
    ///
    /// 分级：disabled 永不参与；硬冷却（余额耗尽）账号若仍处于有效冷却期也排除
    ///（调了必 402）；软冷却与熔断号允许参与。
    fn pick_earliest_expiry(&mut self, tried: &[String], now: SystemTime) -> Option<Auth> {
        let mut best: Option<usize> = None;
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            if tried.iter().any(|t| t == &e.a.uid) {
                continue;
            }
            if e.disabled {
                continue;
            }
            // 余额耗尽号处于有效 hard 冷却期 → 不参与兜底
            if e.cool_kind == Some(CoolKind::Hard)
                && e.until.map(|u| now < u).unwrap_or(false)
            {
                continue;
            }
            if self.in_flight_full(e) {
                continue;
            }
            let exp = e.until.or(e.breaker_until);
            let exp = match exp {
                Some(t) if t > now => t,
                _ => continue,
            };
            if best.is_none() || Some(exp) < best.and_then(|b| self.entries[b].until.or(self.entries[b].breaker_until)) {
                best = Some(i);
            }
        }
        let idx = best?;
        self.mark_used(idx);
        Some(self.entries[idx].a.clone())
    }

    /// 到期分层：返回最早到期那一档，以及是否真的分了档。
    ///
    /// 到期日未知的账号视为最晚（排最后）：只有全部候选都没有到期信息时，
    /// 它们才会成为唯一的一档。
    fn earliest_expiry_tier(&self, cands: Vec<usize>) -> (Vec<usize>, bool) {
        if cands.len() <= 1 {
            return (cands, false);
        }
        // 找出最小的非空 day key（字典序比较即日期先后，因格式固定为 YYYY-MM-DD）
        let mut best: Option<String> = None;
        for &i in &cands {
            let k = self.entries[i].expiry_day_key();
            if k.is_empty() {
                continue; // 到期未知视为最晚，不参与选档
            }
            if best.as_ref().map(|b| &k < b).unwrap_or(true) {
                best = Some(k);
            }
        }
        let Some(best) = best else {
            // 全员无到期信息 → 不分层
            return (cands, false);
        };
        let out: Vec<usize> = cands
            .into_iter()
            .filter(|&i| self.entries[i].expiry_day_key() == best)
            .collect();
        (out, true)
    }

    /// 三因子权重（含 credits 项）；`tiered` 时改用组内权重（去掉 credits）。
    fn weight_of(&self, idx: usize, max_credits: i64, now: SystemTime) -> f64 {
        self.weight_of_mode(idx, max_credits, now, false)
    }

    /// 组内权重：只保留闲置补偿 + 成功率，不含 credits。
    ///
    /// 同一到期档意味着这些额度的「紧迫度相同」，按积分多少分配会让高积分账号
    /// 长期吃掉大部分流量，同档内就不再是平均使用。
    fn tier_weight_of(&self, idx: usize, now: SystemTime) -> f64 {
        self.weight_of_mode(idx, 0, now, true)
    }

    /// 权重计算统一入口。
    fn weight_of_mode(&self, idx: usize, max_credits: i64, now: SystemTime, tiered: bool) -> f64 {
        let e = &self.entries[idx];
        let mut w = 1.0;

        // 1. credits 比例 ×10（tiered 时跳过）
        if !tiered && max_credits > 0 {
            w += e.credits as f64 / max_credits as f64 * 10.0;
        }

        // 2. 闲置补偿
        match e.last_used {
            None => w += self.idle_weight_max,
            Some(lu) => {
                let hours = now
                    .duration_since(lu)
                    .unwrap_or(Duration::ZERO)
                    .as_secs_f64()
                    / 3600.0;
                let mut idle_w = hours * self.idle_weight_per_hour;
                if idle_w > self.idle_weight_max {
                    idle_w = self.idle_weight_max;
                }
                if idle_w < 0.0 {
                    idle_w = 0.0; // 时钟回拨钳 0
                }
                w += idle_w;
            }
        }

        // 3. 成功率 ×3
        let total_req = e.success_count + e.err_total;
        if total_req > 0 {
            w += e.success_count as f64 / total_req as f64 * 3.0;
        } else {
            w += 1.5; // 无请求记录 → 中性偏信任
        }
        w
    }

    /// 加权随机抽签（定点 ×1e6 累加，与 Go 侧同口径）。
    fn pick_weighted_mode(&self, cands: &[usize], tiered: bool) -> usize {
        let now = SystemTime::now();
        let max_credits = cands
            .iter()
            .map(|&i| self.entries[i].credits)
            .max()
            .unwrap_or(0);

        const SCALE: f64 = 1_000_000.0;
        let weights: Vec<i64> = cands
            .iter()
            .map(|&i| {
                let w = if tiered {
                    self.tier_weight_of(i, now)
                } else {
                    self.weight_of(i, max_credits, now)
                };
                (w * SCALE) as i64
            })
            .collect();
        let total: i64 = weights.iter().sum();

        if total <= 0 {
            let n = cands.len() as i64;
            return cands[(self.rand_n(n) as usize).min(cands.len() - 1)];
        }
        let r = self.rand_n(total);
        let mut acc: i64 = 0;
        for (k, &i) in cands.iter().enumerate() {
            acc += weights[k];
            if r < acc {
                return i;
            }
        }
        cands[cands.len() - 1]
    }

    /// 记录一次选中使用（时刻 + 单调序号）。
    fn mark_used(&mut self, idx: usize) {
        self.pick_seq += 1;
        let e = &mut self.entries[idx];
        e.last_used_seq = self.pick_seq;
        e.last_used = Some(SystemTime::now());
    }
}

/// 防并发撞号窗口：同一账号在该窗口内不重复被选中（除非候选全部刚被用过）。
const MIN_PICK_GAP: Duration = Duration::from_millis(100);

/// 计算从 `now` 到下一个本地 04:00 的时长。
///
/// 与 Go 侧 `nextDay4AM` 一致：now 在当天 04:00 之前返回当天 04:00
///（此时签到尚未执行，等当天签到即可）；04:00 及之后返回次日 04:00。
fn next_day_4am_duration(now: chrono::DateTime<chrono::Local>) -> Duration {
    use chrono::{Datelike, Local, TimeZone, Timelike};

    // 用 date() 取当天 00:00，再 +1 天 +4 小时，天然覆盖月末/年末进位
    //（等价于 Go 侧 time.Date 对日溢出的自动进位）。
    let today4am = Local
        .with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0)
        .single()
        .unwrap_or_else(|| Local.with_ymd_and_hms(now.year(), now.month(), now.day(), 4, 0, 0).earliest()
            .unwrap_or_else(|| now));

    // now 在当天 04:00 之前 → 当天 04:00；否则次日 04:00。
    let target = if now.hour() < 4 {
        today4am
    } else {
        today4am + chrono::Duration::days(1)
    };

    let secs = target.signed_duration_since(now).num_seconds().max(0) as u64;
    Duration::from_secs(secs)
}

/// 进程级随机种子（一次性）。
fn seed_once() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 混合时间、计数器与栈地址，避免多线程同种子
    base ^ (n.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ (Instant::now().elapsed().as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(uid: &str) -> Auth {
        Auth {
            uid: uid.into(),
            access_token: format!("at-{uid}"),
            ..Default::default()
        }
    }

    #[test]
    fn add_and_pick() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.add(mk("u2"));
        let a = p.pick().unwrap();
        assert!(a.uid == "u1" || a.uid == "u2");
    }

    #[test]
    fn cooldown_makes_unhealthy_then_recovers() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(p.servable_now());
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(50), "429");
        assert!(!p.servable_now(), "冷却期内应不可选");
        std::thread::sleep(Duration::from_millis(80));
        assert!(p.servable_now(), "冷却到期后应恢复");
    }

    #[test]
    fn disable_is_permanent() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.disable("u1", "session dead");
        std::thread::sleep(Duration::from_millis(10));
        assert!(!p.servable_now());
        assert_eq!(p.list()[0].disabled, true);
    }

    #[test]
    fn breaker_trips_after_threshold() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(60), Duration::from_secs(3600));
        p.note_error("u1");
        assert!(p.servable_now(), "未达阈值不应熔断");
        p.note_error("u1");
        assert!(p.servable_now());
        p.note_error("u1");
        assert!(!p.servable_now(), "达到阈值(3)应熔断");
    }

    #[test]
    fn note_success_clears_breaker() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(2, Duration::from_secs(60), Duration::from_secs(3600));
        p.note_error("u1");
        p.note_error("u1");
        assert!(!p.servable_now());
        p.note_success("u1");
        assert!(p.servable_now(), "成功应清熔断");
    }

    #[test]
    fn in_flight_limits() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_max_in_flight(2);
        assert!(p.acquire("u1"));
        assert!(p.acquire("u1"));
        assert!(!p.acquire("u1"), "超过上限应拒绝");
        assert!(!p.servable_now(), "占满后不可受理");
        p.release("u1");
        assert!(p.servable_now());
    }

    #[test]
    fn pick_excluding_skips_tried() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        let a = p.pick_excluding(&["u1".to_string()]);
        assert!(a.is_none(), "唯一账号被排除后应无候选（且无冷却兜底）");
    }

    #[test]
    fn pick_by_uid_respects_health() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        assert!(p.pick_by_uid("u1").is_some());
        assert!(p.pick_by_uid("nope").is_none());
        p.cooldown("u1", CoolKind::Soft, Duration::from_secs(60), "x");
        assert!(p.pick_by_uid("u1").is_none());
    }

    #[test]
    fn anti_thundering_herd_uses_seq_tiebreak() {
        // 复刻 Go 侧 TestPickAntiThunderingHerd 的核心断言：
        // 大量连续 Pick 不应全部落在同一账号上。
        // （Go 版的并发版依赖 goroutine；这里用串行 Pick 验证 LRU 兜底能发散，
        //   因为串行时每个账号都会进入 minPickGap 窗口，必然走 LRU 分支。）
        let mut p = Pool::new(String::new());
        for i in 0..10 {
            p.add(mk(&format!("c{i:02}")));
        }
        let mut counts: HashMap<String, i64> = HashMap::new();
        const N: i64 = 100;
        for _ in 0..N {
            if let Some(a) = p.pick() {
                *counts.entry(a.uid.clone()).or_insert(0) += 1;
            }
        }
        assert!(counts.len() >= 2, "应覆盖多个账号: {counts:?}");
        for (uid, n) in &counts {
            assert!(*n <= N / 2, "惊群：{uid} 被选中 {n}/{N} 次");
        }
    }

    #[test]
    fn expiry_tier_prefers_earliest() {
        let mut p = Pool::new(String::new());
        let mut late = mk("late");
        late.soonest_expire_at = 2000000000; // 远未来
        let mut early = mk("early");
        early.soonest_expire_at = 1000000000; // 更早
        p.add(late);
        p.add(early);

        // 分层后只有最早档入选
        let mut counts: HashMap<String, i64> = HashMap::new();
        for _ in 0..50 {
            if let Some(a) = p.pick() {
                *counts.entry(a.uid.clone()).or_insert(0) += 1;
            }
        }
        assert_eq!(counts.get("early").copied().unwrap_or(0), 50, "应固定选最早到期档: {counts:?}");
        assert!(!counts.contains_key("late"), "较晚档不应被选中: {counts:?}");
    }

    #[test]
    fn counts_and_available_uids() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.add(mk("u2"));
        p.cooldown("u2", CoolKind::Soft, Duration::from_secs(60), "x");
        let (total, healthy, cooling, disabled, full) = p.counts_detailed();
        assert_eq!(total, 2);
        assert_eq!(healthy, 1);
        assert_eq!(cooling, 1);
        assert_eq!(disabled, 0);
        assert_eq!(full, 0);
        assert_eq!(p.available_uids(), vec!["u1".to_string()]);
    }

    /// 冷却入口也是熔断器的失败信号（对齐 Go `Cooldown → recordBreakerFailureLocked`）。
    ///
    /// 回归防护：Go 版把 `fails++` 放在 `recordBreakerFailureLocked` 内部，
    /// 因此冷却必须能触发熔断；若把自增搬到调用方，冷却就永远喂不进熔断器。
    #[test]
    fn cooldown_feeds_breaker() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(3600), Duration::from_secs(3600));
        // 连续 3 次软冷却（429）应触发熔断，即使从未调用 note_error
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        // 冷却本身极短，此时是否可选取决于熔断是否触发
        std::thread::sleep(Duration::from_millis(20));
        assert!(p.servable_now(), "未达阈值不应熔断");
        p.cooldown("u1", CoolKind::Soft, Duration::from_millis(1), "429");
        std::thread::sleep(Duration::from_millis(20));
        assert!(!p.servable_now(), "3 次冷却应触发熔断（冷却本身已过期）");
    }

    #[test]
    fn breaker_threshold_is_exclusive_of_extra_increment() {
        // 阈值 3：第 1、2 次失败不熔断，第 3 次熔断（不多不少）
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.set_breaker(3, Duration::from_secs(3600), Duration::from_secs(3600));
        p.note_error("u1");
        p.note_error("u1");
        assert!(p.servable_now(), "2 次失败不应熔断");
        p.note_error("u1");
        assert!(!p.servable_now(), "3 次失败应熔断");
        assert_eq!(p.list()[0].breaker_fails, 0, "触发后 fails 应清零");
    }

    /// 回归防护：Go 的零值 time.Time 序列化为 `"0001-01-01T00:00:00Z"`，
    /// 直接交给 SystemTime 会因早于 UNIX_EPOCH 而让**整个 state.json 解析失败**
    ///（实测表现为 credits 全为 0、冷却/统计全丢）。
    #[test]
    fn go_zero_time_parses_as_none() {
        let raw = br#"{
          "accounts": {
            "u1": {
              "credits": 4360,
              "disabled": false,
              "until": "0001-01-01T00:00:00Z",
              "cool_kind": 0,
              "success_count": 922,
              "last_success": "2026-09-12T05:51:44.1541013+08:00",
              "last_err": "0001-01-01T00:00:00Z",
              "expire_at": 1790636739
            }
          }
        }"#;
        let sf: StateFile = serde_json::from_slice(raw).expect("Go 状态文件必须可解析");
        let a = sf.accounts.get("u1").unwrap();
        assert_eq!(a.credits, 4360);
        assert_eq!(a.success_count, 922);
        assert_eq!(a.expire_at, 1790636739);
        // 零值时间 → None（= 不在冷却期）
        assert!(a.until.is_none(), "零值 until 应为 None");
        assert!(a.last_err.is_none(), "零值 last_err 应为 None");
        // 有效时间应正常解析
        assert!(a.last_success.is_some(), "有效 last_success 应解析成功");
    }

    #[test]
    fn load_state_restores_credits_and_survives_add() {
        let dir = std::env::temp_dir().join(format!("wbswg-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");
        std::fs::write(
            &fp,
            br#"{"accounts":{"u1":{"credits":4360,"success_count":922,"expire_at":1790636739,
                 "until":"0001-01-01T00:00:00Z","last_err":"0001-01-01T00:00:00Z"}}}"#,
        )
        .unwrap();

        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.load_state();
        assert!(p.loaded_from_state(), "应报告已从 state.json 恢复");

        // 只载入 state 时是占位凭证，credits 已恢复
        let st = &p.list()[0];
        assert_eq!(st.credits, 4360);
        assert_eq!(st.success_count, 922);
        assert!(!st.cooling, "零值 until 不应视为冷却");

        // add() 灌入完整凭证后，运行态必须保留（credits/统计不丢）
        p.add(Auth {
            uid: "u1".into(),
            access_token: "at".into(),
            nickname: "猫猫".into(),
            soonest_expire_at: 1790636739,
            ..Default::default()
        });
        let st = &p.list()[0];
        assert_eq!(st.credits, 4360, "add() 不应清掉已恢复的 credits");
        assert_eq!(st.success_count, 922, "add() 不应清掉已恢复的统计");
        assert_eq!(st.nickname, "猫猫", "add() 应换入完整凭证");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_only_writes_when_dirty() {
        let dir = std::env::temp_dir().join(format!("wbswg-flush-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");

        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.add(mk("u1"));
        // 刚 add，未标记 dirty → 不写
        assert!(!p.flush(), "非 dirty 不应写盘");

        p.set_credits("u1", 100);
        assert!(p.flush(), "dirty 后应写盘");
        assert!(fp.exists());

        // 写回内容可被自己读回
        let mut p2 = Pool::new(fp.to_string_lossy().to_string());
        p2.load_state();
        assert_eq!(p2.list()[0].credits, 100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn err_count_legacy_field_migrates_to_err_total() {
        // 旧文件只有 err_count → 迁移进 err_total（取较大者）
        let raw = br#"{"accounts":{"u1":{"credits":1,"err_count":7}}}"#;
        let sf: StateFile = serde_json::from_slice(raw).unwrap();
        assert_eq!(sf.accounts["u1"].err_count, 7);
        assert_eq!(sf.accounts["u1"].err_total, 0);

        let dir = std::env::temp_dir().join(format!("wbswg-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let fp = dir.join("state.json");
        std::fs::write(&fp, raw).unwrap();
        let mut p = Pool::new(fp.to_string_lossy().to_string());
        p.load_state();
        assert_eq!(p.list()[0].err_total, 7, "err_count 应迁移为 err_total");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reenable_if_credits_revives_cooling() {
        let mut p = Pool::new(String::new());
        p.add(mk("u1"));
        p.cooldown("u1", CoolKind::Hard, Duration::from_secs(3600), "余额不足");
        assert!(!p.servable_now());
        p.reenable_if_credits("u1", 100);
        assert!(p.servable_now(), "签到有余额应解冻");
        // 余额为 0 则不解冻
        p.cooldown("u1", CoolKind::Hard, Duration::from_secs(3600), "余额不足");
        p.reenable_if_credits("u1", 0);
        assert!(!p.servable_now());
    }
}
