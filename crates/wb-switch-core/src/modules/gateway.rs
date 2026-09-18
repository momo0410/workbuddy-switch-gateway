//! 网关（workbuddy2api）托管与账号桥接。
//!
//! 设计：把编译好的 gateway 可执行文件作为**子进程**托管，并复用本机已有的
//! 账号库（`~/.wb-switch/accounts.json`）为其生成凭证目录，使账号管理页与
//! OpenAI 兼容网关共享同一批账号。
//!
//! 为什么用子进程而不是把网关逻辑用 Rust 重写：
//!   网关包含账号池三因子加权重、熔断指数退避、会话粘性、SSE 逐帧规范化、
//!   指纹脱敏等大量经过测试的逻辑，重写既无必要也会引入行为差异。
//!   子进程方式保留其全部行为，且崩溃可独立重启。
//!
//! 账号同步沿用 workbuddy-switch 侧的职责划分：
//!   - 本模块只做「App 账号库 -> 网关凭证目录」的生成与更新；
//!   - 网关自身刷新 token 后写回的是它自己的 auths/，下次同步时会按
//!     expiresAt 较新者胜出的规则合并回来（见 `sync_auth_to_accounts`）。

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::modules::account;
use crate::modules::config::{atomic_write, now_ms, store_dir};

// ---------------------------------------------------------------------------
// 路径与配置
// ---------------------------------------------------------------------------

/// 网关配置文件名（放在 ~/.wb-switch/ 下，与账号库同目录）。
const GATEWAY_CONFIG: &str = "gateway_config.json";

/// 上游网关在 /healthz 透出的身份标识（响应头 X-Service + 响应体 service 字段）。
///
/// 用途：宿主（本项目）托管网关子进程时，用它识别「同端口上的另一个服务」
/// 冒充应答造成的假启动成功。
const GATEWAY_SERVICE_NAME: &str = "workbuddy2api";
/// 网关凭证目录名（auths/）。
const GATEWAY_AUTH_DIR: &str = "gateway_auths";
/// 网关状态文件名（账号池冷却/熔断状态持久化）。
const GATEWAY_STATE_DIR: &str = "gateway_data";

/// 网关运行目录（~/.wb-switch/gateway）。
pub fn gateway_dir() -> PathBuf {
    store_dir().join("gateway")
}

/// 网关凭证目录。
pub fn gateway_auth_dir() -> PathBuf {
    gateway_dir().join(GATEWAY_AUTH_DIR)
}

/// 网关配置文件路径。
pub fn gateway_config_file() -> PathBuf {
    gateway_dir().join(GATEWAY_CONFIG)
}

/// 网关状态文件路径。
pub fn gateway_state_file() -> PathBuf {
    gateway_dir().join(GATEWAY_STATE_DIR).join("state.json")
}

/// 可执行文件名（Windows 带 .exe 后缀，macOS/Linux 不带）。
fn gateway_exe_name() -> &'static str {
    if cfg!(windows) {
        "gateway.exe"
    } else {
        "gateway"
    }
}

/// 定位网关可执行文件。
///
/// 优先顺序：
///   1. 环境变量 WB_SWITCH_GATEWAY_BIN（显式指定，便于开发/替换）
///   2. 内嵌版本（单文件分发：解压到 ~/.wb-switch/gateway/bin/）
///   3. 与本程序同目录的 gateway.exe（自定义/调试用）
///   4. ~/.wb-switch/gateway/ 下的 gateway.exe
///
/// 内嵌版本优先于「同目录文件」，保证单文件分发场景下运行的是
/// 与主程序同版本、同源码构建的网关，不会误用同目录里遗留的旧文件。
pub fn resolve_gateway_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("WB_SWITCH_GATEWAY_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }

    if crate::modules::gateway_embed::has_embedded() {
        match crate::modules::gateway_embed::materialize() {
            Ok(path) => {
                crate::modules::gateway_embed::cleanup_old();
                return Some(path);
            }
            Err(e) => {
                // 解压失败不应让网关功能静默消失：退回外部文件路径再试
                eprintln!("[gateway] 释放内嵌网关失败: {e}，尝试使用外部文件");
            }
        }
    }

    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            for cand in [dir.join(gateway_exe_name()), dir.join("gateway").join(gateway_exe_name())] {
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    let in_store = gateway_dir().join(gateway_exe_name());
    if in_store.is_file() {
        return Some(in_store);
    }
    None
}

/// 当前网关可执行文件的来源描述（供前端诊断展示）。
pub fn gateway_source() -> &'static str {
    if std::env::var("WB_SWITCH_GATEWAY_BIN").is_ok_and(|p| PathBuf::from(p).is_file()) {
        return "env";
    }
    if crate::modules::gateway_embed::has_embedded() {
        return "embedded";
    }
    "external"
}

/// 默认网关配置（首次启动时落盘，之后由用户在设置页修改）。
pub fn default_gateway_config() -> Value {
    json!({
        "enabled": false,
        "port": 7863,
        "listen": ":7863",
        "api_key": "",
        "auto_start": false,
        // 网关工作模式：
        //   "balance" —— 负载均衡：账号池按三因子加权随机选号（默认）
        //   "pinned"  —— 指定账号：只使用 pinned_uid 对应的那一个账号
        "mode": "balance",
        "pinned_uid": null,
        "last_status": null,
        "last_error": null,
    })
}

/// 把 `patch` 合并到 `base` 之上（浅合并，只覆盖出现的键）。
fn overlay(base: Value, patch: &Value) -> Value {
    let mut out = base;
    if let Some(map) = patch.as_object() {
        if !out.is_object() {
            out = json!({});
        }
        let target = out.as_object_mut().unwrap();
        for (k, v) in map {
            target.insert(k.clone(), v.clone());
        }
    }
    out
}

/// 补齐缺省字段并规范化。
fn finalize_gateway_config(mut cfg: Value) -> Value {
    // port 与 listen 保持一致：以 port 为权威字段（前端只让用户填端口数字）。
    // 兼容老配置里只有 listen 的情况。
    let port = cfg
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .filter(|p| *p > 0)
        .unwrap_or_else(|| {
            let s = cfg.get("listen").and_then(Value::as_str).unwrap_or("");
            if s.trim().is_empty() {
                7863
            } else {
                port_of(s)
            }
        });
    cfg["port"] = json!(port);
    cfg["listen"] = json!(normalize_listen(port));
    cfg
}

/// 读取磁盘上已有的配置（不补默认值）；文件不存在或损坏时返回 None。
fn read_existing_config() -> Option<Value> {
    let text = std::fs::read_to_string(gateway_config_file()).ok()?;
    serde_json::from_str::<Value>(&text).ok().filter(Value::is_object)
}

/// 合并配置：以「默认值 < 磁盘现有配置 < 传入 patch」的顺序覆盖。
///
/// 关键点：必须以**磁盘现有配置**为基准，而不是只用默认值。
/// 否则像「启动后回写 last_status」这类局部更新会把用户设置的
/// listen / api_key 一起重置为默认值，导致界面与网关实际配置不一致
///（曾经因此让账号池查询带上空 api_key，前端显示异常）。
fn merge_gateway_config(input: &Value) -> Value {
    let base = match read_existing_config() {
        Some(existing) => overlay(default_gateway_config(), &existing),
        None => default_gateway_config(),
    };
    finalize_gateway_config(overlay(base, input))
}

/// 读取网关配置（缺字段自动补默认值）。
pub fn load_gateway_config() -> Value {
    let f = gateway_config_file();
    if f.exists() {
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                return merge_gateway_config(&v);
            }
        }
    }
    default_gateway_config()
}

/// 只更新运行态字段（last_status / last_error），不触碰用户配置。
///
/// 启动/停止流程用它记录结果；避免把监听地址、api_key 等设置写坏。
pub fn update_runtime_state(last_status: &str, last_error: Option<String>) {
    let patch = json!({
        "last_status": last_status,
        "last_error": last_error,
    });
    let _ = save_gateway_config(&patch);
}

/// 保存网关配置；未提供的字段沿用磁盘上的现有值。
pub fn save_gateway_config(cfg: &Value) -> Result<Value, String> {
    let merged = merge_gateway_config(cfg);
    std::fs::create_dir_all(gateway_dir()).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    atomic_write(&gateway_config_file(), &text).map_err(|e| e.to_string())?;
    Ok(merged)
}

// ---------------------------------------------------------------------------
// 账号桥接：账号库 -> 网关凭证
// ---------------------------------------------------------------------------

/// 把毫秒时间戳转成网关要求的「秒」。
fn to_sec(ms: i64) -> i64 {
    if ms <= 0 {
        0
    } else if ms > 1_000_000_000_000 {
        ms / 1000
    } else {
        ms
    }
}

/// 为单个账号生成网关凭证 JSON（嵌套形，与 internal/auth.Parse 对齐）。
///
/// `credit` 参数是上一次导出/网关回写留下的积分到期元数据，原样透传。
/// 必须保留：它由网关的积分巡检写入，是「按到期紧迫度分层选号」的依据；
/// 若这里丢掉，每次账号同步都会把依据抹掉一次（表现为分层均衡时灵时不灵）。
fn build_auth_doc(acc: &Value, credit: Option<Value>) -> Option<(String, String)> {
    let uid = account::get_str(acc, "uid")?;
    let access = account::get_str(acc, "access_token")?;
    let refresh = account::get_str(acc, "refresh_token").unwrap_or_default();
    let nickname = account::get_str(acc, "nickname").unwrap_or_default();
    let enterprise = account::get_str(acc, "enterpriseId").unwrap_or_default();
    let domain = account::get_str(acc, "domain").unwrap_or_default();
    let expires_ms = acc.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);

    let mut doc = json!({
        "auth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": to_sec(expires_ms),
            "domain": domain,
        },
        "account": {
            "uid": uid,
            "enterpriseId": enterprise,
            "nickname": nickname,
        },
    });
    if let Some(c) = credit {
        if c.is_object() {
            doc["credit"] = c;
        }
    }
    let file = format!("workbuddy-{uid}.json");
    Some((file, serde_json::to_string_pretty(&doc).ok()?))
}

