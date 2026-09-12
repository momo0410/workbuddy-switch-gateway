//! CodeBuddy CLI 账号轮换桥接。
//!
//! 通过 `~/.codebuddy/settings.json` 的 `env.CODEBUDDY_AUTH_TOKEN` 指定后续
//! 会话使用的账号，复用 wb-switch 的 WorkBuddy 账号库，但保持独立的当前账号。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::modules::account;
use crate::modules::config::{atomic_write, home_dir, now_ms};

const ROTATE_DIR: &str = ".codebuddy-rotate";
const STATE_FILE: &str = "state.json";
/// 旧版 helper.cjs 文件名，仅用于状态自检（判断用户是否仍配置了 helper）。
const LOGIC_FILE: &str = "helper.cjs";
const SETTINGS_DIR: &str = ".codebuddy";
const SETTINGS_FILE: &str = "settings.json";
const CODEBUDDY_AUTH_TOKEN: &str = "CODEBUDDY_AUTH_TOKEN";

fn rotate_dir() -> PathBuf {
    home_dir().join(ROTATE_DIR)
}

fn state_path() -> PathBuf {
    rotate_dir().join(STATE_FILE)
}

fn settings_path() -> PathBuf {
    home_dir().join(SETTINGS_DIR).join(SETTINGS_FILE)
}

fn clean_bearer_token(token: &str) -> &str {
    let token = token.trim();
    if token == "Bearer" {
        return "";
    }
    token.strip_prefix("Bearer ").unwrap_or(token).trim()
}

fn settings_env_token(value: &Value) -> Option<&str> {
    value
        .get("env")
        .and_then(Value::as_object)
        .and_then(|env| env.get(CODEBUDDY_AUTH_TOKEN))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn process_env_token_present() -> bool {
    std::env::var_os(CODEBUDDY_AUTH_TOKEN)
        .is_some_and(|token| !token.to_string_lossy().trim().is_empty())
}

fn ensure_no_process_env_override() -> Result<(), String> {
    if process_env_token_present() {
        return Err(auth_config_error(
            "环境阶段",
            "检测到进程环境变量 CODEBUDDY_AUTH_TOKEN；它会覆盖 settings.json，请先删除该用户或系统环境变量并重启应用与 CodeBuddy CLI",
        ));
    }
    Ok(())
}

fn write_settings_env_token(value: &mut Value, token: &str) -> Result<(), String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "CodeBuddy settings.json 顶层不是对象".to_string())?;
    let env = object.entry("env").or_insert_with(|| json!({}));
    let env = env
        .as_object_mut()
        .ok_or_else(|| "CodeBuddy settings.json 的 env 字段不是对象".to_string())?;
    env.insert(CODEBUDDY_AUTH_TOKEN.to_string(), json!(token));
    Ok(())
}

fn persist_settings_at(settings: &Path, value: &Value) -> Result<(), String> {
    let content = serde_json::to_string_pretty(value).map_err(|_| {
        auth_config_error("配置阶段", "无法生成 CodeBuddy settings.json")
    })?;
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent).map_err(|_| {
            auth_config_error(
                "配置阶段",
                "无法创建 CodeBuddy 配置目录，请检查用户目录权限",
            )
        })?;
    }
    atomic_write(&settings, &content).map_err(|_| {
        auth_config_error(
            "配置阶段",
            "无法写入 CodeBuddy settings.json，请检查文件权限",
        )
    })
}

fn validate_persisted_env_token_at(settings: &PathBuf, expected_token: &str) -> Result<(), String> {
    let persisted = read_json_file(settings).ok_or_else(|| {
        auth_config_error("配置阶段", "写入后无法重新读取 CodeBuddy settings.json")
    })?;
    if settings_env_token(&persisted).map(clean_bearer_token)
        != Some(clean_bearer_token(expected_token))
    {
        return Err(auth_config_error(
            "配置阶段",
            "写入后的认证信息与所选账号不一致",
        ));
    }
    Ok(())
}

fn prepare_settings_env_update(
    settings: &PathBuf,
    token: &str,
) -> Result<(Option<String>, Value), String> {
    let previous = std::fs::read_to_string(settings).ok();
    let mut value = previous
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| "CodeBuddy settings.json 不是有效 JSON")?
        .unwrap_or_else(|| json!({}));
    write_settings_env_token(&mut value, clean_bearer_token(token))?;
    Ok((previous, value))
}

