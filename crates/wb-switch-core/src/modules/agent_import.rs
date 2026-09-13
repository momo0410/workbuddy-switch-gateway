//! 一键导入与更新：把本网关接入本机已安装的 AI 客户端。
//!
//! 支持 11 类主流客户端智能体：
//!
//! | # | 客户端 | 配置文件 | 协议 |
//! |---|---|---|---|
//! | 1 | Claude Code | `~/.claude/settings.json` | Anthropic Messages |
//! | 2 | Claude Desktop | `%LOCALAPPDATA%\Claude-3p\configLibrary\*.json` | Anthropic Messages (3P) |
//! | 3 | Codex | `~/.codex/config.toml` + `auth.json` | OpenAI Responses |
//! | 4 | DeepSeek Harness (DSH) | `~/.dsh/settings.yaml` + `.credentials.yaml` | OpenAI Chat |
//! | 5 | OpenCode | `~/.config/opencode/opencode.json` | OpenAI Chat |
//! | 6 | Pi | `~/.pi/agent/models.json` | OpenAI Chat |
//! | 7 | Grok Build | `~/.grok/config.toml` | OpenAI Responses / Chat |
//! | 8 | ZCode | `~/.zcode/v2/config.json` | OpenAI Chat / Anthropic |
//! | 9 | Kimi Code | `~/.kimi-code/config.toml` | OpenAI Chat |
//! | 10 | OpenClaw | `~/.openclaw/openclaw.json` | OpenAI Chat |
//! | 11 | Hermes Agent | `~/.hermes/config.yaml` | OpenAI Chat |
//!
//! 安全约定：
//!   - 写入前一律备份原文件到 `~/.wb-switch/agent-backups/<target>/<时间戳>/`；
//!   - 只覆盖托管字段，保留用户其余配置（尤其是 MCP / 主题 / 项目信任列表 / 插件）；
//!   - 生成 `manifest.json`，可随时一键安全回滚至修改前状态；
//!   - 支持模型多选：配置时将用户选中的所有模型同步注入客户端配置。

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::modules::config::{atomic_write, home_dir, now_ms, store_dir};

/// 受支持的全部 11 类客户端标识（与 UI 列表顺序严格对齐）。
pub const TARGETS: [&str; 11] = [
    "claude-code",
    "claude-desktop",
    "codex",
    "dsh",
    "opencode",
    "pi",
    "grok-build",
    "zcode",
    "kimi-code",
    "openclaw",
    "hermes",
];

/// 托管配置在客户端侧使用的提供方名称。
const PROVIDER_NAME: &str = "WorkBuddy Switch";
/// Codex 的 provider key（必须是合法 TOML 表名）。
const CODEX_PROVIDER_KEY: &str = "workbuddy";
/// DSH 的 provider key。
const DSH_PROVIDER_KEY: &str = "workbuddy";
/// DSH 凭据引用的环境变量名。
const DSH_CREDENTIAL_KEY: &str = "WORKBUDDY_API_KEY";
/// Claude Desktop 使用的 3P profile id（固定值，便于幂等覆写）。
///
/// 必须使用本应用自己的 id，不能复用 CC Switch / EasyCLIProxyAPI 等其他工具
/// 已有的 profile：`configLibrary` 下每个 profile 是独立文件，`_meta.json`
/// 按 id 管理条目，复用会造成两个工具互相覆盖同一份配置。
const CLAUDE_DESKTOP_PROFILE_ID: &str = "8f2d4b6a-1c3e-4a5d-9b7f-2e6c8d0a4f13";

/// Claude Code / Claude Desktop 的模型槽位数量。
///
/// 两个客户端的模型选择器都是固定的槽位结构：
///   Claude Code    Sonnet / Opus / Haiku / Fable（四个环境变量槽位）
///   Claude Desktop Sonnet / Opus / Haiku / Fable（profile 内四个 inferenceModels 条目）
/// 因此「一键接入」时最多写入四个模型，多余的按顺序忽略。
const CLAUDE_SLOT_LIMIT: usize = 4;

/// 备份根目录。
pub fn backup_root() -> PathBuf {
    store_dir().join("agent-backups")
}

// ---------------------------------------------------------------------------
// 路径解析
// ---------------------------------------------------------------------------

/// DSH 主目录：尊重 `DSH_HOME`，否则 `~/.dsh`。
pub fn dsh_home() -> PathBuf {
    std::env::var_os("DSH_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".dsh"))
}

/// Codex 主目录：尊重 `CODEX_HOME`，否则 `~/.codex`。
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".codex"))
}

/// Claude Code 主目录：尊重 `CLAUDE_CONFIG_DIR`，否则 `~/.claude`。
pub fn claude_code_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".claude"))
}

/// Windows 的 LocalAppData 目录。
#[cfg(windows)]
fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
}

/// 非 Windows 平台（当前项目仅发布 Windows，保留可编译路径）。
#[cfg(not(windows))]
fn local_app_data() -> PathBuf {
    home_dir().join(".local").join("share")
}

/// Claude Desktop 的普通配置目录（1P/3P 切换开关所在）。
pub fn claude_desktop_dir() -> PathBuf {
    pick_claude_dir(&local_app_data(), false).unwrap_or_else(|| local_app_data().join("Claude"))
}

/// Claude Desktop 的 3P 配置目录（第三方网关 profile 所在）。
pub fn claude_desktop_3p_dir() -> PathBuf {
    pick_claude_dir(&local_app_data(), true).unwrap_or_else(|| local_app_data().join("Claude-3p"))
}

/// 在 LocalAppData 下挑选 Claude 目录；容忍带版本后缀的目录名（如 `Claude-3p-1.2.3`）。
fn pick_claude_dir(root: &Path, threep: bool) -> Option<PathBuf> {
    let exact_name = if threep { "Claude-3p" } else { "Claude" };
    let exact = root.join(exact_name);
    if exact.exists() {
        return Some(exact);
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                return false;
            };
            name.starts_with("Claude") && name.contains("-3p") == threep
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Claude Desktop 的 configLibrary 目录。
pub fn claude_desktop_library_dir() -> PathBuf {
    claude_desktop_3p_dir().join("configLibrary")
}

/// OpenCode 配置文件路径：优先 `OPENCODE_CONFIG`，否则 `~/.config/opencode/opencode.json`。
pub fn opencode_config_path() -> PathBuf {
    std::env::var_os("OPENCODE_CONFIG")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".config").join("opencode").join("opencode.json"))
}

/// Pi 配置文件路径：优先 `PI_CODING_AGENT_DIR`，否则 `~/.pi/agent/models.json`。
pub fn pi_models_path() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(|d| PathBuf::from(d).join("models.json"))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".pi").join("agent").join("models.json"))
}

/// Grok Build 目录：优先 `GROK_HOME`，否则 `~/.grok`。
pub fn grok_home() -> PathBuf {
    std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".grok"))
}

/// ZCode 配置文件路径：优先 `ZCODE_HOME`，否则 `~/.zcode/v2/config.json`。
pub fn zcode_config_path() -> PathBuf {
    std::env::var_os("ZCODE_HOME")
        .map(|d| PathBuf::from(d).join("v2").join("config.json"))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".zcode").join("v2").join("config.json"))
}

/// Kimi Code 目录：优先 `KIMI_CODE_HOME`，否则 `~/.kimi-code`。
pub fn kimi_code_home() -> PathBuf {
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home_dir().join(".kimi-code"))
}

/// OpenClaw 配置文件路径：优先官方变量 `OPENCLAW_CONFIG_PATH`（文件），
/// 其次 `OPENCLAW_STATE_DIR`（目录）与旧代码使用的 `OPENCLAW_HOME`，
/// 否则 `~/.openclaw/openclaw.json`。
pub fn openclaw_config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("OPENCLAW_CONFIG_PATH")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        return p;
    }
    for var in ["OPENCLAW_STATE_DIR", "OPENCLAW_HOME"] {
        if let Some(d) = std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|d| !d.as_os_str().is_empty())
        {
            return d.join("openclaw.json");
        }
    }
    home_dir().join(".openclaw").join("openclaw.json")
}