/// 读取凭证文件里已有的 credit 块（网关积分巡检写入）；无则返回 None。
fn read_credit_block(path: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc.get("credit").filter(|v| v.is_object()).cloned()
}

/// 计算账号库的内容指纹：只要 uid + token + 过期时间有变化就算「脏」。
///
/// 用它做增量判断，避免每分钟无条件重写整个凭证目录
/// （写盘 + 触发网关无谓重启都会有代价）。
pub fn accounts_fingerprint() -> String {
    let accounts = account::load_accounts();
    let mut parts: Vec<String> = accounts
        .iter()
        .filter_map(|a| {
            let uid = account::get_str(a, "uid")?;
            let at = account::get_str(a, "access_token").unwrap_or_default();
            let rt = account::get_str(a, "refresh_token").unwrap_or_default();
            let exp = a.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
            // token 只取尾部若干字符参与指纹：避免把完整凭证写进日志/内存字符串
            let at_tail: String = at.chars().rev().take(12).collect();
            let rt_len = rt.len();
            // 需重登标记必须参与指纹：它的变化会改变「哪些账号该导出」，
            // 若不计入，标记翻转后 `sync_if_changed` 会认为无事发生而不重推凭证。
            let relogin = if needs_relogin(a) { 1 } else { 0 };
            Some(format!(
                "{uid}|{at_tail}|{rt_len}|{}|{relogin}",
                to_sec(exp)
            ))
        })
        .collect();
    parts.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for p in &parts {
        for b in p.as_bytes() {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    format!("{}-{:x}", parts.len(), hash)
}

/// 上次成功推送时的账号库指纹。
static LAST_FINGERPRINT: Mutex<Option<String>> = Mutex::new(None);

/// 仅当账号库有变化时才推送到网关（供后台定时任务调用）。
///
/// 返回是否发生了变化。这样新增账号能自动进入网关凭证目录，
/// 用户不必手动点「立即同步」。
pub fn sync_if_changed() -> bool {
    let fp = accounts_fingerprint();
    {
        let guard = LAST_FINGERPRINT.lock().unwrap();
        if guard.as_deref() == Some(fp.as_str()) {
            return false;
        }
    }
    match export_accounts_to_gateway() {
        Ok(_) => {
            *LAST_FINGERPRINT.lock().unwrap() = Some(fp);
            true
        }
        Err(e) => {
            eprintln!("[gateway] 自动同步账号失败: {e}");
            false
        }
    }
}

/// 后台自动同步：账号库变化 → 推送凭证；网关运行中且账号有变动 → 重启使新账号生效。
///
/// 由 GUI / server 的启动流程调用，永续运行。
pub async fn run_auto_sync_loop(interval_secs: u64) {
    let interval = std::time::Duration::from_secs(interval_secs.max(5));
    // 启动先同步一次，保证首屏即是最新
    if sync_if_changed() && is_running() {
        let cfg = load_gateway_config();
        stop_gateway();
        let _ = start_gateway(&cfg).await;
    }
    loop {
        tokio::time::sleep(interval).await;
        if !sync_if_changed() {
            continue;
        }
        // 账号库有变化：网关在跑才需要重启才能加载新凭证
        if is_running() {
            let cfg = load_gateway_config();
            stop_gateway();
            match start_gateway(&cfg).await {
                Ok(_) => eprintln!("[gateway] 检测到账号变化，已自动重启网关以加载新账号"),
                Err(e) => eprintln!("[gateway] 账号变化后重启网关失败: {e}"),
            }
        }
    }
}

/// 从候选 uid 中筛出「应当导出到网关」的集合。
///
/// `only` 为 None 表示不筛选（负载均衡，全部导出）；
/// 为 Some(set) 表示只保留 set 内的（指定账号）。
///
/// 抽成纯函数是为了让「导出」与「清理」共用同一判定 —— 二者一旦分叉，
/// 切换模式时就会出现残留凭证（网关仍把旧账号加载进池，表现为切换无效）。
fn select_export_uids(candidates: &[String], only: &Option<Vec<String>>) -> Vec<String> {
    match only {
        None => candidates.to_vec(),
        Some(allowed) => candidates
            .iter()
            .filter(|u| allowed.iter().any(|a| a == *u))
            .cloned()
            .collect(),
    }
}

/// 网关工作模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayMode {
    /// 负载均衡：账号池加权随机选号，自动避开冷却/熔断的账号。
    Balance,
    /// 指定账号：只使用 pinned_uid 对应的账号。
    Pinned,
    /// 单一模型 + 积分轮转：只用一个账号烧到不可用，再换按到期日排序的下一个。
    ///
    /// 与 Pinned 的关键区别：**仍然导出全部账号** —— 轮转需要「下一个」作为备选，
    /// 只导出一个是转不起来的。区别在网关侧的选择策略（pool.rotation），
    /// 而不在凭证范围。
    Rotation,
}

impl GatewayMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            GatewayMode::Balance => "balance",
            GatewayMode::Pinned => "pinned",
            GatewayMode::Rotation => "rotation",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "pinned" | "pin" | "single" => GatewayMode::Pinned,
            // 兼容几种自然叫法：轮转 / 单一模型轮转
            "rotation" | "rotate" | "rolling" => GatewayMode::Rotation,
            _ => GatewayMode::Balance,
        }
    }
}

/// 读取当前网关模式。
pub fn gateway_mode() -> GatewayMode {
    GatewayMode::from_str(&load_gateway_config()
        .get("mode").and_then(Value::as_str).unwrap_or("balance"))
}

/// 读取「指定账号」模式锁定的 uid。
pub fn pinned_uid() -> Option<String> {
    let cfg = load_gateway_config();
    // 注意：必须先把配置绑定到变量，否则临时值在语句结束即被释放（E0716）
    let s = cfg.get("pinned_uid")?.as_str()?.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// 当前实际参与网关的账号 uid 集合。
///
/// 负载均衡 / 轮转：全部账号；指定账号：仅 pinned_uid。
/// 网关依据 `auths/` 目录里的凭证文件建立账号池，
/// 因此「只导出目标账号」即可实现指定账号，同时保留熔断/冷却/粘性等能力。
///
/// 轮转模式刻意**不过滤**：它的「换下一个」依赖备选账号都在池里。
fn active_uids() -> Option<Vec<String>> {
    match gateway_mode() {
        // None = 不过滤，全部导出
        GatewayMode::Balance | GatewayMode::Rotation => None,
        GatewayMode::Pinned => Some(pinned_uid().into_iter().collect()),
    }
}

/// 网关是否应启用「单一模型 + 积分轮转」选号策略（写入 native config 的 pool.rotation）。
pub fn rotation_enabled() -> bool {
    matches!(gateway_mode(), GatewayMode::Rotation)
}

/// 账号是否已被标记为「需重新登录」。
///
/// 复用 `refresh::needs_relogin`：它会排除旧版本因传输层失败留下的误报标记
///（`code=-1` 并不代表凭证失效），避免这些账号被永久排除出网关池。
pub use crate::modules::refresh::needs_relogin;

/// 账号库 -> 网关凭证目录。返回 (账号数, 有变化的 uid 列表)。
pub fn export_accounts_to_gateway() -> Result<(usize, Vec<String>), String> {
    export_accounts_to_dir(&gateway_auth_dir(), &account::load_accounts(), active_uids())
}

/// `export_accounts_to_gateway` 的可注入目录版本（便于单测验证真实文件增删）。
///
/// `only` 语义见 `select_export_uids`：None=不过滤（负载均衡），Some=仅这些 uid。
pub fn export_accounts_to_dir(
    dir: &std::path::Path,
    accounts: &[Value],
    only: Option<Vec<String>>,
) -> Result<(usize, Vec<String>), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut changed = Vec::new();
    let mut written = 0usize;

    // 「是否导出某账号」只在这里定义一次，导出与清理共用。
    // 分开写会导致切换模式时判定不一致（曾被此坑到：清理用账号库全集，
    // 从负载均衡切到指定账号后其余凭证残留，网关仍把它们加载进池）。
    //
    // 需重登的账号同样不导出：它的 refresh token 已被服务端拒绝，
    // 留在池里只会让每次请求白跑一轮（实测：网关无视该标记持续使用失效账号）。
    // 重新登录成功后 `needs_relogin` 被清除，下次同步会自动把它放回池中。
    let should_export = |acc: &Value| -> bool {
        if needs_relogin(acc) {
            return false;
        }
        let uid = account::get_str(acc, "uid").unwrap_or_default();
        !select_export_uids(&[uid], &only).is_empty()
    };

    for acc in accounts {
        if !should_export(acc) {
            continue;
        }
        let Some((file, _)) = build_auth_doc(acc, None) else { continue };
        let path = dir.join(&file);
        // 保留网关写入的 credit 元数据（积分到期分层选号的依据）。
        let credit = read_credit_block(&path);
        let Some((_, text)) = build_auth_doc(acc, credit) else { continue };
        // 内容一致则不写盘，避免无意义的文件时间戳变动
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if existing.trim() == text.trim() {
                written += 1;
                continue;
            }
        }
        atomic_write(&path, &text).map_err(|e| e.to_string())?;
        if let Some(uid) = account::get_str(acc, "uid") {
            changed.push(uid);
        }
        written += 1;
    }

    // 清理不再需要的凭证：账号库中已删除的、因模式切换而不再导出的，
    // 以及已被标记需重新登录的（留着只会让网关持续调用失效凭证）。
    let all_uids: Vec<String> = accounts
        .iter()
        .filter(|a| !needs_relogin(a))
        .filter_map(|a| account::get_str(a, "uid"))
        .collect();
    let live: Vec<String> = select_export_uids(&all_uids, &only)
        .iter()
        .map(|u| format!("workbuddy-{u}.json"))
        .collect();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for ent in entries.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with("workbuddy") && name.ends_with(".json") && !live.contains(&name) {
                if std::fs::remove_file(ent.path()).is_ok() {
                    changed.push(format!("removed:{name}"));
                }
            }
        }
    }
    Ok((written, changed))
}