fn commit_settings_env_update(
    settings: &PathBuf,
    previous: Option<&str>,
    value: &Value,
    expected_token: &str,
) -> Result<(), String> {
    if let Err(error) = persist_settings_at(settings, value)
        .and_then(|_| validate_persisted_env_token_at(settings, expected_token))
    {
        restore_file(settings, previous);
        return Err(error);
    }
    Ok(())
}

fn read_json_file(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn helper_command() -> Option<String> {
    read_json_file(&settings_path())
        .and_then(|settings| {
            settings
                .get("apiKeyHelper")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty())
}

/// 旧版 `apiKeyHelper` 在 CodeBuddy 2.138.0 中先作为文件路径解析；
/// 这里只识别单一路径，用于状态自检（判断用户是否仍配置着旧 helper）。
fn command_path(command: &str) -> Option<PathBuf> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 只把单一路径视为项目 helper；不解析或接管用户的其他 shell 命令。
    // Windows 绝对路径可能含空格；CodeBuddy 2.138.0 会先将路径解析为
    // workdir 绝对路径，再交给 shell，所以不能在设置里额外包引号。
    let windows_absolute = trimmed.len() >= 3
        && trimmed.as_bytes()[0].is_ascii_alphabetic()
        && trimmed.as_bytes()[1] == b':'
        && matches!(trimmed.as_bytes()[2], b'\\' | b'/');
    if trimmed.starts_with('\'')
        || trimmed.starts_with('"')
        || (!windows_absolute && trimmed.chars().any(char::is_whitespace))
    {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn helper_path() -> Option<PathBuf> {
    helper_command().and_then(|command| command_path(&command))
}

fn helper_is_configured() -> bool {
    helper_path().map(|path| path.is_file()).unwrap_or(false)
}

fn helper_supports_account_ids() -> bool {
    // 升级前可能仍配置 `.cmd` 跳板，因此同时检查实际
    // helper.cjs；新的认证方式不再使用 helper。
    let configured = helper_path().and_then(|path| std::fs::read_to_string(path).ok());
    let logic = std::fs::read_to_string(rotate_dir().join(LOGIC_FILE)).ok();
    configured
        .into_iter()
        .chain(logic)
        .any(|source| source.contains("activeAccountId"))
}

fn restore_file(path: &Path, previous: Option<&str>) {
    if let Some(previous) = previous {
        let _ = atomic_write(path, previous);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn auth_config_error(stage: &str, cause: &str) -> String {
    format!("CodeBuddy CLI 认证配置失败（{stage}）：{cause}")
}

fn account_token(account: &Value) -> Result<&str, String> {
    account
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| auth_config_error(
            "账号阶段",
            "所选账号没有可用的认证信息，请先重新登录或刷新 Token",
        ))
}

fn settings_account_token(account: &Value) -> Result<&str, String> {
    let token = clean_bearer_token(account_token(account)?);
    if token.is_empty() {
        return Err(auth_config_error(
            "账号阶段",
            "所选账号没有可用的认证信息，请先重新登录或刷新 Token",
        ));
    }
    Ok(token)
}

fn load_state() -> Value {
    read_json_file(&state_path())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn account_index(accounts: &[Value], account_id: &str) -> Option<(usize, String)> {
    accounts.iter().enumerate().find_map(|(index, account)| {
        let matches = account.get("id").and_then(Value::as_str) == Some(account_id)
            || account.get("uid").and_then(Value::as_str) == Some(account_id);
        if !matches {
            return None;
        }
        let canonical_id = account
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| account_id.to_string());
        Some((index, canonical_id))
    })
}

fn state_account_index(state: &Value, accounts: &[Value]) -> Option<(usize, String)> {
    if accounts.is_empty() {
        return None;
    }
    if let Some(active_id) = state.get("activeAccountId").and_then(Value::as_str) {
        if let Some(found) = account_index(accounts, active_id) {
            return Some(found);
        }
    }

    let index = state
        .get("active")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .rem_euclid(accounts.len() as i64) as usize;
    let account = accounts.get(index)?;
    let id = account.get("id").and_then(Value::as_str)?.to_string();
    Some((index, id))
}

fn account_index_by_token(accounts: &[Value], token: &str) -> Option<(usize, String)> {
    let expected = clean_bearer_token(token);
    if expected.is_empty() {
        return None;
    }
    accounts.iter().enumerate().find_map(|(index, account)| {
        let actual = account
            .get("access_token")
            .and_then(Value::as_str)
            .map(clean_bearer_token)?;
        if actual != expected {
            return None;
        }
        let id = account.get("id").and_then(Value::as_str)?.to_string();
        Some((index, id))
    })
}

/// 把当前 CLI 账号刷新后的 token 同步到 settings env。
/// 仅当 settings 当前 token 能匹配该账号（或状态明确指向该账号）时写入，
/// 避免后台刷新覆盖用户刚刚手动选择的其他账号；失败只返回脱敏错误。
pub fn sync_windows_env_for_account(
    account_value: &Value,
    previous_access_token: Option<&str>,
) -> Result<bool, String> {
    ensure_no_process_env_override()?;
    let settings = settings_path();
    let Some(current_settings) = read_json_file(&settings) else {
        return Ok(false);
    };
    let Some(current_token) = settings_env_token(&current_settings) else {
        return Ok(false);
    };
    let accounts = account::load_accounts();
    let state = load_state();
    let Some((_, active_id)) = state_account_index(&state, &accounts) else {
        return Ok(false);
    };
    let account_id = account_value.get("id").and_then(Value::as_str);
    if account_id != Some(active_id.as_str()) {
        return Ok(false);
    }
    let Some(updated_token) = account_value.get("access_token").and_then(Value::as_str) else {
        return Ok(false);
    };
    // settings 必须仍是刷新前 token（或已同步的新 token）；否则视为用户已切换/手工修改。
    let current = clean_bearer_token(current_token);
    let previous_matches = previous_access_token
        .map(clean_bearer_token)
        .is_some_and(|token| token == current);
    let already_synced = clean_bearer_token(updated_token) == current;
    if !previous_matches && !already_synced {
        return Ok(false);
    }
    if already_synced {
        return Ok(true);
    }
    let (previous, value) = prepare_settings_env_update(&settings, updated_token)?;
    commit_settings_env_update(&settings, previous.as_deref(), &value, updated_token)?;
    Ok(true)
}

/// 返回脱敏的 CLI 轮换状态，不返回 token 或 helper 内容。
pub fn status() -> Value {
    let accounts = account::load_accounts();
    let state = load_state();
    let settings = read_json_file(&settings_path()).unwrap_or_else(|| json!({}));
    let env_token = settings_env_token(&settings);
    let env_configured = env_token.is_some();
    let environment_override = process_env_token_present();
    let active = env_token.and_then(|token| account_index_by_token(&accounts, token));
    let expected_active = state_account_index(&state, &accounts);
    let configured = env_configured && !environment_override;
    // 仍配置着旧 helper 但尚未迁移到 settings env：提示用户重新接入。
    let migration_required = environment_override || (!env_configured && helper_is_configured());
    json!({
        "configured": configured,
        "authMode": "settings-env",
        "environmentOverride": environment_override,
        "helperCurrent": env_configured,
        "migrationRequired": migration_required,
        "syncPending": env_configured && active.is_none() && expected_active.is_some(),
        "settingsPresent": settings_path().is_file(),
        "helperPresent": helper_path().map(|path| path.is_file()).unwrap_or(false),
        "helperSupportsAccountIds": helper_supports_account_ids(),
        "activeIndex": active.as_ref().map(|(index, _)| *index),
        "activeAccountId": active.as_ref().map(|(_, id)| id),
        "activeAccountName": active.and_then(|(_, id)| account::find_account(&id).map(|account| account::account_display_name(&account))),
        "accountCount": accounts.len(),
        "statePath": state_path().to_string_lossy(),
    })
}

/// 写入 settings env 认证（`env.CODEBUDDY_AUTH_TOKEN`）。
/// 只有用户显式调用这个命令时才会修改用户级配置。
///
/// 兼容旧版：早期使用 `apiKeyHelper`（helper.cjs / helper.cmd / helper.sh）；
/// 现在统一改为 settings env，残留的旧 helper 配置会在状态里提示重新接入。
pub fn install_helper() -> Result<Value, String> {
    ensure_no_process_env_override()?;
    let settings = settings_path();
    let accounts = account::load_accounts();
    let state = load_state();
    let active = state_account_index(&state, &accounts)
        .and_then(|(index, _)| accounts.get(index))
        .or_else(|| accounts.first())
        .ok_or_else(|| {
            auth_config_error("账号阶段", "当前没有可供 CodeBuddy CLI 使用的账号")
        })?;
    let token = settings_account_token(active)?;
    let (previous_settings, settings_value) = prepare_settings_env_update(&settings, token)?;
    commit_settings_env_update(
        &settings,
        previous_settings.as_deref(),
        &settings_value,
        token,
    )?;

    Ok(json!({
        "ok": true,
        "configured": true,
        "authMode": "settings-env",
        "helperPresent": helper_path().map(|path| path.is_file()).unwrap_or(false),
        "helperSupportsAccountIds": helper_supports_account_ids(),
        "verified": true,
        "message": "CodeBuddy CLI 认证配置已更新；当前运行会话不会切换，请由 ACP 重新加载会话或重启 CLI 后生效",
    }))
}

/// 将 CodeBuddy CLI 的当前账号设置为 WorkBuddy 账号库中的目标账号。
pub fn set_active_account(account_id: &str) -> Result<Value, String> {
    ensure_no_process_env_override()?;

    let accounts = account::load_accounts();
    let Some((index, canonical_id)) = account_index(&accounts, account_id) else {
        return Err("账号不存在".to_string());
    };

    // 先验证并生成完整 settings，再写独立账号状态，避免无效 JSON、
    // 缺失 token 等前置错误造成只有 state.json 被修改的半成功。
    let settings = settings_path();
    let token = settings_account_token(&accounts[index])?;
    let (previous_settings, settings_value) = prepare_settings_env_update(&settings, token)?;

    let previous_state = std::fs::read_to_string(state_path()).ok();
    let mut state = load_state();
    state["active"] = json!(index);
    state["activeAccountId"] = json!(canonical_id);
    state["updatedAt"] = json!(now_ms());
    std::fs::create_dir_all(rotate_dir()).map_err(|_| {
        auth_config_error("状态阶段", "无法创建 CLI 账号状态目录，请检查用户目录权限")
    })?;
    let content = serde_json::to_string_pretty(&state).map_err(|error| error.to_string())?;
    atomic_write(&state_path(), &content).map_err(|_| {
        auth_config_error("状态阶段", "无法写入所选 CLI 账号状态，请检查文件权限")
    })?;

    if let Err(error) = commit_settings_env_update(
        &settings,
        previous_settings.as_deref(),
        &settings_value,
        token,
    ) {
        restore_file(&state_path(), previous_state.as_deref());
        return Err(error);
    }

    Ok(json!({
        "ok": true,
        "configured": true,
        "synced": true,
        "verified": true,
        "authMode": "settings-env",
        "activeIndex": index,
        "activeAccountId": canonical_id,
        "message": "CodeBuddy CLI 默认账号已更新；当前运行会话不会切换，请由 ACP 重新加载会话或重启 CLI 后生效",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn helper_test_dir() -> PathBuf {
        std::env::temp_dir().join(format!("wb-switch-codebuddy-helper-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn resolves_account_by_id_or_uid_and_returns_canonical_id() {
        let accounts = vec![
            json!({"id": "a1", "uid": "u1"}),
            json!({"id": "a2", "uid": "u2"}),
        ];
        assert_eq!(account_index(&accounts, "a2"), Some((1, "a2".to_string())));
        assert_eq!(account_index(&accounts, "u1"), Some((0, "a1".to_string())));
        assert_eq!(account_index(&accounts, "missing"), None);
    }

    #[test]
    fn state_prefers_account_id_over_legacy_index() {
        let accounts = vec![
            json!({"id": "a1", "uid": "u1"}),
            json!({"id": "a2", "uid": "u2"}),
        ];
        let state = json!({"active": 0, "activeAccountId": "a2"});
        assert_eq!(
            state_account_index(&state, &accounts),
            Some((1, "a2".to_string()))
        );
    }

    #[test]
    fn legacy_index_wraps_without_panicking() {
        let accounts = vec![json!({"id": "a1"}), json!({"id": "a2"})];
        let state = json!({"active": 5});
        assert_eq!(
            state_account_index(&state, &accounts),
            Some((1, "a2".to_string()))
        );
    }

    #[test]
    fn empty_accounts_have_no_active_account() {
        assert_eq!(state_account_index(&json!({"active": 0}), &[]), None);
    }

    #[test]
    fn settings_env_token_preserves_helper_and_other_env_values() {
        let mut settings = json!({
            "apiKeyHelper": "C:/Users/tester/bin/wb-helper.bat",
            "trustedDirectories": ["C:/Users/tester"],
            "env": { "HTTPS_PROXY": "http://127.0.0.1:7890" }
        });
        write_settings_env_token(&mut settings, "RAW_SECRET").unwrap();
        assert_eq!(settings_env_token(&settings), Some("RAW_SECRET"));
        assert_eq!(
            settings["apiKeyHelper"],
            "C:/Users/tester/bin/wb-helper.bat"
        );
        assert_eq!(settings["env"]["HTTPS_PROXY"], "http://127.0.0.1:7890");
        assert_eq!(settings["env"][CODEBUDDY_AUTH_TOKEN], "RAW_SECRET");
    }

    #[test]
    fn settings_env_token_rejects_non_object_env() {
        let mut settings = json!({"env": "invalid"});
        let error = write_settings_env_token(&mut settings, "SECRET").unwrap_err();
        assert!(error.contains("env 字段不是对象"));
        assert!(!error.contains("SECRET"));
    }

    #[test]
    fn settings_env_file_update_roundtrips_and_preserves_existing_fields() {
        let test_dir = helper_test_dir();
        let settings = test_dir.join(".codebuddy").join("settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            r#"{
  "apiKeyHelper": "C:/Users/tester/bin/wb-helper.bat",
  "trustedDirectories": ["C:/Users/tester"],
  "env": { "HTTPS_PROXY": "http://127.0.0.1:7890" }
}"#,
        )
        .unwrap();

        let (previous, value) =
            prepare_settings_env_update(&settings, "Bearer RAW_SECRET").unwrap();
        commit_settings_env_update(&settings, previous.as_deref(), &value, "RAW_SECRET").unwrap();

        let persisted = read_json_file(&settings).unwrap();
        assert_eq!(settings_env_token(&persisted), Some("RAW_SECRET"));
        assert_eq!(persisted["apiKeyHelper"], "C:/Users/tester/bin/wb-helper.bat");
        assert_eq!(persisted["trustedDirectories"][0], "C:/Users/tester");
        assert_eq!(persisted["env"]["HTTPS_PROXY"], "http://127.0.0.1:7890");
        fs::remove_dir_all(test_dir).unwrap();
    }

    #[test]
    fn windows_current_account_is_derived_from_persisted_token() {
        let accounts = vec![
            json!({"id": "a1", "access_token": "TOKEN_ONE"}),
            json!({"id": "a2", "access_token": "TOKEN_TWO"}),
        ];
        assert_eq!(
            account_index_by_token(&accounts, "Bearer TOKEN_TWO"),
            Some((1, "a2".to_string()))
        );
        assert_eq!(account_index_by_token(&accounts, "UNKNOWN"), None);
    }

    #[test]
    fn settings_account_token_rejects_empty_bearer_value() {
        let error = settings_account_token(&json!({"access_token": "Bearer "})).unwrap_err();
        assert!(error.contains("没有可用的认证信息"));
        assert!(!error.contains("Bearer"));
    }

    #[test]
    fn invalid_settings_file_is_not_overwritten_or_leaked() {
        let test_dir = helper_test_dir();
        let settings = test_dir.join("settings.json");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(&settings, "not-json SECRET_ON_DISK").unwrap();

        let error = prepare_settings_env_update(&settings, "NEW_SECRET").unwrap_err();
        assert!(error.contains("不是有效 JSON"));
        assert!(!error.contains("NEW_SECRET"));
        assert!(!error.contains("SECRET_ON_DISK"));
        assert_eq!(fs::read_to_string(&settings).unwrap(), "not-json SECRET_ON_DISK");
        fs::remove_dir_all(test_dir).unwrap();
    }

    #[test]
    fn recognizes_legacy_windows_path_and_direct_cjs_path() {
        assert_eq!(
            command_path(r"C:\Users\tester\.codebuddy-rotate\helper.cmd"),
            Some(PathBuf::from(
                r"C:\Users\tester\.codebuddy-rotate\helper.cmd"
            ))
        );
        assert_eq!(
            command_path(r"C:\Users\test user\.codebuddy-rotate\helper.cjs"),
            Some(PathBuf::from(
                r"C:\Users\test user\.codebuddy-rotate\helper.cjs"
            ))
        );
        assert!(command_path("node helper.cjs").is_none());
    }

    #[test]
    fn restores_previous_file_after_failed_validation() {
        let test_dir = helper_test_dir();
        fs::create_dir_all(&test_dir).unwrap();
        let existing = test_dir.join("existing.json");
        fs::write(&existing, "old").unwrap();
        restore_file(&existing, Some("old"));
        assert_eq!(fs::read_to_string(&existing).unwrap(), "old");

        let newly_created = test_dir.join("new.json");
        fs::write(&newly_created, "temporary").unwrap();
        restore_file(&newly_created, None);
        assert!(!newly_created.exists());
        fs::remove_dir_all(test_dir).unwrap();
    }
}