/// Hermes 配置文件路径：`$HERMES_HOME/config.yaml`。
///
/// Hermes 安装时会把 `HERMES_HOME` 写进用户环境变量并指向其数据目录，
/// 配置文件就在该目录根部；之前把它当作配置文件本身，写入时会对目录做
/// rename 而被 Windows 拒绝（os error 5）。未设置时回退 `~/.hermes/config.yaml`。
pub fn hermes_config_path() -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("config.yaml"))
        .unwrap_or_else(|| home_dir().join(".hermes").join("config.yaml"))
}

// ---------------------------------------------------------------------------
// 检测
// ---------------------------------------------------------------------------

/// 单个客户端的检测结果。
#[derive(Debug, Clone)]
pub struct TargetStatus {
    pub id: &'static str,
    pub label: &'static str,
    /// 是否检测到已安装（配置文件或客户端目录存在）。
    pub installed: bool,
    /// 是否已接入本网关（托管字段与本机网关地址一致）。
    pub configured: bool,
    /// 主要配置文件的绝对路径。
    pub config_path: String,
    /// 面向用户的补充说明（警告 / 未安装原因）。
    pub note: String,
    /// 客户端检测到的版本号（如 2.1.233, 1.17.11 等）。
    pub version: Option<String>,
}

fn probe_cmd_version(exe: &str, args: &[&str]) -> Option<String> {
    use std::process::Command;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let out = cmd.output().ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// 探测全部 11 类目标客户端的安装与配置状态。
pub fn detect_all(gateway_base: &str, api_key: &str) -> Vec<TargetStatus> {
    vec![
        detect_claude_code(gateway_base, api_key),
        detect_claude_desktop(gateway_base, api_key),
        detect_codex(gateway_base, api_key),
        detect_dsh(gateway_base, api_key),
        detect_opencode(gateway_base, api_key),
        detect_pi(gateway_base, api_key),
        detect_grok_build(gateway_base, api_key),
        detect_zcode(gateway_base, api_key),
        detect_kimi_code(gateway_base, api_key),
        detect_openclaw(gateway_base, api_key),
        detect_hermes(gateway_base, api_key),
    ]
}

fn detect_claude_code(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = claude_code_home().join("settings.json");
    let installed = claude_code_home().is_dir() || path.is_file();

    let mut configured = false;
    if let Some(v) = read_json(&path) {
        configured = env_value(&v, "ANTHROPIC_BASE_URL").as_deref() == Some(gateway_base)
            && env_value(&v, "ANTHROPIC_AUTH_TOKEN").as_deref() == Some(api_key);
    }

    let version = if installed {
        probe_cmd_version("claude.cmd", &["--version"])
            .or_else(|| probe_cmd_version("claude", &["--version"]))
    } else {
        None
    };

    TargetStatus {
        id: "claude-code",
        label: "Claude Code",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Claude Code（~/.claude 不存在）".to_string()
        },
        version,
    }
}

fn detect_claude_desktop(gateway_base: &str, api_key: &str) -> TargetStatus {
    let dir = claude_desktop_3p_dir();
    let profile = claude_desktop_library_dir().join(format!("{CLAUDE_DESKTOP_PROFILE_ID}.json"));
    let installed = claude_desktop_dir().is_dir() || dir.is_dir();

    let mut configured = false;
    if let Some(v) = read_json(&profile) {
        configured = v.get("inferenceGatewayBaseUrl").and_then(Value::as_str) == Some(gateway_base)
            && v.get("inferenceGatewayApiKey").and_then(Value::as_str) == Some(api_key);
    }

    TargetStatus {
        id: "claude-desktop",
        label: "Claude Desktop",
        installed,
        configured,
        config_path: profile.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Claude Desktop".to_string()
        },
        version: if installed { Some("Claude Desktop".to_string()) } else { None },
    }
}

fn detect_codex(gateway_base: &str, api_key: &str) -> TargetStatus {
    let config = codex_home().join("config.toml");
    let auth = codex_home().join("auth.json");
    let installed = codex_home().is_dir() || config.is_file();

    let mut configured = false;
    if let Ok(text) = std::fs::read_to_string(&config) {
        let base_ok = text.contains(gateway_base);
        let key_ok = read_json(&auth)
            .and_then(|v| {
                v.get("OPENAI_API_KEY")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .map(|k| k == api_key)
            .unwrap_or(false);
        configured = base_ok && key_ok;
    }

    let version = if installed {
        probe_cmd_version("codex.cmd", &["--version"])
            .or_else(|| probe_cmd_version("codex", &["--version"]))
    } else {
        None
    };

    TargetStatus {
        id: "codex",
        label: "Codex",
        installed,
        configured,
        config_path: config.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Codex（~/.codex 不存在）".to_string()
        },
        version,
    }
}

fn detect_dsh(gateway_base: &str, api_key: &str) -> TargetStatus {
    let settings = dsh_home().join("settings.yaml");
    let credentials = dsh_home().join(".credentials.yaml");
    let installed = dsh_home().is_dir() || settings.is_file();

    let mut configured = false;
    let mut note = String::new();
    if let Ok(text) = std::fs::read_to_string(&settings) {
        let base_ok = text.contains(&format!("{DSH_PROVIDER_KEY}:"))
            && text.contains(gateway_base);
        let key_ok = std::fs::read_to_string(&credentials)
            .map(|c| c.contains(api_key) || c.contains(DSH_CREDENTIAL_KEY))
            .unwrap_or(false);
        configured = base_ok && key_ok;
    }
    if !installed {
        note = "未检测到 DSH（~/.dsh 不存在）".to_string();
    } else if !settings.is_file() {
        note = "检测到 DSH 目录，但尚无 settings.yaml（导入时会自动创建）".to_string();
    }

    TargetStatus {
        id: "dsh",
        label: "DeepSeek Harness",
        installed,
        configured,
        config_path: settings.to_string_lossy().to_string(),
        note,
        version: if installed { Some("DeepSeek Harness".to_string()) } else { None },
    }
}

fn detect_opencode(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = opencode_config_path();
    let app_dir = home_dir().join(".config").join("opencode");
    let dot_dir = home_dir().join(".opencode");
    let desktop_exe = local_app_data().join("Programs").join("@opencode-aidesktop").join("OpenCode.exe");
    let installed = path.is_file() || app_dir.is_dir() || dot_dir.is_dir() || desktop_exe.is_file();

    let mut configured = false;
    if let Some(v) = read_json(&path) {
        if let Some(provider) = v.get("provider").and_then(Value::as_object) {
            if let Some(wb) = provider.get("workbuddy").and_then(Value::as_object) {
                let opt_base = wb.get("options").and_then(|o| o.get("baseURL")).and_then(Value::as_str);
                let opt_key = wb.get("options").and_then(|o| o.get("apiKey")).and_then(Value::as_str);
                configured = opt_base.map(|b| b.contains(gateway_base)).unwrap_or(false)
                    && (api_key.is_empty() || opt_key == Some(api_key));
            }
        }
    }

    let version = if installed {
        probe_cmd_version("opencode.cmd", &["-v"])
            .or_else(|| probe_cmd_version("opencode", &["-v"]))
    } else {
        None
    };

    TargetStatus {
        id: "opencode",
        label: "OpenCode",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 OpenCode（~/.config/opencode 不存在）".to_string()
        },
        version,
    }
}

fn detect_pi(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = pi_models_path();
    let agent_dir = home_dir().join(".pi").join("agent");
    let dot_dir = home_dir().join(".pi");
    let installed = path.is_file() || agent_dir.is_dir() || dot_dir.is_dir();

    let mut configured = false;
    if let Some(v) = read_json(&path) {
        if let Some(providers) = v.get("providers").and_then(Value::as_object) {
            if let Some(wb) = providers.get("workbuddy").and_then(Value::as_object) {
                let b = wb.get("baseUrl").and_then(Value::as_str);
                let k = wb.get("apiKey").and_then(Value::as_str);
                configured = b.map(|s| s.contains(gateway_base)).unwrap_or(false)
                    && (api_key.is_empty() || k == Some(api_key));
            }
        }
    }

    TargetStatus {
        id: "pi",
        label: "Pi",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Pi（~/.pi 不存在）".to_string()
        },
        version: if installed { Some("Pi".to_string()) } else { None },
    }
}