/// 网关凭证 -> 账号库：把网关刷新后的 token 回写账号库。
///
/// 规则：仅当网关侧 expiresAt（秒）比账号库侧（毫秒）更新时才回写；
/// 空 refresh_token 不覆盖已有值。返回回写的 uid 列表。
pub fn sync_auth_to_accounts() -> Result<Vec<String>, String> {
    let dir = gateway_auth_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut accounts = account::load_accounts();
    let mut updated = Vec::new();

    for ent in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(doc) = serde_json::from_str::<Value>(&text) else { continue };
        // 兼容嵌套形与扁平形
        let (auth, acct) = match (doc.get("auth"), doc.get("account")) {
            (Some(a), Some(b)) => (a.clone(), b.clone()),
            _ => (doc.clone(), doc.clone()),
        };
        let Some(uid) = account::get_str(&acct, "uid") else { continue };
        let Some(gw_at) = account::get_str(&auth, "accessToken") else { continue };
        let gw_exp_ms = auth.get("expiresAt").and_then(Value::as_i64).map(|s| {
            if s > 0 && s < 1_000_000_000_000 { s * 1000 } else { s }
        }).unwrap_or(0);

        let Some(idx) = accounts.iter().position(|a| account::get_str(a, "uid").as_deref() == Some(uid.as_str()))
        else {
            continue; // 网关侧新账号由「导入」流程显式处理，不静默注入
        };

        let cur_exp_ms = accounts[idx].get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
        let cur_at = account::get_str(&accounts[idx], "access_token").unwrap_or_default();

        // 以秒为单位比较，避免 ms/s 精度差导致反复回写
        if to_sec(gw_exp_ms) <= to_sec(cur_exp_ms) && gw_at == cur_at {
            continue;
        }
        if to_sec(gw_exp_ms) < to_sec(cur_exp_ms) {
            continue; // 账号库更新，保持本地
        }

        accounts[idx]["access_token"] = json!(gw_at);
        if let Some(rt) = account::get_str(&auth, "refreshToken") {
            if !rt.is_empty() {
                accounts[idx]["refresh_token"] = json!(rt);
            }
        }
        if gw_exp_ms > 0 {
            accounts[idx]["expiresAt"] = json!(gw_exp_ms);
        }
        if let Some(d) = account::get_str(&auth, "domain") {
            if !d.is_empty() {
                accounts[idx]["domain"] = json!(d);
            }
        }
        accounts[idx]["refreshedAt"] = json!(now_ms());
        updated.push(uid);
    }

    if !updated.is_empty() {
        account::save_accounts(&accounts).map_err(|e| e.to_string())?;
    }
    Ok(updated)
}
// ---------------------------------------------------------------------------
// 进程托管
// ---------------------------------------------------------------------------

static GATEWAY_PROC: OnceLock<Mutex<Option<GatewayChild>>> = OnceLock::new();
static GATEWAY_RUNNING: AtomicBool = AtomicBool::new(false);

fn proc_slot() -> &'static Mutex<Option<GatewayChild>> {
    GATEWAY_PROC.get_or_init(|| Mutex::new(None))
}

/// 正在托管的网关进程：子进程句柄 + 它所属的 Job（Windows）。
///
/// 两者必须**一起**持有。Job 句柄一旦关闭，`KILL_ON_JOB_CLOSE` 会立刻终止
/// 网关，所以句柄的生命周期必须与「网关正在运行」严格一致：放在同一个槽里，
/// 由 `start_gateway` 装入、`stop_gateway` 取出，进程异常结束时由系统统一关闭。
struct GatewayChild {
    child: Child,
    /// 每个网关进程一个 Job，随进程一起创建、一起销毁。
    #[cfg(windows)]
    job: Option<GatewayJob>,
}

// ---------------------------------------------------------------------------
// 孤儿进程防护：Windows Job Object
// ---------------------------------------------------------------------------

/// 网关子进程所属的 Job Object 包装。
///
/// 只靠 `stop_gateway()` 无法覆盖应用异常结束的路径（崩溃、任务管理器强杀、
/// 更新重启时的 `std::process::exit`）。这些情况下 Windows 不会回收子进程，
/// gateway.exe 会变成孤儿继续占用端口。Job 设置 `KILL_ON_JOB_CLOSE` 后，
/// 父进程无论以何种方式退出，系统在关闭句柄时都会连带终止 Job 内的子进程。
///
/// 句柄在网关存活期间一直持有；句柄关闭（含进程崩溃导致的系统自动关闭）
/// 即代表杀死网关。
#[cfg(windows)]
struct GatewayJob(windows::Win32::Foundation::HANDLE);

// 句柄是进程级资源，仅通过 Win32 API 使用，跨线程共享安全。
#[cfg(windows)]
unsafe impl Send for GatewayJob {}
#[cfg(windows)]
unsafe impl Sync for GatewayJob {}

#[cfg(windows)]
impl GatewayJob {
    /// 创建带 KILL_ON_JOB_CLOSE 限制的 Job；失败返回 None（不影响网关本身运行）。
    fn create() -> Option<Self> {
        use windows::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let handle = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .is_err()
            {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }

    /// 把网关子进程收进 Job。
    fn assign(&self, child: &Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        let process = HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(self.0, process).is_ok() }
    }

    /// 终止 Job 内所有进程（含网关自己拉起的子进程树）。
    ///
    /// 这是 `stop_gateway` 的首选路径：纯 API 调用，不创建任何进程。
    /// 曾经的实现 spawn `taskkill /F /T`，在关机/注销时会话的控制台子系统
    /// 正在拆除，新建的控制台进程初始化失败（`0xc0000142`）并弹出错误框。
    /// `TerminateJobObject` 没有这个问题，也不依赖 PATH 上存在 taskkill。
    fn terminate(&self) -> bool {
        use windows::Win32::System::JobObjects::TerminateJobObject;
        unsafe { TerminateJobObject(self.0, 0).is_ok() }
    }
}

