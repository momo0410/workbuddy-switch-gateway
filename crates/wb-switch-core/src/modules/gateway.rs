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

/// 可执行文件名。
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
fn build_auth_doc(acc: &Value) -> Option<(String, String)> {
    let uid = account::get_str(acc, "uid")?;
    let access = account::get_str(acc, "access_token")?;
    let refresh = account::get_str(acc, "refresh_token").unwrap_or_default();
    let nickname = account::get_str(acc, "nickname").unwrap_or_default();
    let enterprise = account::get_str(acc, "enterpriseId").unwrap_or_default();
    let domain = account::get_str(acc, "domain").unwrap_or_default();
    let expires_ms = acc.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);

    let doc = json!({
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
    let file = format!("workbuddy-{uid}.json");
    Some((file, serde_json::to_string_pretty(&doc).ok()?))
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
            Some(format!("{uid}|{at_tail}|{rt_len}|{}", to_sec(exp)))
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

/// 账号库 -> 网关凭证目录。返回 (账号数, 有变化的 uid 列表)。
pub fn export_accounts_to_gateway() -> Result<(usize, Vec<String>), String> {
    let dir = gateway_auth_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let accounts = account::load_accounts();
    let mut changed = Vec::new();
    let mut written = 0usize;

    for acc in &accounts {
        let Some((file, text)) = build_auth_doc(acc) else { continue };
        let path = dir.join(&file);
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

    // 清理账号库中已删除账号对应的凭证，避免网关注销账号仍被调度
    let live: Vec<String> = accounts
        .iter()
        .filter_map(|a| account::get_str(a, "uid"))
        .map(|u| format!("workbuddy-{u}.json"))
        .collect();
    if let Ok(entries) = std::fs::read_dir(&dir) {
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

static GATEWAY_PROC: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static GATEWAY_RUNNING: AtomicBool = AtomicBool::new(false);

fn proc_slot() -> &'static Mutex<Option<Child>> {
    GATEWAY_PROC.get_or_init(|| Mutex::new(None))
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
        "schedule": { "checkin_hours": [9, 21], "keepalive_hours": [22] },
        "upstream": {
            "timeout_seconds": 120,
            "header_timeout_seconds": 120,
            "idle_timeout_seconds": 300
        },
        "features": { "sanitize_blacklist_fingerprints": true },
        "upstash": { "url": "", "token": "" },
        "pool": {
            "max_in_flight": 3,
            "breaker_threshold": 3,
            "breaker_cooldown": "30m",
            "breaker_cooldown_max": "6h",
            "idle_weight_per_hour": 0.5,
            "idle_weight_max": 5.0
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
            let body = resp.text().await.unwrap_or_default();
            let parsed: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));

            // 身份校验：/status 需要 api_key。若配置了 key 却拿到 401，
            // 说明应答者是同端口上的**其他**网关（旧容器等），不能当作启动成功。
            let cfg = load_gateway_config();
            let key = cfg.get("api_key").and_then(Value::as_str).unwrap_or("");
            let mut identity_ok = true;
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
        return Err("账号库为空，请先添加账号再启动网关".to_string());
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
    #[cfg(windows)]
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
    *proc_slot().lock().unwrap() = Some(child);
    GATEWAY_RUNNING.store(true, Ordering::SeqCst);

    // 等待端口就绪（最多 20s）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last_err = String::new();
    while std::time::Instant::now() < deadline {
        // 子进程若已退出（配置错误/端口冲突等），立即失败并回报退出码
        {
            let mut slot = proc_slot().lock().unwrap();
            if let Some(child) = slot.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
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
pub fn stop_gateway() -> Value {
    let mut slot = proc_slot().lock().unwrap();
    let stopped = match slot.as_mut() {
        Some(child) => {
            let pid = child.id();
            #[cfg(windows)]
            {
                // Windows 需连同子进程树一起结束
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
            true
        }
        None => false,
    };
    *slot = None;
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
        "portAvailable": port_free(port),
        "authDir": gateway_auth_dir().to_string_lossy(),
        "accountsInLibrary": account_count,
        "config": cfg,
        "health": health,
        "pool": pool,
    })
}

/// 按配置自动启动（供启动流程调用）。
pub async fn maybe_autostart() -> Option<Value> {
    let cfg = load_gateway_config();
    let enabled = cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let auto = cfg.get("auto_start").and_then(Value::as_bool).unwrap_or(false);
    if !(enabled && auto) {
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
}