fn detect_grok_build(gateway_base: &str, api_key: &str) -> TargetStatus {
    let home = grok_home();
    let config = home.join("config.toml");
    let bin = home.join("bin").join("grok.exe");
    let installed = home.is_dir() || config.is_file() || bin.is_file();

    let mut configured = false;
    if let Ok(text) = std::fs::read_to_string(&config) {
        configured = text.contains(gateway_base) && (api_key.is_empty() || text.contains(api_key));
    }

    let version = read_json(&grok_home().join("version.json"))
        .and_then(|v| v.get("version").and_then(Value::as_str).map(|s| format!("grok {s}")));

    TargetStatus {
        id: "grok-build",
        label: "Grok Build",
        installed,
        configured,
        config_path: config.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Grok Build（~/.grok 不存在）".to_string()
        },
        version,
    }
}

fn detect_zcode(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = zcode_config_path();
    let home = home_dir().join(".zcode");
    let app_exe = local_app_data().join("Programs").join("ZCode").join("ZCode.exe");
    let installed = path.is_file() || home.is_dir() || app_exe.is_file();

    let mut configured = false;
    if let Some(v) = read_json(&path) {
        if let Some(provider) = v.get("provider").and_then(Value::as_object) {
            if let Some(wb) = provider.get("workbuddy").and_then(Value::as_object) {
                let opt_base = wb.get("options").and_then(|o| o.get("baseURL")).and_then(Value::as_str);
                let opt_key = wb.get("options").and_then(|o| o.get("apiKey")).and_then(Value::as_str);
                configured = opt_base.map(|b| b.contains(gateway_base)).unwrap_or(false)
                    && (api_key.is_empty() || opt_key == Some(api_key));
            }
        }
    }

    TargetStatus {
        id: "zcode",
        label: "ZCode",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 ZCode（~/.zcode 不存在）".to_string()
        },
        version: if installed { Some("ZCode".to_string()) } else { None },
    }
}

fn detect_kimi_code(gateway_base: &str, api_key: &str) -> TargetStatus {
    let home = kimi_code_home();
    let config = home.join("config.toml");
    let bin = home.join("bin").join("kimi.exe");
    let installed = home.is_dir() || config.is_file() || bin.is_file();

    let mut configured = false;
    if let Ok(text) = std::fs::read_to_string(&config) {
        configured = text.contains(gateway_base) && (api_key.is_empty() || text.contains(api_key));
    }

    let version = if installed {
        probe_cmd_version(&bin.to_string_lossy(), &["--version"])
    } else {
        None
    };

    TargetStatus {
        id: "kimi-code",
        label: "Kimi Code",
        installed,
        configured,
        config_path: config.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Kimi Code（~/.kimi-code 不存在）".to_string()
        },
        version,
    }
}

fn detect_openclaw(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = openclaw_config_path();
    let home = home_dir().join(".openclaw");
    let installed = path.is_file() || home.is_dir();

    let mut configured = false;
    if let Ok(text) = std::fs::read_to_string(&path) {
        configured = text.contains("workbuddy") && text.contains(gateway_base) && (api_key.is_empty() || text.contains(api_key));
    }

    TargetStatus {
        id: "openclaw",
        label: "OpenClaw",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 OpenClaw（~/.openclaw 不存在）".to_string()
        },
        version: if installed { Some("OpenClaw".to_string()) } else { None },
    }
}

fn detect_hermes(gateway_base: &str, api_key: &str) -> TargetStatus {
    let path = hermes_config_path();
    let home = home_dir().join(".hermes");
    let installed = path.is_file() || home.is_dir();

    let mut configured = false;
    if let Ok(text) = std::fs::read_to_string(&path) {
        configured = text.contains("workbuddy") && text.contains(gateway_base) && (api_key.is_empty() || text.contains(api_key));
    }

    TargetStatus {
        id: "hermes",
        label: "Hermes Agent",
        installed,
        configured,
        config_path: path.to_string_lossy().to_string(),
        note: if installed {
            String::new()
        } else {
            "未检测到 Hermes Agent（~/.hermes 不存在）".to_string()
        },
        version: if installed { Some("Hermes Agent".to_string()) } else { None },
    }
}

// ---------------------------------------------------------------------------
// 备份与恢复
// ---------------------------------------------------------------------------

/// 备份一组文件到 `<backup_root>/<target>/<ts>/`，返回备份目录。
pub fn backup_files(target: &str, paths: &[PathBuf]) -> Result<PathBuf, String> {
    let stamp = now_ms().to_string();
    let dir = backup_root().join(target).join(&stamp);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建备份目录失败: {e}"))?;

    let mut manifest = Map::new();
    for (index, path) in paths.iter().enumerate() {
        if !path.is_file() {
            continue;
        }
        let name = format!("{index}-{}", path.file_name().unwrap_or_default().to_string_lossy());
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("读取待备份文件失败 {}: {e}", path.display()))?;
        atomic_write(&dir.join(&name), &content)
            .map_err(|e| format!("写入备份失败 {}: {e}", path.display()))?;
        manifest.insert(name, json!(path.to_string_lossy()));
    }
    manifest.insert("target".to_string(), json!(target));
    manifest.insert("createdAt".to_string(), json!(now_ms()));

    let text = serde_json::to_string_pretty(&Value::Object(manifest))
        .map_err(|e| e.to_string())?;
    atomic_write(&dir.join("manifest.json"), &text).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 列出某个客户端的历史备份（按时间倒序），供 UI 展示与恢复。
pub fn list_backups(target: &str) -> Vec<Value> {
    let root = backup_root().join(target);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let created = entry
            .file_name()
            .to_string_lossy()
            .parse::<i64>()
            .unwrap_or(0);
        out.push(json!({
            "id": entry.file_name().to_string_lossy(),
            "createdAt": created,
            "path": path.to_string_lossy(),
        }));
    }
    out.sort_by(|a, b| {
        b.get("createdAt")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .cmp(&a.get("createdAt").and_then(Value::as_i64).unwrap_or(0))
    });
    out
}

/// 从指定备份恢复文件。
pub fn restore_backup(target: &str, backup_id: &str) -> Result<usize, String> {
    let dir = backup_root().join(target).join(backup_id);
    let manifest_path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("读取备份清单失败: {e}"))?;
    let manifest: Value = serde_json::from_str(&text).map_err(|e| format!("备份清单损坏: {e}"))?;
    let obj = manifest
        .as_object()
        .ok_or_else(|| "备份清单格式错误".to_string())?;

    let mut restored = 0usize;
    for (name, original) in obj {
        if name == "target" || name == "createdAt" {
            continue;
        }
        let Some(target_path_str) = original.as_str() else {
            continue;
        };
        let backup_file = dir.join(name);
        if !backup_file.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&backup_file)
            .map_err(|e| format!("读取备份文件失败: {e}"))?;
        let target_path = PathBuf::from(target_path_str);
        if let Some(parent) = target_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        atomic_write(&target_path, &content)
            .map_err(|e| format!("恢复文件失败 {target_path_str}: {e}"))?;
        restored += 1;
    }
    Ok(restored)
}

// ---------------------------------------------------------------------------
// 导入与批量更新分发
// ---------------------------------------------------------------------------

/// 导入操作的执行结果。
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub target: String,
    pub backup_dir: String,
    pub files: Vec<String>,
    pub models: Vec<String>,
}