#[cfg(windows)]
impl Drop for GatewayJob {
    fn drop(&mut self) {
        // 显式关闭句柄：KILL_ON_JOB_CLOSE 在最后一个句柄关闭时终止 Job 内所有进程。
        // （网关运行期间句柄由 GatewayChild 持有、保持打开；进程结束时由系统关闭。）
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// 把新启动的网关子进程纳入一个新建的 Job：应用进程结束时由系统连带回收。
///
/// 每个网关进程一个 Job —— `stop_gateway` 会 `TerminateJobObject` 并关闭句柄，
/// 复用同一个 Job 就得依赖「已终止的 Job 能否再接收新进程」这一未文档化的行为，
/// 重建的代价又极低，因此不做复用。
///
/// 创建或加入失败不阻断启动（显式 stop_gateway 仍可正常停止），只打日志提示兜底失效。
#[cfg(windows)]
fn new_gateway_job(child: &Child) -> Option<GatewayJob> {
    match GatewayJob::create() {
        None => {
            eprintln!("[gateway] Job Object 创建失败：应用异常退出时可能残留网关进程");
            None
        }
        Some(job) => {
            if job.assign(child) {
                Some(job)
            } else {
                eprintln!(
                    "[gateway] 网关子进程加入 Job Object 失败：应用异常退出时可能残留网关进程"
                );
                None
            }
        }
    }
}

/// 网关是否在运行（本进程视角）。
pub fn is_running() -> bool {
    GATEWAY_RUNNING.load(Ordering::SeqCst)
}

/// 从监听地址解析端口，兼容多种写法：
/// `7863` / `:7863` / `0.0.0.0:7863` / `127.0.0.1:7863` / `[::]:7863`
pub fn port_of(listen: &str) -> u16 {
    let s = listen.trim();
    if s.is_empty() {
        return 7863;
    }
    // 纯数字
    if let Ok(p) = s.parse::<u16>() {
        return p;
    }
    s.rsplit(':')
        .next()
        .map(|p| p.trim().trim_end_matches(']'))
        .and_then(|p| p.parse::<u16>().ok())
        .filter(|p| *p > 0)
        .unwrap_or(7863)
}

/// 把端口规范化成网关可用的监听地址（`7863` → `:7863`）。
pub fn normalize_listen(port: u16) -> String {
    format!(":{port}")
}

/// 端口是否可绑定（对外暴露，供前端做占用检测）。
pub fn is_port_available(port: u16) -> bool {
    port > 0 && port_free(port)
}

/// 从 start 开始（含）向后找一个空闲端口，最多找 span 个。
pub fn find_free_port(start: u16, span: u16) -> Option<u16> {
    let begin = start.max(1024);
    for p in begin..begin.saturating_add(span.max(1)) {
        if port_free(p) {
            return Some(p);
        }
    }
    None
}

/// 端口占用检测结果（供前端即时反馈）。
pub fn inspect_port(port: u16) -> Value {
    // 注意：若网关自身正跑在该端口上，这里会判定为「被占用」。
    // 调用方（前端）需结合 status.running 判断，避免误报。
    let available = port > 0 && port_free(port);
    json!({
        "port": port,
        "available": available,
        "reserved": port > 0 && port < 1024,
        "inUseByGateway": is_running() && port_of(&load_gateway_config()
            .get("listen").and_then(Value::as_str).unwrap_or("")) == port,
        "suggest": if available { Value::Null } else {
            find_free_port(port.saturating_add(1), 50).map(|p| json!(p)).unwrap_or(Value::Null)
        },
    })
}

/// 出站代理地址：复用「设置 → 更新代理」里已填的值（`github_config.json` 的 `proxy`）。
///
/// 设计取舍：**不新增一个「网关代理」配置项**。用户在设置页填的更新代理，
/// 目的就是访问被墙的服务；国际版 workbuddy.ai 属于同类需求，让用户配两遍
/// 既啰嗦又容易只配一处导致「浏览器能用、网关不能用」的困惑。
///
/// 返回空串表示未配置（网关将直连，并在日志里提示国际版可能超时）。
fn upstream_proxy() -> String {
    crate::modules::update::load_github_config()
        .get("proxy")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 生成网关需要的 config.json（网关原生格式）。
fn write_native_config(cfg: &Value) -> Result<PathBuf, String> {
    let dir = gateway_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let auth_dir = gateway_auth_dir();
    let state_file = gateway_state_file();
    if let Some(parent) = state_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let native = json!({
        "listen": cfg.get("listen").and_then(Value::as_str).unwrap_or(":7863"),
        "api_key": cfg.get("api_key").and_then(Value::as_str).unwrap_or(""),
        "auth_dir": auth_dir.to_string_lossy(),
        "state_file": state_file.to_string_lossy(),
        "cooldown": { "soft_rate": "60s" },
        "schedule": {
            "checkin_hours": [9, 21],
            "keepalive_hours": [22],
            // 缺省 true；false = 关签到（猫猫旅行搭签到便车，因此也随之停摆）
            "checkin_enabled": cfg.get("checkin_enabled").and_then(Value::as_bool).unwrap_or(true),
            // 缺省 true；false = 关 token 保活
            "keepalive_enabled": cfg.get("keepalive_enabled").and_then(Value::as_bool).unwrap_or(true),
            // 签到 + 猫猫旅行的区域范围：cn（缺省，仅国服）/ all。
            // 国际版（workbuddy.ai）的 billing 与 growth 接口暂无真实数据，默认跳过。
            "checkin_scope": cfg.get("checkin_scope").and_then(Value::as_str).unwrap_or("cn"),
        },
        "upstream": {
            "timeout_seconds": 120,
            "header_timeout_seconds": 120,
            "idle_timeout_seconds": 300
        },
        "features": { "sanitize_blacklist_fingerprints": true },
        "upstash": { "url": "", "token": "" },
        // 出站代理：**复用**「设置 → 更新代理」里已填的地址，用户无需配两遍。
        //
        // 为什么网关需要它：国际版（workbuddy.ai）在国内直连不稳定（实测 wsarecv 超时），
        // 走代理才稳。而 Go 的 http.ProxyFromEnvironment **只读环境变量**、不读 Windows
        // 注册表，所以「浏览器能走系统代理」不代表网关也能。
        "proxy": upstream_proxy(),
        "pool": {
            "max_in_flight": 3,
            "breaker_threshold": 3,
            "breaker_cooldown": "30m",
            "breaker_cooldown_max": "6h",
            "idle_weight_per_hour": 0.5,
            "idle_weight_max": 5.0,
            // 「单一模型 + 积分轮转」：缺省 false = 负载均衡（老配置行为不变）。
            // 由当前**工作模式**推导，而不是读 cfg 里的独立开关 ——
            // 避免「模式是轮转、pool.rotation 却是 false」这类不一致状态。
            "rotation": matches!(
                GatewayMode::from_str(cfg.get("mode").and_then(Value::as_str).unwrap_or("balance")),
                GatewayMode::Rotation
            ),
            // 「单一模型」锁定：非空时网关只放行该模型。
            // 只在轮转模式下有意义 —— 负载均衡不限制模型（保持原有行为）。
            "allowed_model": if matches!(
                GatewayMode::from_str(cfg.get("mode").and_then(Value::as_str).unwrap_or("balance")),
                GatewayMode::Rotation
            ) {
                cfg.get("allowed_model").and_then(Value::as_str).unwrap_or("").trim()
            } else {
                ""
            }
        },
        "session_sticky": { "enabled": true, "ttl": "30m", "gc_interval": "5m" }
    });
    let path = dir.join("gateway_native_config.json");
    let text = serde_json::to_string_pretty(&native).map_err(|e| e.to_string())?;
    atomic_write(&path, &text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// 本地端口是否空闲。
///
/// 启动前必须预检：网关只在启动时绑定端口，若端口已被占用（例如另一个网关
/// 或 Docker 映射的服务），子进程会立刻退出，而后面的健康探测会打到**别人的**
/// 服务上并误报「启动成功」。这里先挡住这种误判。
fn port_free(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

    // 判定「端口可用」需要两个维度都通过，缺一不可：
    //
    //   1) 试绑 IPv4/IPv6 通配地址 —— 能发现「已绑定但尚未 accept」的监听者；
    //   2) 主动连接 127.0.0.1 / [::1]     —— 能发现「绑定在特定地址」的服务。
    //
    // 为什么必须两者结合（实测教训）：
    //   * 只看试绑：本机 Docker Desktop 把端口发布成 wslrelay/com.docker.backend
    //     的转发形式，试绑 0.0.0.0 与 [::] 都**能成功**，于是 7863 被误判为空闲，
    //     网关子进程随后绑定失败，而健康探测又打到 Docker 里的旧网关，
    //     最终报告一个虚假的「启动成功」。
    //   * 只看连接：占用但未监听（如处于 TIME_WAIT 的主动关闭端）会漏判。
    if TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).is_err() {
        return false;
    }
    match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))) {
        Ok(l) => drop(l),
        // 系统未启用 IPv6 不算占用
        Err(e) if e.kind() == std::io::ErrorKind::AddrNotAvailable => {}
        Err(_) => return false,
    }
    // 已有服务在应答 → 判定为被占用
    !has_listener(port)
}

/// 主动连接探测：127.0.0.1 或 [::1] 上有服务应答即认为端口被占用。
fn has_listener(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
    use std::time::Duration;

    let probes = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    for addr in probes {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return true;
        }
    }
    false
}

/// 探测网关健康（HTTP /healthz）。
/// /healthz 在无可用账号时返回 503，这代表进程活着但没有可用账号，
/// 因此只有连接失败才算「未就绪」。
pub async fn probe_health(port: u16, timeout_ms: u64) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| e.to_string())?;
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // 必须在 text() 之前取出响应头 —— text() 会消费 resp（E0382）
            let hdr_service = resp
                .headers()
                .get("X-Service")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = resp.text().await.unwrap_or_default();
            let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));

            // 身份校验：确认应答者确实是本项目的网关，而不是恰好占用同端口的
            // 其他服务（Docker 里的旧容器等）—— 否则会报告「假启动成功」。
            //
            // 优先用上游自 v0.2 起在 /healthz 提供的身份标识：
            //   - X-Service 响应头
            //   - 响应体 service 字段
            // 值均为 "workbuddy2api"。这比用 api_key 校验更可靠：
            // api_key 为空（未鉴权）时后者完全失去作用。
            //
            // 兼容旧版网关（无标识）：回退到「用 api_key 访问 /status 是否通」。
            let mut identity_ok = true;
            let body_service = parsed
                .get("service")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            if !hdr_service.is_empty() || !body_service.is_empty() {
                identity_ok = hdr_service == GATEWAY_SERVICE_NAME
                    || body_service == GATEWAY_SERVICE_NAME;
            } else {
                let cfg = load_gateway_config();
                let key = cfg.get("api_key").and_then(Value::as_str).unwrap_or("");
                if !key.is_empty() {
                    let surl = format!("http://127.0.0.1:{port}/status");
                    if let Ok(sresp) = client
                        .get(&surl)
                        .header("Authorization", format!("Bearer {key}"))
                        .send()
                        .await
                    {
                        identity_ok = sresp.status().as_u16() == 200;
                    }
                }
            }

            Ok(json!({
                "reachable": true,
                "http_status": status,
                "healthy": status == 200 && identity_ok,
                "identity_ok": identity_ok,
                "detail": parsed,
            }))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 启动网关子进程。成功返回可访问基址。
pub async fn start_gateway(cfg: &Value) -> Result<Value, String> {
    if is_running() {
        return Err("网关已在运行".to_string());
    }
    let Some(exe) = resolve_gateway_exe() else {
        return Err(format!(
            "未找到网关可执行文件 {}（可用环境变量 WB_SWITCH_GATEWAY_BIN 指定路径）",
            gateway_exe_name()
        ));
    };

    // 先把账号库导出为网关凭证，保证启动即能加载到账号
    let (count, _) = export_accounts_to_gateway()?;
    if count == 0 {
        // 区分「本来就没有账号」与「账号都在需重登状态」：后者不给出提示的话，
        // 用户只会看到「账号库为空」，完全不知道去重新登录就能恢复。
        let accounts = account::load_accounts();
        let dead = accounts.iter().filter(|a| needs_relogin(a)).count();
        if dead > 0 && dead == accounts.len() {
            return Err(format!(
                "账号库中的 {dead} 个账号都已标记「需重新登录」（refresh token 已失效），\
                 已暂停导出到网关。请先在「账号管理」页重新登录，恢复后会自动重新加入。"
            ));
        }
        if gateway_mode() == GatewayMode::Pinned {
            let pinned = pinned_uid().unwrap_or_default();
            if accounts
                .iter()
                .any(|a| account::get_str(a, "uid").as_deref() == Some(pinned.as_str()) && needs_relogin(a))
            {
                return Err(
                    "「指定账号」锁定的账号已标记「需重新登录」，请先在「账号管理」页重新登录，\
                     或改选其他账号。"
                        .to_string(),
                );
            }
        }
        return Err(match gateway_mode() {
            GatewayMode::Pinned => "「指定账号」模式尚未选择账号，请在网关页面选择后启动".to_string(),
            GatewayMode::Balance => "账号库为空，请先添加账号再启动网关".to_string(),
            // 轮转模式同样需要至少一个账号；此时「没有下一个可轮转」是主要问题。
            GatewayMode::Rotation => {
                "「单一模型 + 积分轮转」模式需要账号池中有可用账号，请先添加账号再启动网关".to_string()
            }
        });
    }

    // 统一端口来源：优先显式 port 字段，其次解析 listen
    let port = cfg
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .filter(|p| *p > 0)
        .unwrap_or_else(|| {
            port_of(cfg.get("listen").and_then(Value::as_str).unwrap_or(":7863"))
        });
    let mut cfg = cfg.clone();
    cfg["port"] = json!(port);
    cfg["listen"] = json!(normalize_listen(port));

    let native_cfg = write_native_config(&cfg)?;

    let mut cmd = Command::new(&exe);
    cmd.arg("-config")
        .arg(&native_cfg)
        .current_dir(gateway_dir())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // 非 Windows 平台不需要抑制控制台窗口，保持默认行为。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // 端口预检：被占用时直接给出可操作的错误，而不是让子进程启动即退出
    if !port_free(port) {
        return Err(format!(
            "端口 {port} 已被占用（可能已有网关或其他服务在监听）。请在配置中改用其他端口，\
             或先停止占用该端口的程序。"
        ));
    }

    let child = cmd.spawn().map_err(|e| format!("启动网关失败: {e}"))?;
    // 立即纳入 Job：之后父进程意外结束也会被系统连带终止，避免孤儿进程占端口。
    #[cfg(windows)]
    let job = new_gateway_job(&child);
    *proc_slot().lock().unwrap() = Some(GatewayChild {
        child,
        #[cfg(windows)]
        job,
    });
    GATEWAY_RUNNING.store(true, Ordering::SeqCst);

    // 等待端口就绪（最多 20s）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last_err = String::new();
    while std::time::Instant::now() < deadline {
        // 子进程若已退出（配置错误/端口冲突等），立即失败并回报退出码
        {
            let mut slot = proc_slot().lock().unwrap();
            if let Some(held) = slot.as_mut() {
                if let Ok(Some(status)) = held.child.try_wait() {
                    // 丢弃 GatewayChild：Job 句柄一并关闭，网关可能留下的子进程
                    // 由 KILL_ON_JOB_CLOSE 兜底回收。
                    *slot = None;
                    GATEWAY_RUNNING.store(false, Ordering::SeqCst);
                    return Err(format!(
                        "网关启动后立即退出（退出码 {:?}）。常见原因：端口被占用、\
                         配置无效或账号凭证不可读。",
                        status.code()
                    ));
                }
            }
        }
        match probe_health(port, 900).await {
            Ok(v) if v.get("identity_ok").and_then(Value::as_bool) == Some(false) => {
                last_err = format!(
                    "端口 {port} 上的服务不是本网关（api_key 校验未通过），\
                     可能已有其他网关在监听"
                );
            }
            Ok(v) => {
                return Ok(json!({
                    "started": true,
                    "base": format!("http://127.0.0.1:{port}"),
                    "port": port,
                    "accounts": count,
                    "health": v,
                }));
            }
            Err(e) => last_err = e,
        }
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    }

    stop_gateway();
    Err(format!("网关启动超时（端口 {port}）：{last_err}"))
}