/// 把网关配置接入指定目标客户端（支持多模型）。
pub fn import_target(
    target: &str,
    gateway_base: &str,
    api_key: &str,
    models: &[String],
) -> Result<ImportOutcome, String> {
    if api_key.trim().is_empty() {
        return Err("请先在网关设置里填写 API Key（客户端必须携带凭据）".to_string());
    }
    let base = gateway_base.trim_end_matches('/');
    let safe_models = if models.is_empty() {
        vec!["deepseek-v4-flash".to_string()]
    } else {
        models.to_vec()
    };

    match target {
        "claude-code" => import_claude_code(base, api_key, &safe_models),
        "claude-desktop" => import_claude_desktop(base, api_key, &safe_models),
        "codex" => import_codex(base, api_key, &safe_models),
        "dsh" => import_dsh(base, api_key, &safe_models),
        "opencode" => import_opencode(base, api_key, &safe_models),
        "pi" => import_pi(base, api_key, &safe_models),
        "grok-build" => import_grok_build(base, api_key, &safe_models),
        "zcode" => import_zcode(base, api_key, &safe_models),
        "kimi-code" => import_kimi_code(base, api_key, &safe_models),
        "openclaw" => import_openclaw(base, api_key, &safe_models),
        "hermes" => import_hermes(base, api_key, &safe_models),
        other => Err(format!("不支持的客户端: {other}")),
    }
}

/// 针对指定 targets 列表批量导入/更新。
pub fn import_targets(
    target_ids: &[String],
    gateway_base: &str,
    api_key: &str,
    models: &[String],
) -> Result<Vec<ImportOutcome>, String> {
    let mut outcomes = Vec::new();
    for id in target_ids {
        let outcome = import_target(id, gateway_base, api_key, models)?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// 一键接入/更新所有检测到已安装的客户端。
pub fn import_all_installed(
    gateway_base: &str,
    api_key: &str,
    models: &[String],
) -> Result<Vec<ImportOutcome>, String> {
    let targets = detect_all(gateway_base, api_key);
    let mut outcomes = Vec::new();
    for t in targets {
        if t.installed {
            let outcome = import_target(t.id, gateway_base, api_key, models)?;
            outcomes.push(outcome);
        }
    }
    Ok(outcomes)
}

fn import_claude_code(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = claude_code_home().join("settings.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 Claude 配置目录失败: {e}"))?;
    }

    // Claude Code 只有 Sonnet/Opus/Haiku/Fable 四个槽位，多余模型无法承载，
    // 明确截断并如实回传实际写入的模型列表（避免 UI 报告未生效的数量）。
    let slot_models: Vec<String> = models.iter().take(CLAUDE_SLOT_LIMIT).cloned().collect();

    let backup = backup_files("claude-code", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_claude_code_settings(existing.as_deref(), base, api_key, &slot_models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 Claude Code 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "claude-code".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: slot_models,
    })
}

fn import_claude_desktop(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let dir = claude_desktop_3p_dir();
    let library = dir.join("configLibrary");
    std::fs::create_dir_all(&library).map_err(|e| format!("创建 Claude 3P 目录失败: {e}"))?;

    // 与 Claude Code 相同：profile 只有四个槽位，超出部分截断并如实回传。
    let slot_models: Vec<String> = models.iter().take(CLAUDE_SLOT_LIMIT).cloned().collect();

    let normal_config = claude_desktop_dir().join("claude_desktop_config.json");
    let threep_config = dir.join("claude_desktop_config.json");
    let profile = library.join(format!("{CLAUDE_DESKTOP_PROFILE_ID}.json"));
    let meta = library.join("_meta.json");

    let backup = backup_files(
        "claude-desktop",
        &[
            normal_config.clone(),
            threep_config.clone(),
            profile.clone(),
            meta.clone(),
        ],
    )?;

    let prof_existing = std::fs::read_to_string(&profile).ok();
    let prof_text = build_claude_desktop_profile(prof_existing.as_deref(), base, api_key, &slot_models)?;
    atomic_write(&profile, &prof_text).map_err(|e| format!("写入 3P profile 失败: {e}"))?;

    let meta_existing = std::fs::read_to_string(&meta).ok();
    let meta_text = build_claude_desktop_meta(meta_existing.as_deref())?;
    atomic_write(&meta, &meta_text).map_err(|e| format!("写入 3P _meta.json 失败: {e}"))?;

    let mode_normal = std::fs::read_to_string(&normal_config).ok();
    let mode_text = build_claude_desktop_mode(mode_normal.as_deref())?;
    if let Some(parent) = normal_config.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    atomic_write(&normal_config, &mode_text).map_err(|e| format!("写入普通配置模式失败: {e}"))?;
    atomic_write(&threep_config, &mode_text).map_err(|e| format!("写入 3P 配置模式失败: {e}"))?;

    Ok(ImportOutcome {
        target: "claude-desktop".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![
            profile.to_string_lossy().to_string(),
            meta.to_string_lossy().to_string(),
            normal_config.to_string_lossy().to_string(),
        ],
        models: slot_models,
    })
}

fn import_codex(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let home = codex_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 Codex 目录失败: {e}"))?;
    let config_path = home.join("config.toml");
    let auth_path = home.join("auth.json");

    let backup = backup_files("codex", &[config_path.clone(), auth_path.clone()])?;

    let existing_cfg = std::fs::read_to_string(&config_path).ok();
    let cfg_text = build_codex_config(existing_cfg.as_deref(), base, models)?;
    atomic_write(&config_path, &cfg_text).map_err(|e| format!("写入 Codex config 失败: {e}"))?;

    let existing_auth = std::fs::read_to_string(&auth_path).ok();
    let auth_text = build_codex_auth(existing_auth.as_deref(), api_key)?;
    atomic_write(&auth_path, &auth_text).map_err(|e| format!("写入 Codex auth 失败: {e}"))?;

    Ok(ImportOutcome {
        target: "codex".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![
            config_path.to_string_lossy().to_string(),
            auth_path.to_string_lossy().to_string(),
        ],
        models: models.to_vec(),
    })
}

fn import_dsh(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let home = dsh_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 DSH 目录失败: {e}"))?;
    let settings_path = home.join("settings.yaml");
    let credentials_path = home.join(".credentials.yaml");

    let backup = backup_files("dsh", &[settings_path.clone(), credentials_path.clone()])?;

    let existing = std::fs::read_to_string(&settings_path).ok();
    let settings = build_dsh_settings(existing.as_deref(), base, models)?;
    atomic_write(&settings_path, &settings).map_err(|e| format!("写入 DSH settings 失败: {e}"))?;

    let creds_existing = std::fs::read_to_string(&credentials_path).ok();
    let creds = build_dsh_credentials(creds_existing.as_deref(), api_key)?;
    atomic_write(&credentials_path, &creds)
        .map_err(|e| format!("写入 DSH 凭据失败: {e}"))?;

    Ok(ImportOutcome {
        target: "dsh".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![
            settings_path.to_string_lossy().to_string(),
            credentials_path.to_string_lossy().to_string(),
        ],
        models: models.to_vec(),
    })
}

fn import_opencode(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = opencode_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 OpenCode 目录失败: {e}"))?;
    }
    let backup = backup_files("opencode", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_opencode_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 OpenCode 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "opencode".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_pi(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = pi_models_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 Pi 目录失败: {e}"))?;
    }
    let backup = backup_files("pi", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_pi_models(existing.as_deref(), base, api_key, models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 Pi 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "pi".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_grok_build(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let home = grok_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 Grok 目录失败: {e}"))?;
    let config_path = home.join("config.toml");
    let backup = backup_files("grok-build", &[config_path.clone()])?;
    let existing = std::fs::read_to_string(&config_path).ok();
    let text = build_grok_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&config_path, &text).map_err(|e| format!("写入 Grok Build 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "grok-build".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![config_path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_zcode(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = zcode_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 ZCode 目录失败: {e}"))?;
    }
    let backup = backup_files("zcode", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_zcode_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 ZCode 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "zcode".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_kimi_code(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let home = kimi_code_home();
    std::fs::create_dir_all(&home).map_err(|e| format!("创建 Kimi Code 目录失败: {e}"))?;
    let config_path = home.join("config.toml");
    let backup = backup_files("kimi-code", &[config_path.clone()])?;
    let existing = std::fs::read_to_string(&config_path).ok();
    let text = build_kimi_code_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&config_path, &text).map_err(|e| format!("写入 Kimi Code 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "kimi-code".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![config_path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_openclaw(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = openclaw_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 OpenClaw 目录失败: {e}"))?;
    }
    let backup = backup_files("openclaw", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_openclaw_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 OpenClaw 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "openclaw".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

fn import_hermes(base: &str, api_key: &str, models: &[String]) -> Result<ImportOutcome, String> {
    let path = hermes_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 Hermes 目录失败: {e}"))?;
    }
    let backup = backup_files("hermes", &[path.clone()])?;
    let existing = std::fs::read_to_string(&path).ok();
    let text = build_hermes_config(existing.as_deref(), base, api_key, models)?;
    atomic_write(&path, &text).map_err(|e| format!("写入 Hermes 配置失败: {e}"))?;

    Ok(ImportOutcome {
        target: "hermes".to_string(),
        backup_dir: backup.to_string_lossy().to_string(),
        files: vec![path.to_string_lossy().to_string()],
        models: models.to_vec(),
    })
}

// ---------------------------------------------------------------------------
// 各客户端配置生成器（支持多模型注入）
// ---------------------------------------------------------------------------

/// 生成 DSH settings.yaml：在 llm-pi-ai 插件下注册 workbuddy 路由，并把默认模型指向它。
///
/// DSH 的插件配置以插件 id 为顶层键：模型路由由 `llm-pi-ai.providers` 注册
/// （provider 名即路由名），`agent-default-model` 决定会话实际使用的路由与模型。
/// 写到顶层 `providers` / `default_model` 不会被任何插件读取，会话仍会落到
/// DeepSeek 官方路由并报 MISSING_CREDENTIAL。
pub fn build_dsh_settings(
    existing: Option<&str>,
    base: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let mut root = parse_yaml_map(existing)?;

    let llm = ensure_yaml_map(&mut root, "llm-pi-ai");
    let providers = ensure_yaml_map(llm, "providers");
    let models_val: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m,
                "name": m,
                "reasoningEfforts": { "off": null, "high": "high", "max": "max" }
            })
        })
        .collect();

    providers.insert(
        DSH_PROVIDER_KEY.to_string(),
        json!({
            "api": "openai-completions",
            "baseURL": format!("{base}/v1"),
            "apiKeyEnv": DSH_CREDENTIAL_KEY,
            "models": models_val
        }),
    );

    let default_model = ensure_yaml_map(&mut root, "agent-default-model");
    default_model.insert("provider".to_string(), json!(DSH_PROVIDER_KEY));
    default_model.insert("model".to_string(), json!(primary));

    render_yaml(Value::Object(root))
}

/// 生成 DSH .credentials.yaml：写入 API Key 引用。
pub fn build_dsh_credentials(existing: Option<&str>, api_key: &str) -> Result<String, String> {
    let mut root = parse_yaml_map(existing)?;
    if !root.contains_key("version") {
        root.insert("version".to_string(), json!(1));
    }
    let refs = ensure_yaml_map(&mut root, "refs");
    refs.insert(DSH_CREDENTIAL_KEY.to_string(), json!(api_key));
    render_yaml(Value::Object(root))
}

/// 生成 Claude Code settings.json：覆盖 env 块内的网关环境变量与模型槽位映射。
///
/// Claude Code 的模型选择器由四个别名槽位构成（Sonnet / Opus / Haiku / Fable），
/// `ANTHROPIC_DEFAULT_<槽位>_MODEL` 决定该槽位实际发给服务端的模型名，
/// `_MODEL_NAME` 是 /model 菜单里的显示名。
///
/// 这里直接写入上游真实模型名（对第三方网关是官方推荐用法），Claude Code 会
/// 原样把真实模型名作为请求的 `model` 发出，网关无需任何转译即可转发；
/// 用户若在 /model 里选择了内置的 Claude 型号（如 claude-opus-5），网关侧按
/// 名字中的槽位关键词兜底映射到对应槽位写入的模型。
pub fn build_claude_code_settings(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let opus = models.get(1).map(String::as_str).unwrap_or(primary);
    let haiku = models.get(2).map(String::as_str).unwrap_or(primary);
    let fable = models.get(3).map(String::as_str).unwrap_or(primary);

    let mut root = parse_json_object(existing)?;
    if !root.get("env").map(Value::is_object).unwrap_or(false) {
        root.insert("env".into(), json!({}));
    }
    let env = root
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "Claude Code 配置的 env 字段必须是对象".to_string())?;

    env.insert("ANTHROPIC_BASE_URL".into(), json!(base));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(api_key));

    // 四个槽位（含 Fable）都写真实模型名与同名显示名：
    // 显示名 = 模型名，菜单里看到什么就发什么，不留虚拟名与真实名的映射歧义。
    let slots = [
        ("SONNET", primary),
        ("OPUS", opus),
        ("HAIKU", haiku),
        ("FABLE", fable),
    ];
    for (slot, model) in slots {
        env.insert(format!("ANTHROPIC_DEFAULT_{slot}_MODEL"), json!(model));
        env.insert(format!("ANTHROPIC_DEFAULT_{slot}_MODEL_NAME"), json!(model));
    }

    render_json(Value::Object(root))
}

/// 生成 Claude Desktop 的 3P gateway profile：以 Claude 槽位名为主键，将上游模型写入 labelOverride。
///
/// Claude Desktop 的模型菜单只认它内置的四个槽位名（claude-sonnet-5 /
/// claude-opus-5 / claude-haiku-4-5 / claude-fable-5），请求时把槽位名作为
/// `model` 发出，因此无法像 Claude Code 那样直写真实模型名；真实模型写入
/// `labelOverride` 用于菜单展示，网关侧按槽位名反查翻译成上游模型。
pub fn build_claude_desktop_profile(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let sonnet = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let opus = models.get(1).map(String::as_str).unwrap_or(sonnet);
    let haiku = models.get(2).map(String::as_str).unwrap_or(sonnet);
    let fable = models.get(3).map(String::as_str).unwrap_or(sonnet);

    let list = vec![
        json!({
            "name": "claude-sonnet-5",
            "labelOverride": sonnet,
            "supports1m": true
        }),
        json!({
            "name": "claude-opus-5",
            "labelOverride": opus,
            "supports1m": true
        }),
        json!({
            "name": "claude-haiku-4-5",
            "labelOverride": haiku,
            "supports1m": true
        }),
        json!({
            "name": "claude-fable-5",
            "labelOverride": fable,
            "supports1m": true
        }),
    ];

    let mut root = parse_json_object(existing)?;
    root.insert("coworkEgressAllowedHosts".into(), json!(["*"]));
    root.insert("disableDeploymentModeChooser".into(), json!(true));
    root.insert("inferenceGatewayApiKey".into(), json!(api_key));
    root.insert("inferenceGatewayAuthScheme".into(), json!("bearer"));
    root.insert("inferenceGatewayBaseUrl".into(), json!(base));
    root.insert("inferenceProvider".into(), json!("gateway"));
    root.insert("inferenceModels".into(), Value::Array(list));
    render_json(Value::Object(root))
}

/// 生成 Claude Desktop 的 1P/3P 模式开关文件。
pub fn build_claude_desktop_mode(existing: Option<&str>) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    root.insert("deploymentMode".into(), json!("3p"));
    render_json(Value::Object(root))
}