/// 停止网关子进程。
///
/// Windows 上优先走 `TerminateJobObject`（连同网关的子进程树一起结束），
/// 不创建任何外部进程 —— 关机/注销时也能安全执行，见 [`GatewayJob::terminate`]。
/// Job 不可用（创建失败或加入失败）时退回 `Child::kill`：网关自身会被终止，
/// 但它若已拉起子进程则可能残留，此时只影响兜底彻底性，不影响用户可见行为。
pub fn stop_gateway() -> Value {
    let mut slot = proc_slot().lock().unwrap();
    let stopped = match slot.take() {
        Some(mut held) => {
            #[cfg(windows)]
            {
                let pid = held.child.id();
                match held.job.as_ref().map(GatewayJob::terminate) {
                    Some(true) => {}
                    Some(false) => {
                        eprintln!(
                            "[gateway] TerminateJobObject 失败（PID {pid}），退回直接结束进程"
                        );
                        let _ = held.child.kill();
                    }
                    None => {
                        let _ = held.child.kill();
                    }
                }
            }
            #[cfg(not(windows))]
            let _ = held.child.kill();

            let _ = held.child.wait();
            // held 在此丢弃：Job 句柄随之关闭。网关此时已被终止，关闭句柄只是
            // 释放资源（KILL_ON_JOB_CLOSE 对已空 Job 无副作用）。
            true
        }
        None => false,
    };
    GATEWAY_RUNNING.store(false, Ordering::SeqCst);
    json!({ "stopped": stopped })
}

/// 网关综合状态：配置 + 运行态 + 健康 + 账号池详情。
pub async fn gateway_status() -> Value {
    let cfg = load_gateway_config();
    let listen = cfg.get("listen").and_then(Value::as_str).unwrap_or(":7863");
    let port = port_of(listen);
    let exe = resolve_gateway_exe();
    let running = is_running();

    let mut health = Value::Null;
    let mut pool = Value::Null;
    let mut reachable = false;
    if running {
        if let Ok(h) = probe_health(port, 1200).await {
            reachable = true;
            health = h;
        }
        // 账号池详情（/status 需要 api_key）
        if reachable {
            let key = cfg.get("api_key").and_then(Value::as_str).unwrap_or("");
            let url = format!("http://127.0.0.1:{port}/status");
            if let Ok(client) = reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(1500))
                .build()
            {
                let mut req = client.get(&url);
                if !key.is_empty() {
                    req = req.header("Authorization", format!("Bearer {key}"));
                }
                if let Ok(resp) = req.send().await {
                    if let Ok(v) = resp.json::<Value>().await {
                        pool = v;
                    }
                }
            }
        }
    }

    let account_count = account::load_accounts().len();
    // 需重登的账号不导出到网关，界面需要能看到「为什么某个账号不在池里」。
    let excluded_accounts: Vec<Value> = account::load_accounts()
        .iter()
        .filter(|a| needs_relogin(a))
        .filter_map(|a| {
            let uid = account::get_str(a, "uid")?;
            Some(json!({
                "uid": uid,
                "nickname": account::get_str(a, "nickname").unwrap_or_default(),
                "reason": account::get_str(a, "needs_relogin_reason"),
            }))
        })
        .collect();
    let exe_path = exe.as_ref().map(|p| p.to_string_lossy().to_string());
    let exe_found = exe.is_some();
    json!({
        "running": running,
        "reachable": reachable,
        "base": format!("http://127.0.0.1:{port}"),
        "openaiBase": format!("http://127.0.0.1:{port}/v1"),
        "port": port,
        "exePath": exe_path,
        "exeFound": exe_found,
        "exeSource": gateway_source(),
        "mode": gateway_mode().as_str(),
        "pinnedUid": pinned_uid(),
        // 被排除出网关池的账号（需重新登录），供界面提示
        "excludedAccounts": excluded_accounts,
        // 供前端下拉选择「指定账号」
        "accounts": account::load_accounts()
            .iter()
            .filter_map(|a| {
                let uid = account::get_str(a, "uid")?;
                Some(json!({
                    "uid": uid,
                    "nickname": account::get_str(a, "nickname").unwrap_or_default(),
                    "expiresAt": a.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
                    "needsRelogin": a.get("needs_relogin").and_then(Value::as_bool).unwrap_or(false),
                }))
            })
            .collect::<Vec<Value>>(),
        "exeSource": gateway_source(),
        "portAvailable": port_free(port),
        "authDir": gateway_auth_dir().to_string_lossy(),
        "accountsInLibrary": account_count,
        "config": cfg,
        "health": health,
        "pool": pool,
    })
}

/// 按配置自动启动（供启动流程调用）。
///
/// 只认 `auto_start` 这一个开关 —— 界面上的「随 App 启动」对应的就是它。
///
/// 历史缺陷（本次修复）：原实现要求 `enabled && auto_start` 同时为真，但 `enabled`
/// 是 `default_gateway_config()` 里的遗留字段，**全仓库没有任何地方写入它**，
/// 永远停留在默认值 false，于是条件恒不成立、网关从不自动启动 —— 正是用户反馈的
/// 「勾了随 App 启动却从未生效」。`enabled` 与 `auto_start` 语义本就重叠
/// （都是「要不要自动跑」），保留前者参与判定只会制造这种恒假条件，故移除。
///
/// 另注：本函数此前**从未被调用**（死代码），已接入 `lib.rs` 的 setup 启动流程。
pub async fn maybe_autostart() -> Option<Value> {
    let cfg = load_gateway_config();
    let auto = cfg.get("auto_start").and_then(Value::as_bool).unwrap_or(false);
    if !auto {
        return None;
    }
    // 已在运行（如上次退出未清理干净）时不重复启动，但仍同步一次运行态。
    if is_running() {
        update_runtime_state("started", None);
        return None;
    }
    match start_gateway(&cfg).await {
        Ok(v) => {
            update_runtime_state("started", None);
            Some(v)
        }
        Err(e) => {
            update_runtime_state("failed", Some(e));
            None
        }
    }
}