/// 生成 Claude Desktop 的 _meta.json 索引文件。
pub fn build_claude_desktop_meta(existing: Option<&str>) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    let mut retained: Vec<Value> = root
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter(|e| {
                    e.get("id").and_then(Value::as_str) != Some(CLAUDE_DESKTOP_PROFILE_ID)
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    retained.push(json!({ "id": CLAUDE_DESKTOP_PROFILE_ID, "name": PROVIDER_NAME }));
    root.insert("entries".into(), Value::Array(retained));
    root.insert("appliedId".into(), json!(CLAUDE_DESKTOP_PROFILE_ID));
    render_json(Value::Object(root))
}

/// 生成 Codex config.toml：覆盖 provider 表与顶层 model。
pub fn build_codex_config(existing: Option<&str>, base: &str, models: &[String]) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let mut text = existing.unwrap_or("").to_string();

    text = set_toml_top_level(&text, "model_provider", &format!("\"{CODEX_PROVIDER_KEY}\""));
    text = set_toml_top_level(&text, "model", &format!("\"{primary}\""));
    text = set_toml_top_level(&text, "model_reasoning_effort", "\"high\"");
    text = set_toml_top_level(&text, "disable_response_storage", "true");

    let table = format!("[model_providers.{CODEX_PROVIDER_KEY}]");
    let body = format!(
        "{table}\nname = \"{PROVIDER_NAME}\"\nbase_url = \"{base}/v1\"\nwire_api = \"responses\"\nrequires_openai_auth = true\n"
    );
    text = replace_toml_table(&text, &table, &body);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// 生成 Codex auth.json：写入 OpenAI 形态的 API Key。
pub fn build_codex_auth(existing: Option<&str>, api_key: &str) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    root.insert("OPENAI_API_KEY".into(), json!(api_key));
    root.remove("tokens");
    root.remove("last_refresh");
    root.insert("auth_mode".into(), json!("apikey"));
    render_json(Value::Object(root))
}

/// 生成 OpenCode 配置：写入 provider.workbuddy 及多个选定模型。
pub fn build_opencode_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    if !root.contains_key("$schema") {
        root.insert("$schema".into(), json!("https://opencode.ai/config.json"));
    }
    if !root.get("provider").map(Value::is_object).unwrap_or(false) {
        root.insert("provider".into(), json!({}));
    }
    let provider = root.get_mut("provider").and_then(Value::as_object_mut).unwrap();

    let mut models_map = Map::new();
    for m in models {
        models_map.insert(m.clone(), json!({ "name": m }));
    }

    provider.insert(
        "workbuddy".into(),
        json!({
            "name": PROVIDER_NAME,
            "npm": "@ai-sdk/openai-compatible",
            "options": {
                "baseURL": format!("{base}/v1"),
                "apiKey": api_key,
                "setCacheKey": true
            },
            "models": models_map
        }),
    );
    render_json(Value::Object(root))
}

/// 生成 Pi 配置：写入 providers.workbuddy 及其支持的模型列表。
pub fn build_pi_models(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    if !root.get("providers").map(Value::is_object).unwrap_or(false) {
        root.insert("providers".into(), json!({}));
    }
    let providers = root.get_mut("providers").and_then(Value::as_object_mut).unwrap();
    let models_list: Vec<Value> = models
        .iter()
        .map(|m| json!({ "id": m, "reasoning": true }))
        .collect();

    providers.insert(
        "workbuddy".into(),
        json!({
            "api": "openai-completions",
            "baseUrl": format!("{base}/v1"),
            "apiKey": api_key,
            "models": models_list
        }),
    );
    render_json(Value::Object(root))
}

/// 生成 Grok Build config.toml：写入 default 模型及为每个选中模型生成配置表。
pub fn build_grok_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let text = existing.unwrap_or("");
    let mut out = if text.trim().is_empty() {
        format!("[models]\ndefault = \"{primary}\"\n")
    } else {
        let models_table = format!("[models]\ndefault = \"{primary}\"\n");
        replace_toml_table(text, "[models]", &models_table)
    };

    for m in models {
        let table_header = format!("[model.\"{m}\"]");
        let model_body = format!(
            "{table_header}\nmodel = \"{m}\"\nbase_url = \"{base}/v1\"\nname = \"{PROVIDER_NAME}\"\napi_key = \"{api_key}\"\napi_backend = \"responses\"\ncontext_window = 500000\n"
        );
        out = replace_toml_table(&out, &table_header, &model_body);
    }
    Ok(out)
}

/// 生成 ZCode 配置：写入 provider.workbuddy 及多个选定模型。
pub fn build_zcode_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let mut root = parse_json_object(existing)?;
    if !root.get("provider").map(Value::is_object).unwrap_or(false) {
        root.insert("provider".into(), json!({}));
    }
    let provider = root.get_mut("provider").and_then(Value::as_object_mut).unwrap();

    let mut models_map = Map::new();
    for m in models {
        models_map.insert(
            m.clone(),
            json!({
                "reasoning": {
                    "enabled": true,
                    "variants": ["low", "medium", "high", "max"],
                    "defaultVariant": "max"
                },
                "limit": {
                    "context": 200000,
                    "output": 8192
                },
                "modalities": {
                    "input": ["text"],
                    "output": ["text"]
                }
            }),
        );
    }

    provider.insert(
        "workbuddy".into(),
        json!({
            "name": PROVIDER_NAME,
            // ZCode 的 kind 枚举只有 "anthropic" 与 "openai-compatible"，
            // 该值决定请求走 /v1/messages 还是 /v1/chat/completions。
            "kind": "openai-compatible",
            "options": {
                "baseURL": format!("{base}/v1"),
                "apiKey": api_key
            },
            "enabled": true,
            "source": "custom",
            "models": models_map
        }),
    );
    render_json(Value::Object(root))
}

/// 生成 Kimi Code config.toml：写入 provider 表、default_model 与每个选中模型的配置表。
///
/// Kimi Code 的凭证挂在 providers 表上（`type = "openai"` 走 OpenAI Chat 兼容协议），
/// 模型条目只用 `provider` 字段引用该供应商。缺少 provider 字段时整条模型配置会被
/// CLI 判为无效（`kimi doctor` 报 "provider: expected string, received undefined"）。
pub fn build_kimi_code_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let text = existing.unwrap_or("");
    let mut out = set_toml_top_level(text, "default_model", &format!("\"{primary}\""));

    let provider_table = "[providers.workbuddy]";
    let provider_body = format!(
        "{provider_table}\ntype = \"openai\"\nbase_url = \"{base}/v1\"\napi_key = \"{api_key}\"\n"
    );
    out = replace_toml_table(&out, provider_table, &provider_body);

    for m in models {
        let table_header = format!("[models.\"{m}\"]");
        let model_body = format!(
            "{table_header}\nprovider = \"workbuddy\"\nmodel = \"{m}\"\nmax_context_size = 200000\ncapabilities = [\"tool_use\"]\n"
        );
        out = replace_toml_table(&out, &table_header, &model_body);
    }
    Ok(out)
}

/// 生成 OpenClaw 配置：支持标准 JSON 与 JSON5 格式，支持注入多个选中模型。
pub fn build_openclaw_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let mut root = match existing.map(str::trim).filter(|s| !s.is_empty()) {
        None => Map::new(),
        Some(text) => parse_json_or_json5(text)?,
    };

    if !root.get("models").map(Value::is_object).unwrap_or(false) {
        root.insert("models".into(), json!({ "mode": "merge", "providers": {} }));
    }
    let models_block = root.get_mut("models").and_then(Value::as_object_mut).unwrap();
    if !models_block.get("providers").map(Value::is_object).unwrap_or(false) {
        models_block.insert("providers".into(), json!({}));
    }
    let providers = models_block.get_mut("providers").and_then(Value::as_object_mut).unwrap();

    let models_list: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m,
                "name": m,
                "contextWindow": 1048576,
                "reasoning": true,
                "input": ["text", "image"],
                "maxTokens": 131072
            })
        })
        .collect();

    providers.insert(
        "workbuddy".into(),
        json!({
            "baseUrl": format!("{base}/v1"),
            "apiKey": api_key,
            "api": "openai-completions",
            "models": models_list
        }),
    );

    if !root.get("agents").map(Value::is_object).unwrap_or(false) {
        root.insert("agents".into(), json!({ "defaults": {} }));
    }
    let agents = root.get_mut("agents").and_then(Value::as_object_mut).unwrap();
    if !agents.get("defaults").map(Value::is_object).unwrap_or(false) {
        agents.insert("defaults".into(), json!({}));
    }
    let defaults = agents.get_mut("defaults").and_then(Value::as_object_mut).unwrap();
    if !defaults.get("models").map(Value::is_object).unwrap_or(false) {
        defaults.insert("models".into(), json!({}));
    }
    let d_models = defaults.get_mut("models").and_then(Value::as_object_mut).unwrap();
    for m in models {
        let qualified = format!("workbuddy/{m}");
        d_models.insert(qualified, json!({ "alias": "WorkBuddy" }));
    }
    defaults.insert("model".into(), json!({ "primary": format!("workbuddy/{primary}") }));

    render_json(Value::Object(root))
}

/// 生成 Hermes Agent config.yaml：更新 model 默认项并挂载选中的所有模型。
pub fn build_hermes_config(
    existing: Option<&str>,
    base: &str,
    api_key: &str,
    models: &[String],
) -> Result<String, String> {
    let primary = models.first().map(String::as_str).unwrap_or("deepseek-v4-flash");
    let raw = existing.unwrap_or("");

    let mut models_yaml = String::new();
    for m in models {
        models_yaml.push_str(&format!("      {m}:\n        name: {m}\n"));
    }

    let wb_entry = format!(
        "- name: workbuddy\n  base_url: {base}/v1\n  api_key: {api_key}\n  models:\n{models_yaml}  model: {primary}\n"
    );

    if raw.trim().is_empty() {
        return Ok(format!(
            "model:\n  default: {primary}\n  provider: workbuddy\n\ncustom_providers:\n{wb_entry}"
        ));
    }

    let mut text = raw.to_string();

    // 1. 更新或注入 model: 块
    if let Some(pos) = text.find("model:") {
        let after = &text[pos + 6..];
        let mut block_len = 6;
        for line in after.lines() {
            if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('\t') {
                break;
            }
            block_len += line.len() + 1;
        }
        let replacement = format!("model:\n  default: {primary}\n  provider: workbuddy\n");
        text.replace_range(pos..pos + block_len.min(text.len() - pos), &replacement);
    } else {
        text = format!("model:\n  default: {primary}\n  provider: workbuddy\n\n{text}");
    }

    // 2. 更新 custom_providers 列表
    if text.contains("custom_providers:") {
        if let Some(wb_pos) = text.find("- name: workbuddy") {
            let after = &text[wb_pos + 17..];
            let mut entry_len = 17;
            for line in after.lines() {
                if line.trim_start().starts_with("- ")
                    || (!line.is_empty() && !line.starts_with(' ') && !line.starts_with('\t'))
                {
                    break;
                }
                entry_len += line.len() + 1;
            }
            text.replace_range(wb_pos..wb_pos + entry_len.min(text.len() - wb_pos), "");
        }
        if let Some(insert_pos) = text.find("custom_providers:") {
            let after_hdr = insert_pos + "custom_providers:".len();
            let next_nl = text[after_hdr..]
                .find('\n')
                .map(|p| after_hdr + p + 1)
                .unwrap_or(text.len());
            text.insert_str(next_nl, &wb_entry);
        }
    } else {
        text.push_str(&format!("\ncustom_providers:\n{wb_entry}"));
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_json_or_json5(&text).ok().map(Value::Object)
}

fn env_value(root: &Value, key: &str) -> Option<String> {
    root.get("env")
        .and_then(|e| e.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn parse_json_object(existing: Option<&str>) -> Result<Map<String, Value>, String> {
    match existing.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(Map::new()),
        Some(text) => parse_json_or_json5(text),
    }
}

/// 支持宽松 JSON5 风格（支持单引号、无引号属性名、单行注释、尾随逗号）。
fn parse_json_or_json5(text: &str) -> Result<Map<String, Value>, String> {
    if let Ok(Value::Object(map)) = serde_json::from_str(text) {
        return Ok(map);
    }

    let mut out = String::with_capacity(text.len() + 32);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut in_string = false;
    let mut string_quote = '"';
    let mut escaped = false;

    while i < n {
        let c = chars[i];
        if in_string {
            if escaped {
                out.push(c);
                escaped = false;
            } else if c == '\\' {
                out.push(c);
                escaped = true;
            } else if c == string_quote {
                out.push('"');
                in_string = false;
            } else {
                out.push(c);
            }
            i += 1;
            continue;
        }

        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        if c == '"' || c == '\'' {
            in_string = true;
            string_quote = c;
            out.push('"');
            i += 1;
            continue;
        }

        if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-' || chars[i] == '$') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            let mut peek = i;
            while peek < n && chars[peek].is_whitespace() {
                peek += 1;
            }
            if peek < n && chars[peek] == ':' {
                out.push('"');
                out.push_str(&ident);
                out.push('"');
            } else {
                out.push_str(&ident);
            }
            continue;
        }

        if c == ',' {
            let mut peek = i + 1;
            while peek < n && chars[peek].is_whitespace() {
                peek += 1;
            }
            if peek < n && (chars[peek] == '}' || chars[peek] == ']') {
                i += 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }

    match serde_json::from_str::<Value>(&out) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("配置文件根节点必须是 JSON 对象".to_string()),
        Err(e) => Err(format!("配置文件解析失败: {e}")),
    }
}

fn render_json(value: Value) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|e| format!("序列化 JSON 失败: {e}"))
}

fn parse_yaml_map(existing: Option<&str>) -> Result<Map<String, Value>, String> {
    let Some(text) = existing.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Map::new());
    };

    match crate::modules::yaml_lite::parse_mapping(text) {
        Ok(map) => Ok(map),
        Err(_) => Err("DSH 配置文件解析失败（已备份原文件，未做修改）".to_string()),
    }
}

fn render_yaml(value: Value) -> Result<String, String> {
    crate::modules::yaml_lite::render_mapping(&value)
}

fn ensure_yaml_map<'a>(root: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    if !root.get(key).map(Value::is_object).unwrap_or(false) {
        root.insert(key.to_string(), json!({}));
    }
    root.get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("just inserted object")
}

/// 设置 TOML 顶层标量（存在则替换，不存在则插入到首个 `[table]` 之前）。
fn set_toml_top_level(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let prefix = format!("{key} ");
    let prefix_eq = format!("{key}=");

    if let Some(index) = lines.iter().position(|l| {
        let t = l.trim_start();
        !t.starts_with('#') && (t.starts_with(&prefix) || t.starts_with(&prefix_eq))
    }) {
        lines[index] = format!("{key} = {value}");
        return lines.join("\n");
    }

    let insert_at = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    lines.insert(insert_at, format!("{key} = {value}"));
    lines.join("\n")
}