/// 账号库变化后同步到网关，并让运行中的网关热加载。
///
/// 网关只在启动时扫描 auths/，所以新账号需要重启进程才能生效；
/// 这里按需重启，避免用户手动操作。
pub async fn sync_and_reload(restart_if_changed: bool) -> Value {
    let (count, changed) = match export_accounts_to_gateway() {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    let from_gateway = sync_auth_to_accounts().unwrap_or_default();

    let mut reloaded = false;
    if restart_if_changed && !changed.is_empty() && is_running() {
        let cfg = load_gateway_config();
        stop_gateway();
        if start_gateway(&cfg).await.is_ok() {
            reloaded = true;
        }
    }

    json!({
        "ok": true,
        "accounts": count,
        "changed": changed,
        "updatedFromGateway": from_gateway,
        "reloaded": reloaded,
    })
}

/// 仅同步账号（账号库 -> 网关），不重启。
pub fn sync_only() -> Value {
    match export_accounts_to_gateway() {
        Ok((count, changed)) => json!({ "ok": true, "accounts": count, "changed": changed }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

/// 构造模式切换要落盘的配置补丁。
///
/// 抽成纯函数便于测试：`switch_mode` 会真的写用户配置并可能重启网关，
/// 不适合在单测里直接调用。
///
/// 语义：切到负载均衡时 `pinned_uid` 置 null（避免残留旧锁定值导致
/// 下次切回指定账号时用到意料之外的账号）；空串同样归一为 null。
fn mode_patch(mode: GatewayMode, pinned_uid: Option<&str>) -> Value {
    let pinned = match pinned_uid.map(str::trim) {
        Some(uid) if !uid.is_empty() => json!(uid),
        _ => Value::Null,
    };
    json!({
        "mode": mode.as_str(),
        "pinned_uid": pinned,
    })
}

/// 读取「单一模型」锁定的模型名（配合轮转模式）；未设置返回空串。
pub fn allowed_model() -> String {
    load_gateway_config()
        .get("allowed_model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 归一化「单一模型」配置值：空串/纯空白 → Null（清除锁定），其余取 trim 后的值。
///
/// 抽成纯函数便于测试（真正的保存路径会写用户配置并可能重启网关）。
fn allowed_model_patch(model: &str) -> Value {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        json!({ "allowed_model": Value::Null })
    } else {
        json!({ "allowed_model": trimmed })
    }
}

/// 保存「单一模型 + 积分轮转」的目标模型并**立即生效**。
///
/// 空串 = 清除锁定（仍可轮转，但不限制模型 —— 不建议，客户端能换模型绕过
/// 额度控制）。
///
/// 为什么需要立即生效：`allowed_model` 由网关**启动时**读取，光落盘不会改变
/// 正在运行的进程（与切换模式同理，用户会看到「选了没反应」），因此在网关
/// 运行时重启它。
pub async fn set_allowed_model(model: &str) -> Value {
    let patch = allowed_model_patch(model);
    let cfg = match save_gateway_config(&patch) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };
    // 立即生效：网关只在启动时读配置。
    // 重启方式与 switch_mode 保持一致（先停、等端口释放、再启动）——
    // 端口未释放就启动会因占用而失败。
    let mut reloaded = false;
    if is_running() {
        stop_gateway();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        match start_gateway(&cfg).await {
            Ok(_) => {
                reloaded = true;
                update_runtime_state("started", None);
            }
            Err(e) => {
                update_runtime_state("failed", Some(e.clone()));
                return json!({
                    "ok": false,
                    "error": format!("模型已保存，但重启网关失败：{e}"),
                    "config": cfg,
                });
            }
        }
    }
    json!({
        "ok": true,
        "reloaded": reloaded,
        "allowedModel": model.trim(),
    })
}

/// 切换工作模式（负载均衡 / 指定账号）并**立即生效**。
///
/// 为什么需要这个专用入口：`save_gateway_config` 只写配置文件，
/// 而网关的账号池是**启动时**扫描 `gateway_auths/` 建立的，两者都不会
/// 因改配置而变化。于是用户点了「指定账号」后，池里仍是全部账号，
/// 必须手动点「重启」才真正生效（这正是「切换模式要重启」的根因）。
///
/// 这里把三件事合成一步：
///  1. 落盘新配置（mode / pinned_uid）
///  2. 按新模式重导出凭证（清理不再需要的账号文件）
///  3. 若网关正在运行，重启它以加载新池
///
/// 未运行时只做 1+2：下次启动自然是新池，无需空转重启。
pub async fn switch_mode(mode: GatewayMode, pinned_uid: Option<String>) -> Value {
    let patch = mode_patch(mode, pinned_uid.as_deref());
    let cfg = match save_gateway_config(&patch) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    // 指定账号模式必须先选好账号，否则池会是空的 —— 提前拦住并给出可操作提示，
    // 而不是让用户看到一个「启动了但没有账号」的网关。
    if mode == GatewayMode::Pinned && pinned_uid.as_deref().unwrap_or("").trim().is_empty() {
        return json!({
            "ok": false,
            "error": "「指定账号」模式需要先选择一个账号",
            "config": cfg,
        });
    }

    let (count, changed) = match export_accounts_to_gateway() {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e, "config": cfg }),
    };

    let mut reloaded = false;
    if is_running() {
        stop_gateway();
        // 停止后端口需要一点时间释放（TIME_WAIT / 子进程退出），
        // 否则紧接着的 start 会因端口被占用而失败。
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        match start_gateway(&cfg).await {
            Ok(_) => {
                reloaded = true;
                update_runtime_state("started", None);
            }
            Err(e) => {
                update_runtime_state("failed", Some(e.clone()));
                return json!({
                    "ok": false,
                    "error": format!("模式已保存，但重启网关失败：{e}"),
                    "accounts": count,
                    "changed": changed,
                    "config": cfg,
                });
            }
        }
    }

    json!({
        "ok": true,
        "mode": mode.as_str(),
        "pinnedUid": pinned_uid,
        "accounts": count,
        "changed": changed,
        "reloaded": reloaded,
        "config": cfg,
    })
}

/// 从运行中的网关动态拉取上游模型列表。仅从网关实时获取，不使用内置静态模型。
pub async fn fetch_models() -> Vec<Value> {
    let cfg = load_gateway_config();
    let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(7863) as u16;
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let url = format!("http://127.0.0.1:{port}/v1/models");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(2000))
        .build();

    if let Ok(c) = client {
        let mut req = c.get(&url);
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {api_key}"));
        }
        if let Ok(resp) = req.send().await {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<Value>().await {
                    if let Some(arr) = json.get("data").and_then(Value::as_array) {
                        if !arr.is_empty() {
                            return arr.clone();
                        }
                    }
                }
            }
        }
    }

    // 网关未运行 / 未就绪 → 回退内置静态清单。
    //
    // 为什么必须有回退：模型下拉若为空，用户就无法选择「单一模型」，
    // 而「配置模型」这一步通常发生在**启动网关之前**（先配好再启动）——
    // 若只依赖运行中的网关，这个顺序下功能直接不可用（实测就是空列表）。
    static_models()
}

/// 内置模型清单（网关不可达时的回退）。
///
/// 取自上游已知的常用模型；运行中的网关会返回更权威的动态列表，
/// 此处仅保证「未启动时也能选」。
fn static_models() -> Vec<Value> {
    const IDS: &[&str] = &[
        "deepseek-v4.1-flash",
        "deepseek-v4-flash",
        "deepseek-v4-pro",
        "deepseek-v3-2-volc",
        "glm-5.3",
        "glm-5.3-flash",
        "glm-5.2",
        "glm-4.7",
        "kimi-k3-1",
        "kimi-k2.5",
        "minimax-m3",
        "hunyuan-chat",
        "gpt-5.6-sol",
        "gemini-3.5-flash",
    ];
    IDS.iter()
        .map(|id| json!({ "id": id, "object": "model", "owned_by": "workbuddy" }))
        .collect()
}

/// 从运行中的网关拉取 Token 用量统计（GET /usage）。
///
/// `days` 为统计范围（近 N 天，含今天）；None 或非正值表示全部历史。
/// 网关未运行 / 未就绪 / 返回异常时同样返回结构化结果而不报错，
/// 由调用方依据 `running` / `reachable` / `usage` 给出不同提示：
///   { "running": bool, "reachable": bool, "usage": {...} | null, "error": string | null }
pub async fn fetch_usage(days: Option<i64>) -> Value {
    let cfg = load_gateway_config();
    let port = cfg.get("port").and_then(Value::as_u64).unwrap_or(7863) as u16;
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut url = format!("http://127.0.0.1:{port}/usage");
    if let Some(d) = days.filter(|d| *d > 0) {
        url.push_str(&format!("?days={d}"));
    }

    let running = is_running();
    let fail = |error: String| {
        json!({
            "running": running,
            "reachable": false,
            "usage": Value::Null,
            "error": error,
        })
    };

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(2500))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(format!("无法创建 HTTP 客户端: {e}")),
    };

    let mut req = client.get(&url);
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                return fail(format!("网关 /usage 返回 HTTP {status}"));
            }
            match resp.json::<Value>().await {
                Ok(v) => json!({
                    "running": running,
                    "reachable": true,
                    "usage": v,
                    "error": Value::Null,
                }),
                Err(e) => fail(format!("解析网关 /usage 响应失败: {e}")),
            }
        }
        Err(e) => fail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 回归保护：局部更新（如启动后回写 last_status）不得覆盖用户设置。
    // 曾经的缺陷是 merge 以「默认值」为基准，导致 api_key / listen 被重置，
    // 前端账号池查询因此带上空 key 而显示为空。
    #[test]
    fn runtime_state_update_preserves_user_config() {
        let dir = std::env::temp_dir().join(format!("wb-gw-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 直接验证 overlay/merge 的语义（不依赖真实 store 目录）
        let defaults = json!({"listen": ":7863", "api_key": "", "auto_start": false});
        let disk = json!({"listen": ":7899", "api_key": "sk-user", "auto_start": true});
        let with_disk = super::overlay(defaults, &disk);
        let after_runtime = super::overlay(with_disk, &json!({"last_status": "started"}));

        assert_eq!(after_runtime["listen"], ":7899", "监听地址不应被运行态更新覆盖");
        assert_eq!(after_runtime["api_key"], "sk-user", "api_key 不应被运行态更新清空");
        assert_eq!(after_runtime["auto_start"], true, "自动启动设置不应丢失");
        assert_eq!(after_runtime["last_status"], "started");
    }

    #[test]
    fn overlay_only_touches_provided_keys() {
        let base = json!({"a": 1, "b": 2});
        let out = super::overlay(base, &json!({"b": 9}));
        assert_eq!(out["a"], 1);
        assert_eq!(out["b"], 9);
    }

    // 回归保护：切换「负载均衡 ↔ 指定账号」时，导出与清理必须用同一判定。
    // 曾经的缺陷是清理用账号库全集，导致切到指定账号后其余凭证残留，
    // 网关仍把它们加载进池 —— 表现为「模式切换没生效」。
    #[test]
    fn select_export_uids_filters_by_mode() {
        let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        // 负载均衡：不过滤
        assert_eq!(select_export_uids(&all, &None), all);

        // 指定账号：只保留选中的
        let only = Some(vec!["b".to_string()]);
        assert_eq!(select_export_uids(&all, &only), vec!["b".to_string()]);

        // 选中的不在账号库里（例如账号已删除）：结果为空 → 清理掉全部残留
        let missing = Some(vec!["zzz".to_string()]);
        assert!(select_export_uids(&all, &missing).is_empty());

        // 空选择：同样应清空（调用方据此报「尚未选择账号」）
        assert!(select_export_uids(&all, &Some(vec![])).is_empty());
    }

    // 账号指纹：内容变化必须导致指纹变化，否则自动同步会漏掉新账号。
    #[test]
    fn fingerprint_reflects_token_and_expiry_changes() {
        let base = vec![
            ("uid-1".to_string(), "AT1".to_string(), 1000i64),
            ("uid-2".to_string(), "AT2".to_string(), 2000i64),
        ];
        let calc = |items: &[(String, String, i64)]| -> String {
            let mut parts: Vec<String> = items
                .iter()
                .map(|(u, at, exp)| format!("{u}|{at}|{exp}"))
                .collect();
            parts.sort();
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for p in &parts {
                for b in p.as_bytes() {
                    hash ^= *b as u64;
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
            }
            format!("{}-{:x}", parts.len(), hash)
        };

        let f1 = calc(&base);
        assert_eq!(f1, calc(&base), "相同内容必须得到相同指纹");

        // token 变化
        let mut changed = base.clone();
        changed[0].1 = "AT1-NEW".to_string();
        assert_ne!(f1, calc(&changed), "token 变化必须改变指纹");

        // 过期时间变化
        let mut changed2 = base.clone();
        changed2[1].2 = 3000;
        assert_ne!(f1, calc(&changed2), "过期时间变化必须改变指纹");

        // 新增账号
        let mut added = base.clone();
        added.push(("uid-3".to_string(), "AT3".to_string(), 3000));
        assert_ne!(f1, calc(&added), "新增账号必须改变指纹");

        // 删除账号
        let removed = vec![base[0].clone()];
        assert_ne!(f1, calc(&removed), "删除账号必须改变指纹");
    }

    #[test]
    fn finalize_fills_listen_default() {
        let cfg = super::finalize_gateway_config(json!({"listen": "  "}));
        assert_eq!(cfg["listen"], ":7863");
    }

    // 回归保护：被标记「需重新登录」的账号不得导出到网关。
    //
    // 曾经的缺陷是导出只看 uid/模式，完全无视 `needs_relogin` —— 于是 refresh
    // token 已被服务端拒绝的账号仍留在网关池里，网关每次请求都拿它去试一轮，
    // 表现为「网关无视账号需重登，一直用失效账号调用模型」。
    #[test]
    fn needs_relogin_accounts_are_excluded_from_gateway_export() {
        let healthy = json!({"uid": "uid-ok", "access_token": "AT", "domain": "www.workbuddy.ai"});
        let dead = json!({
            "uid": "uid-dead",
            "access_token": "AT",
            "domain": "www.workbuddy.ai",
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=12153)",
        });

        assert!(!needs_relogin(&healthy));
        assert!(needs_relogin(&dead));

        // 判定口径与导出循环一致：dead 应被 should_export 拒绝。
        let only: Option<Vec<String>> = None;
        let should_export = |acc: &Value| -> bool {
            if needs_relogin(acc) {
                return false;
            }
            let uid = account::get_str(acc, "uid").unwrap_or_default();
            !select_export_uids(&[uid], &only).is_empty()
        };
        assert!(should_export(&healthy), "健康账号必须导出");
        assert!(!should_export(&dead), "需重登账号必须被排除出网关凭证目录");

        // 清理清单同样不含它 —— 否则残留凭证会被网关继续加载进池。
        let accounts = vec![healthy, dead];
        let live: Vec<String> = accounts
            .iter()
            .filter(|a| !needs_relogin(a))
            .filter_map(|a| account::get_str(a, "uid"))
            .collect();
        assert_eq!(live, vec!["uid-ok".to_string()]);
    }

    // 标记翻转必须改变指纹，否则自动同步不会重推凭证（账号会一直留在池里）。
    #[test]
    fn fingerprint_changes_when_relogin_flag_flips() {
        let acc_ok = json!({"uid": "uid-1", "access_token": "AT", "expiresAt": 1000});
        let mut acc_dead = acc_ok.clone();
        acc_dead["needs_relogin"] = json!(true);

        let fingerprint = |a: &Value| -> String {
            let uid = account::get_str(a, "uid").unwrap_or_default();
            let at = account::get_str(a, "access_token").unwrap_or_default();
            let rt = account::get_str(a, "refresh_token").unwrap_or_default();
            let exp = a.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
            let at_tail: String = at.chars().rev().take(12).collect();
            let rt_len = rt.len();
            let relogin = if needs_relogin(a) { 1 } else { 0 };
            format!("{uid}|{at_tail}|{rt_len}|{}|{relogin}", to_sec(exp))
        };

        assert_ne!(
            fingerprint(&acc_ok),
            fingerprint(&acc_dead),
            "需重登标记必须参与指纹，否则标记翻转后不会触发重新同步"
        );
    }

    /// 端到端：导出到真实目录时，需重登账号的凭证文件必须被写入-剔除。
    ///
    /// 覆盖两种路径：
    ///   1. 标记出现时，已有的凭证文件必须被**删除**（否则网关重启仍会加载它）；
    ///   2. 标记清除后（重新登录成功），凭证必须被**写回**。
    #[test]
    fn export_removes_and_restores_relogin_account_credentials() {
        let dir = std::env::temp_dir().join(format!(
            "wb-gw-export-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let healthy = json!({
            "uid": "uid-ok", "nickname": "健康号", "access_token": "AT-OK",
            "refresh_token": "RT-OK", "domain": "www.workbuddy.ai", "expiresAt": 1_900_000_000_000i64,
        });
        let dead = json!({
            "uid": "uid-dead", "nickname": "失效号", "access_token": "AT-DEAD",
            "refresh_token": "RT-DEAD", "domain": "www.workbuddy.ai", "expiresAt": 1_900_000_000_000i64,
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=12153): Offline user session not found",
        });

        // 1) 健康账号：凭证落盘（count 统计的是处理的账号记录数，不是文件数）
        let (count, _) = export_accounts_to_dir(&dir, &[healthy.clone()], None)
            .expect("export healthy");
        assert_eq!(count, 1);
        assert!(dir.join("workbuddy-uid-ok.json").exists());

        // 先把失效账号以「健康」形态写进去（模拟它此前登录正常、凭证已在池中）
        let dead_before = json!({
            "uid": "uid-dead", "nickname": "失效号", "access_token": "AT-DEAD",
            "refresh_token": "RT-DEAD", "domain": "www.workbuddy.ai", "expiresAt": 1_900_000_000_000i64,
        });
        export_accounts_to_dir(&dir, &[dead_before], None).expect("seed dead before flag");
        assert!(
            dir.join("workbuddy-uid-dead.json").exists(),
            "前置条件：失效账号的凭证文件此刻存在"
        );

        // 2) 账号被打上需重登标记后：它的凭证必须被清理，健康账号保留
        let (_, changed) = export_accounts_to_dir(&dir, &[healthy.clone(), dead.clone()], None)
            .expect("export with dead");
        assert!(
            !dir.join("workbuddy-uid-dead.json").exists(),
            "需重登账号的凭证文件必须被删除，否则网关仍会把它加载进池"
        );
        assert!(
            dir.join("workbuddy-uid-ok.json").exists(),
            "健康账号不受影响"
        );
        assert!(
            changed.iter().any(|c| c == "removed:workbuddy-uid-dead.json"),
            "变更列表应报告删除，便于界面/日志追踪：{changed:?}"
        );

        // 3) 重新登录成功（标记清除）后：凭证必须被写回
        let recovered = json!({
            "uid": "uid-dead", "nickname": "失效号", "access_token": "AT-NEW",
            "refresh_token": "RT-NEW", "domain": "www.workbuddy.ai", "expiresAt": 1_900_000_000_000i64,
        });
        export_accounts_to_dir(&dir, &[healthy, recovered], None).expect("export recovered");
        assert!(
            dir.join("workbuddy-uid-dead.json").exists(),
            "重新登录后凭证必须被写回，账号才能回到池中"
        );
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("workbuddy-uid-dead.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["auth"]["accessToken"], "AT-NEW");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 历史误报标记（code=-1）不得导致账号被移出网关池。
    #[test]
    fn legacy_transport_error_flag_keeps_account_in_pool() {
        let dir = std::env::temp_dir().join(format!(
            "wb-gw-legacy-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let legacy = json!({
            "uid": "uid-legacy", "nickname": "被误判的号", "access_token": "AT",
            "refresh_token": "RT", "domain": "www.workbuddy.ai",
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败(code=-1): error sending request for url (https://www.workbuddy.ai/v2/plugin/auth/token/refresh)",
        });

        let (count, _) =
            export_accounts_to_dir(&dir, &[legacy], None).expect("export legacy");
        assert_eq!(count, 1, "传输层失败留下的误报标记不应把账号排除出池");
        assert!(dir.join("workbuddy-uid-legacy.json").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    // 模式切换补丁：切回负载均衡必须清掉 pinned_uid（否则残留旧锁定值）。
    #[test]
    fn mode_patch_sets_mode_and_pinned() {
        let p = super::mode_patch(GatewayMode::Pinned, Some("uid-1"));
        assert_eq!(p["mode"], "pinned");
        assert_eq!(p["pinned_uid"], "uid-1");

        // 负载均衡：pinned_uid 归 null，不带任何遗留值。
        let b = super::mode_patch(GatewayMode::Balance, None);
        assert_eq!(b["mode"], "balance");
        assert!(b["pinned_uid"].is_null());

        // 空串/纯空白同样归一为 null（前端「未选择」会传空串）。
        for empty in ["", "   "] {
            let e = super::mode_patch(GatewayMode::Pinned, Some(empty));
            assert!(e["pinned_uid"].is_null(), "empty {empty:?} must become null");
        }

        // uid 首尾空白被裁掉（避免与账号库里的 uid 不匹配导致导出为空）。
        let t = super::mode_patch(GatewayMode::Pinned, Some("  uid-2  "));
        assert_eq!(t["pinned_uid"], "uid-2");
    }

    // 模式字符串解析：未知值一律回落 balance（不报错、不误锁账号）。
    #[test]
    fn gateway_mode_from_str_defaults_to_balance() {
        assert_eq!(GatewayMode::from_str("pinned"), GatewayMode::Pinned);
        assert_eq!(GatewayMode::from_str("PINNED"), GatewayMode::Pinned);
        assert_eq!(GatewayMode::from_str("single"), GatewayMode::Pinned);
        assert_eq!(GatewayMode::from_str("balance"), GatewayMode::Balance);
        assert_eq!(GatewayMode::from_str(""), GatewayMode::Balance);
        assert_eq!(GatewayMode::from_str("garbage"), GatewayMode::Balance);
    }

    // 「单一模型 + 积分轮转」模式的解析与序列化。
    #[test]
    fn rotation_mode_round_trips() {
        for s in ["rotation", "ROTATION", "rotate", "rolling"] {
            assert_eq!(GatewayMode::from_str(s), GatewayMode::Rotation, "输入 {s:?}");
        }
        assert_eq!(GatewayMode::Rotation.as_str(), "rotation");
        // 不能被误当成 pinned（两者语义完全不同：pinned 只导出一个账号）
        assert_ne!(GatewayMode::from_str("rotation"), GatewayMode::Pinned);
    }

    // 轮转模式的 mode_patch 必须清掉 pinned_uid —— 否则从「指定账号」切过去时
    // 会残留旧锁定值，而 active_uids() 对轮转是「不过滤」，残留值虽不生效，
    // 但下次切回 pinned 会用到意料之外的账号。
    #[test]
    fn mode_patch_for_rotation_clears_pinned() {
        let r = super::mode_patch(GatewayMode::Rotation, None);
        assert_eq!(r["mode"], "rotation");
        assert!(r["pinned_uid"].is_null(), "轮转模式不应带 pinned_uid");
    }

    // write_native_config 必须把 rotation 从**模式**推导出来（而非独立开关），
    // 避免出现「模式=轮转但 pool.rotation=false」这类自相矛盾的配置。
    #[test]
    fn native_config_derives_rotation_from_mode() {
        // 用 helper 直接构造 native 配置太重（会写盘），这里验证推导规则本身：
        let derive = |mode: &str| {
            matches!(
                GatewayMode::from_str(mode),
                GatewayMode::Rotation
            )
        };
        assert!(derive("rotation"));
        assert!(!derive("balance"));
        assert!(!derive("pinned"));
        assert!(!derive("garbage"));
    }

    // 凭证导出必须原样带着 credit 块：它是网关「按积分到期分层选号」的依据。
    // 丢了它，每次账号同步都会把依据抹掉一次，表现为分层均衡时灵时不灵。
    #[test]
    fn build_auth_doc_preserves_credit_block() {
        let acc = json!({
            "uid": "u1",
            "access_token": "at",
            "refresh_token": "rt",
            "nickname": "n1",
            "domain": "www.workbuddy.cn",
            "expiresAt": 1793263003967i64,
        });

        // 无 credit：不出现该键（保持文件精简）。
        let (file, text) = super::build_auth_doc(&acc, None).expect("doc");
        assert_eq!(file, "workbuddy-u1.json");
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert!(doc.get("credit").is_none(), "no credit expected: {text}");
        // expiresAt 必须转成秒（网关侧按秒解析）。
        assert_eq!(doc["auth"]["expiresAt"], 1793263003i64);

        // 有 credit：原样透传（网关据此分档）。
        let credit = json!({"soonestExpireAt": 1790639067i64});
        let (_, text2) = super::build_auth_doc(&acc, Some(credit)).expect("doc");
        let doc2: Value = serde_json::from_str(&text2).unwrap();
        assert_eq!(doc2["credit"]["soonestExpireAt"], 1790639067i64);

        // 非对象（脏数据）不应写进凭证。
        let (_, text3) = super::build_auth_doc(&acc, Some(json!("garbage"))).expect("doc");
        let doc3: Value = serde_json::from_str(&text3).unwrap();
        assert!(doc3.get("credit").is_none(), "non-object credit must be ignored");
    }

    // read_credit_block 从已有凭证里取回 credit；文件缺失/损坏/无该键时返回 None。
    #[test]
    fn read_credit_block_handles_missing_and_malformed() {
        let dir = std::env::temp_dir().join(format!("wb-switch-credit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"auth":{},"credit":{"soonestExpireAt":123}}"#).unwrap();
        assert_eq!(
            super::read_credit_block(&good).unwrap()["soonestExpireAt"],
            123
        );

        let nokey = dir.join("nokey.json");
        std::fs::write(&nokey, r#"{"auth":{}}"#).unwrap();
        assert!(super::read_credit_block(&nokey).is_none());

        let bad = dir.join("bad.json");
        std::fs::write(&bad, "not json").unwrap();
        assert!(super::read_credit_block(&bad).is_none());

        assert!(super::read_credit_block(&dir.join("absent.json")).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // 孤儿进程防护：Job 句柄关闭（父进程结束的等价事件）必须连带终止子进程。
    // 覆盖崩溃/任务管理器强杀等 stop_gateway 来不及执行的情况。
    #[cfg(windows)]
    #[test]
    fn job_object_kills_child_when_handle_closes() {
        use std::time::{Duration, Instant};

        let Some(job) = super::GatewayJob::create() else {
            // 极少数环境不支持 Job Object 时跳过；显式 stop_gateway 仍然有效。
            return;
        };
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        assert!(job.assign(&child), "子进程应成功加入 Job");

        drop(job); // 等价于父进程退出时系统自动关闭句柄

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("Job 句柄关闭后子进程仍存活");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // 启动路径接线：new_gateway_job 必须把子进程登记进新建的 Job，
    // 否则 Job 兜底形同虚设。
    #[cfg(windows)]
    #[test]
    fn new_gateway_job_registers_child() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping", "-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let Some(job) = super::new_gateway_job(&child) else {
            // 环境不支持 Job Object 时跳过；显式 stop_gateway 仍然有效。
            let _ = child.kill();
            let _ = child.wait();
            return;
        };

        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::{BOOL, HANDLE};
        use windows::Win32::System::JobObjects::IsProcessInJob;
        let mut in_job = BOOL::default();
        unsafe {
            IsProcessInJob(HANDLE(child.as_raw_handle()), job.0, &mut in_job)
                .expect("IsProcessInJob");
        }
        assert!(in_job.as_bool(), "子进程应已加入 Job");

        let _ = child.kill();
        let _ = child.wait();
    }

    // 退出路径的关键行为：stop_gateway 必须用 TerminateJobObject 结束**整棵**
    // 进程树，且不创建任何外部进程。
    //
    // 回归背景（issue #21）：原先 spawn `taskkill /F /T`，在关机/注销时控制台
    // 子系统正在拆除，新建的控制台进程初始化失败（0xc0000142）并弹出错误框。
    // 这里通过一个会再拉起孙进程的中间进程，验证 Job 的连带终止能力。
    #[cfg(windows)]
    #[test]
    fn terminate_job_kills_whole_process_tree() {
        use std::time::{Duration, Instant};

        let Some(job) = super::GatewayJob::create() else {
            return; // 环境不支持 Job Object 时跳过
        };
        // cmd 先 sleep 约 1s 再拉起 ping 作为孙进程：确保孙进程是在加入 Job
        // **之后**才创建的（Job 只对加入之后创建的子进程生效），避免竞态。
        // 只 kill cmd 不会终止 ping，因此孙进程存活即代表进程树没被终止。
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >nul & ping -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        assert!(job.assign(&child), "子进程应成功加入 Job");
        let grandchild = wait_for_child_pid(&child, Duration::from_secs(10));

        assert!(job.terminate(), "TerminateJobObject 应成功");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let child_gone = child.try_wait().expect("try_wait").is_some();
            let grandchild_gone = !pid_alive(grandchild);
            if child_gone && grandchild_gone {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "TerminateJobObject 后进程树仍存活（cmd={} grandchild={grandchild}）",
                    child.id()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // 端到端：真正调用公开的 stop_gateway()，验证它确实走 Job 终止进程树，
    // 而不是只把 Child 句柄丢掉。覆盖 stop_gateway -> GatewayJob::terminate 的接线。
    #[cfg(windows)]
    #[test]
    fn stop_gateway_terminates_installed_process_tree() {
        use std::time::Duration;

        // 与上一个测试同样先延迟再拉孙进程，保证孙进程在加入 Job 之后创建。
        let child = Command::new("cmd")
            .args(["/C", "ping -n 2 127.0.0.1 >nul & ping -n 30 127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let job = super::new_gateway_job(&child);
        let grandchild = wait_for_child_pid(&child, Duration::from_secs(10));

        *super::proc_slot().lock().unwrap() = Some(super::GatewayChild { child, job });
        super::GATEWAY_RUNNING.store(true, Ordering::SeqCst);

        let result = super::stop_gateway();
        assert_eq!(result["stopped"], true, "已装入子进程时应报告 stopped=true");
        assert!(!super::is_running(), "stop_gateway 后运行标志应清零");
        assert!(
            super::proc_slot().lock().unwrap().is_none(),
            "stop_gateway 后进程槽应清空"
        );

        // 孙进程若仍在，说明只结束了直接子进程而没有终止整棵树。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pid_alive(grandchild) {
            if std::time::Instant::now() >= deadline {
                panic!("stop_gateway 后孙进程仍存活（PID {grandchild}）");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 轮询等待指定进程拉起第一个子进程，返回其 PID（超时 panic）。
    #[cfg(windows)]
    fn wait_for_child_pid(parent: &Child, timeout: std::time::Duration) -> u32 {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(pid) = first_child_pid(parent.id()) {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待 PID {} 拉起子进程超时",
                parent.id()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// 进程是否仍存活（测试用）。用 OpenProcess 探测，避免依赖 tasklist。
    #[cfg(windows)]
    fn pid_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false; // 进程已退出（PID 不存在）
            };
            let mut code = 0u32;
            let alive =
                GetExitCodeProcess(handle, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
            let _ = CloseHandle(handle);
            alive
        }
    }

    /// 查指定 PID 的第一个子进程（测试用）。
    #[cfg(windows)]
    fn first_child_pid(pid: u32) -> Option<u32> {
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    if entry.th32ParentProcessID == pid {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(snap);
            found
        }
    }
}