/// 替换整个 TOML 表（从 `[table]` 到下一个顶层 `[` 之前）。
fn replace_toml_table(text: &str, table: &str, body: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.trim() == table) else {
        let mut out = text.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(body.trim_end());
        return out;
    };

    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let t = line.trim();
        if t.starts_with('[') {
            end = i;
            break;
        }
    }

    let mut out = String::new();
    for line in &lines[..start] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(body.trim_end());
    out.push('\n');
    for line in &lines[end..] {
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_config_uses_responses_and_keeps_other_tables() {
        let existing = "model = \"old\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n";
        let out = build_codex_config(Some(existing), "http://127.0.0.1:7863", &["glm-5.2".into()]).unwrap();
        assert!(out.contains("wire_api = \"responses\""), "must use responses:\n{out}");
        assert!(!out.contains("wire_api = \"chat\""));
        assert!(out.contains("base_url = \"http://127.0.0.1:7863/v1\""));
        assert!(out.contains("model = \"glm-5.2\""));
        assert!(out.contains("[mcp_servers.foo]"), "must keep unrelated tables:\n{out}");
    }

    #[test]
    fn codex_config_is_idempotent() {
        let models = vec!["m".to_string()];
        let first = build_codex_config(None, "http://127.0.0.1:7863", &models).unwrap();
        let second = build_codex_config(Some(&first), "http://127.0.0.1:7863", &models).unwrap();
        assert_eq!(first.matches("[model_providers.workbuddy]").count(), 1);
        assert_eq!(second.matches("[model_providers.workbuddy]").count(), 1);
    }

    #[test]
    fn codex_auth_drops_oauth_fields() {
        let existing = r#"{"tokens":{"a":1},"last_refresh":"x","OPENAI_API_KEY":"old"}"#;
        let out = build_codex_auth(Some(existing), "sk-new").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["OPENAI_API_KEY"], "sk-new");
        assert_eq!(v["auth_mode"], "apikey");
        assert!(v.get("tokens").is_none());
    }

    #[test]
    fn claude_code_keeps_unrelated_settings_and_maps_models() {
        let existing = r#"{"permissions":{"allow":["Bash"]},"env":{"OTHER":"1"}}"#;
        let models = vec!["deepseek-v4-flash".to_string(), "glm-5.2".to_string()];
        let out =
            build_claude_code_settings(Some(existing), "http://127.0.0.1:7863", "sk-1", &models)
                .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:7863");
        assert_eq!(v["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-1");
        // 槽位直写真实模型名（含显示名），不再使用 claude-* 虚拟名。
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "deepseek-v4-flash");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"], "deepseek-v4-flash");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "glm-5.2");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL_NAME"], "glm-5.2");
        // 未提供的槽位回退到首个模型，Fable 槽位必须存在。
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-v4-flash");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "deepseek-v4-flash");
        assert_eq!(v["env"]["OTHER"], "1");
        assert_eq!(v["permissions"]["allow"][0], "Bash");
    }

    #[test]
    fn claude_code_maps_four_slots_in_order() {
        let models = vec![
            "m1".to_string(),
            "m2".to_string(),
            "m3".to_string(),
            "m4".to_string(),
        ];
        let out = build_claude_code_settings(None, "http://127.0.0.1:7863", "sk-1", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_SONNET_MODEL"], "m1");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"], "m2");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "m3");
        assert_eq!(v["env"]["ANTHROPIC_DEFAULT_FABLE_MODEL"], "m4");
    }

    #[test]
    fn claude_desktop_fable_slot_uses_fourth_model() {
        let models = vec![
            "m1".to_string(),
            "m2".to_string(),
            "m3".to_string(),
            "m4".to_string(),
        ];
        let out =
            build_claude_desktop_profile(None, "http://127.0.0.1:7863", "sk-1", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let list = v["inferenceModels"].as_array().unwrap();
        let fable = list
            .iter()
            .find(|m| m["name"] == "claude-fable-5")
            .expect("fable slot must exist");
        assert_eq!(fable["labelOverride"], "m4");
    }

    #[test]
    fn claude_desktop_meta_replaces_managed_entry() {
        let existing = format!(
            r#"{{"entries":[{{"id":"other","name":"X"}},{{"id":"{CLAUDE_DESKTOP_PROFILE_ID}","name":"Old"}}]}}"#
        );
        let out = build_claude_desktop_meta(Some(&existing)).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let entries = v["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "should not duplicate managed entry");
        assert!(entries
            .iter()
            .any(|e| e["id"] == "other" && e["name"] == "X"));
        assert_eq!(v["appliedId"], CLAUDE_DESKTOP_PROFILE_ID);
    }

    #[test]
    fn dsh_settings_registers_pi_ai_route_and_default_model() {
        let models = vec!["glm-5.2".to_string(), "kimi-k2.7".to_string()];
        let out = build_dsh_settings(None, "http://127.0.0.1:7863", &models).unwrap();
        // 路由必须注册在 llm-pi-ai.providers 下、默认模型必须指向 agent-default-model；
        // 写到顶层 providers/default_model 不会被任何插件读取。
        assert!(out.contains("llm-pi-ai:"), "{out}");
        assert!(out.contains("api: openai-completions"), "{out}");
        assert!(out.contains("http://127.0.0.1:7863/v1"), "{out}");
        assert!(out.contains(DSH_CREDENTIAL_KEY), "{out}");
        assert!(out.contains("agent-default-model:"), "{out}");
        assert!(out.contains("glm-5.2"), "{out}");
        assert!(out.contains("kimi-k2.7"), "{out}");

        let parsed = crate::modules::yaml_lite::parse_mapping(&out).unwrap();
        assert_eq!(parsed["agent-default-model"]["provider"], "workbuddy");
        assert_eq!(parsed["agent-default-model"]["model"], "glm-5.2");
        let provider = &parsed["llm-pi-ai"]["providers"]["workbuddy"];
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["baseURL"], "http://127.0.0.1:7863/v1");
        assert_eq!(provider["models"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn opencode_config_writes_multiple_models() {
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_opencode_config(None, "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["provider"]["workbuddy"]["options"]["baseURL"], "http://127.0.0.1:7863/v1");
        assert_eq!(v["provider"]["workbuddy"]["options"]["apiKey"], "sk-test");
        assert!(v["provider"]["workbuddy"]["models"]["m1"].is_object());
        assert!(v["provider"]["workbuddy"]["models"]["m2"].is_object());
    }

    #[test]
    fn pi_models_writes_multiple_models() {
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_pi_models(None, "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["providers"]["workbuddy"]["baseUrl"], "http://127.0.0.1:7863/v1");
        assert_eq!(v["providers"]["workbuddy"]["apiKey"], "sk-test");
        let list = v["providers"]["workbuddy"]["models"].as_array().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn grok_build_config_writes_multiple_models() {
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_grok_config(None, "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        assert!(out.contains("default = \"m1\""));
        assert!(out.contains("[model.\"m1\"]"));
        assert!(out.contains("[model.\"m2\"]"));
    }

    #[test]
    fn zcode_config_writes_multiple_models() {
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_zcode_config(None, "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        // kind 必须是 ZCode 认识的枚举值，否则该 provider 不被加载。
        assert_eq!(v["provider"]["workbuddy"]["kind"], "openai-compatible");
        assert!(v["provider"]["workbuddy"]["models"]["m1"].is_object());
        assert!(v["provider"]["workbuddy"]["models"]["m2"].is_object());
    }

    #[test]
    fn kimi_code_config_writes_multiple_models() {
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_kimi_code_config(None, "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        assert!(out.contains("default_model = \"m1\""));
        // 凭据挂在 provider 表；模型条目必须带 provider 引用，否则 Kimi CLI 判为无效配置。
        assert!(out.contains("[providers.workbuddy]"), "{out}");
        assert!(out.contains("type = \"openai\""), "{out}");
        assert!(out.contains("provider = \"workbuddy\""), "{out}");
        assert!(out.contains("[models.\"m1\"]"));
        assert!(out.contains("[models.\"m2\"]"));
        assert!(out.contains("capabilities = [\"tool_use\"]"), "{out}");
    }

    #[test]
    fn openclaw_config_writes_multiple_models() {
        let json5 = "{ models: { mode: 'merge', providers: {} } }";
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_openclaw_config(Some(json5), "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let list = v["models"]["providers"]["workbuddy"]["models"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(v["agents"]["defaults"]["model"]["primary"], "workbuddy/m1");
    }

    #[test]
    fn hermes_config_writes_multiple_models() {
        let existing = "model:\n  default: old\n  provider: old\ncustom_providers:\n- name: old\n  base_url: https://old\n";
        let models = vec!["m1".to_string(), "m2".to_string()];
        let out = build_hermes_config(Some(existing), "http://127.0.0.1:7863", "sk-test", &models).unwrap();
        assert!(out.contains("provider: workbuddy"));
        assert!(out.contains("default: m1"));
        assert!(out.contains("- name: workbuddy"));
        assert!(out.contains("m1:"));
        assert!(out.contains("m2:"));
    }
}
