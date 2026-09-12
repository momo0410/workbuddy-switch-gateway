//! 账号存储：读取/写入 `~/.wb-switch/accounts.json`，与 Python 版共享数据目录。
//!
//! 对照 server.py `load_accounts` / `save_accounts` / `find_account` /
//! `account_display_name` / `account_meta`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

use crate::modules::config::{accounts_file, atomic_write};

fn load_accounts_from_path(path: &Path) -> Vec<Value> {
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(Value::Array(accounts)) = serde_json::from_str::<Value>(&text) {
            return accounts;
        }
    }
    vec![]
}

fn save_accounts_to_path(path: &Path, accounts: &[Value]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(accounts).unwrap_or_default();
    atomic_write(path, &content)
}

fn find_account_in(accounts: &[Value], account_id: &str) -> Option<Value> {
    accounts
        .iter()
        .find(|account| {
            account.get("id").and_then(Value::as_str) == Some(account_id)
                || account.get("uid").and_then(Value::as_str) == Some(account_id)
        })
        .cloned()
}

fn delete_account_from_path(path: &Path, account_id: &str) -> Result<(), String> {
    let mut accounts = load_accounts_from_path(path);
    let before = accounts.len();
    accounts.retain(|account| account.get("id").and_then(Value::as_str) != Some(account_id));
    if accounts.len() == before {
        return Err("账号不存在".to_string());
    }
    save_accounts_to_path(path, &accounts).map_err(|error| error.to_string())
}

/// 读取账号库；文件缺失或损坏返回空列表。
pub fn load_accounts() -> Vec<Value> {
    load_accounts_from_path(&accounts_file())
}

/// 写回账号库（原子写），保持原 JSON 数组结构。
pub fn save_accounts(accounts: &[Value]) -> std::io::Result<()> {
    save_accounts_to_path(&accounts_file(), accounts)
}

/// 按 id 或 uid 查找账号。
pub fn find_account(account_id: &str) -> Option<Value> {
    find_account_in(&load_accounts(), account_id)
}

/// 账号展示名（email → nickname → uid → unknown）。
pub fn account_display_name(acc: &Value) -> String {
    get_str(acc, "email")
        .or_else(|| get_str(acc, "nickname"))
        .or_else(|| get_str(acc, "uid"))
        .unwrap_or_else(|| "unknown".to_string())
}

/// 账号的展示元数据（不泄露 token）。对照 server.py `account_meta`。
pub fn account_meta(acc: &Value) -> Value {
    // 区域由 domain 后缀推导（国服 .cn / 国际版 .ai），供界面区分展示。
    let region = crate::modules::config::Region::of(acc);
    json!({
        "region": region.label(),
        "regionKey": match region {
            crate::modules::config::Region::Cn => "cn",
            crate::modules::config::Region::Intl => "intl",
        },
        "id": acc.get("id"),
        "uid": acc.get("uid"),
        "email": acc.get("email"),
        "nickname": acc.get("nickname"),
        "enterpriseName": acc.get("enterpriseName"),
        "expiresAt": acc.get("expiresAt"),
        "refreshExpiresAt": acc.get("refreshExpiresAt"),
        "refreshedAt": acc.get("refreshedAt"),
        "createdAt": acc.get("createdAt"),
        "needsRelogin": acc.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true),
        "needsReloginReason": acc.get("needs_relogin_reason"),
    })
}

/// 取非空字符串字段；空/缺失返回 None。
pub fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 返回可用于 UID 缺失场景的真实邮箱。历史展示占位值不参与身份匹配。
fn identity_email(account: &Value) -> Option<String> {
    let email = get_str(account, "email")?;
    if !email.contains('@')
        || email.eq_ignore_ascii_case("unknown")
        || email == "手动添加"
        || get_str(account, "nickname").as_deref() == Some(email.as_str())
        || get_str(account, "uid").as_deref() == Some(email.as_str())
    {
        return None;
    }
    Some(email.to_ascii_lowercase())
}

/// 两条记录是否属于同一服务区域。
///
/// 国服与国际版的 uid / 邮箱是**相互独立的命名空间**，同一串 uid 或同一个邮箱
/// 在两个区域可以同时存在（实测本机国服与国际版各自登录、uid 互不相干）。
/// 因此身份匹配必须带上区域，否则新采集的国际版账号会直接顶掉同 uid 的国服账号。
/// domain 缺失按国服处理，与 `Region::from_domain` 的历史默认一致。
fn same_region(a: &Value, b: &Value) -> bool {
    crate::modules::config::Region::of(a) == crate::modules::config::Region::of(b)
}

/// 按稳定身份将采集结果合并到账号列表，并返回最终持久化的账号。
///
/// 非空 UID 始终优先；仅当新账号没有 UID 时，才使用真实邮箱兜底。
/// 命中已有身份时保留本地 id，避免调用方持有的账号引用失效。
/// 两个区域各自独立匹配，跨区域永不合并。
pub fn upsert_collected_account(accounts: &mut Vec<Value>, mut collected: Value) -> Value {
    let collected_uid = get_str(&collected, "uid");
    let collected_email = identity_email(&collected);
    let matches_identity = |existing: &Value| {
        if !same_region(existing, &collected) {
            return false;
        }
        if let Some(uid) = collected_uid.as_deref() {
            return get_str(existing, "uid").as_deref() == Some(uid);
        }
        collected_email
            .as_deref()
            .is_some_and(|email| identity_email(existing).as_deref() == Some(email))
    };

    let matching_indexes: Vec<usize> = accounts
        .iter()
        .enumerate()
        .filter_map(|(index, existing)| matches_identity(existing).then_some(index))
        .collect();

    if let Some(&first_index) = matching_indexes.first() {
        let existing = &accounts[first_index];
        if let Some(existing_id) = existing.get("id").cloned() {
            collected["id"] = existing_id;
        }
        if get_str(&collected, "uid").is_none() {
            if let Some(existing_uid) = existing.get("uid").cloned() {
                collected["uid"] = existing_uid;
            }
        }
        if let Some(created_at) = existing.get("createdAt").cloned() {
            collected["createdAt"] = created_at;
        }

        for index in matching_indexes.into_iter().rev() {
            accounts.remove(index);
        }
        accounts.insert(first_index.min(accounts.len()), collected.clone());
    } else {
        accounts.push(collected.clone());
    }

    collected
}

/// 使用统一身份规则保存采集到的账号。
pub fn save_collected_account(collected: Value) -> std::io::Result<Value> {
    let mut accounts = load_accounts();
    let saved = upsert_collected_account(&mut accounts, collected);
    save_accounts(&accounts)?;
    Ok(saved)
}

/// 按 id 覆盖写入账号库（不存在则追加）。对照 server.py `_upsert_account`。
pub fn upsert_account(updated: &Value) -> std::io::Result<()> {
    let mut accounts = load_accounts();
    let id = updated.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut replaced = false;
    for a in accounts.iter_mut() {
        if a.get("id").and_then(|v| v.as_str()) == Some(id) {
            *a = updated.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        accounts.push(updated.clone());
    }
    save_accounts(&accounts)
}

/// 构造与官方对齐的请求头。对照 server.py `build_auth_headers`。
pub fn build_auth_headers(account: &Value) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "Authorization".to_string(),
        format!(
            "Bearer {}",
            get_str(account, "access_token").unwrap_or_default()
        ),
    );
    headers.insert("Accept".to_string(), "application/json".to_string());
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if let Some(uid) = get_str(account, "uid") {
        headers.insert("X-User-Id".to_string(), uid);
    }
    if let Some(eid) =
        get_str(account, "enterpriseId").or_else(|| get_str(account, "enterprise_id"))
    {
        headers.insert("X-Enterprise-Id".to_string(), eid.clone());
        headers.insert("X-Tenant-Id".to_string(), eid);
    }
    if let Some(domain) = get_str(account, "domain") {
        headers.insert("X-Domain".to_string(), domain);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn account_meta_strips_tokens() {
        let acc = json!({
            "id": "a1",
            "uid": "u1",
            "email": "x@y.z",
            "nickname": "小明",
            "enterpriseName": "某公司",
            "access_token": "SECRET_ACCESS",
            "refresh_token": "SECRET_REFRESH",
            "expiresAt": 123456,
            "needs_relogin": true,
            "needs_relogin_reason": "刷新失败",
        });
        let meta = account_meta(&acc);
        assert_eq!(meta["id"], "a1");
        assert_eq!(meta["needsRelogin"], true);
        assert_eq!(meta["needsReloginReason"], "刷新失败");
        assert!(meta.get("access_token").is_none(), "不得泄露 token");
        assert!(meta.get("refresh_token").is_none(), "不得泄露 token");
    }

    #[test]
    fn account_display_name_priority() {
        assert_eq!(
            account_display_name(&json!({"email": "a@b.c", "nickname": "n"})),
            "a@b.c"
        );
        assert_eq!(
            account_display_name(&json!({"nickname": "n", "uid": "u"})),
            "n"
        );
        assert_eq!(account_display_name(&json!({"uid": "u"})), "u");
        assert_eq!(account_display_name(&json!({})), "unknown");
    }

    #[test]
    fn get_str_trims_and_filters_empty() {
        assert_eq!(get_str(&json!({"k": "  v  "}), "k"), Some("v".to_string()));
        assert_eq!(get_str(&json!({"k": "  "}), "k"), None);
        assert_eq!(get_str(&json!({"k": 123}), "k"), None);
    }

    fn account(id: &str, uid: Option<&str>, nickname: &str, email: Option<&str>) -> Value {
        json!({
            "id": id,
            "uid": uid,
            "nickname": nickname,
            "email": email,
            "access_token": format!("token-{id}"),
            "createdAt": 1,
        })
    }

    /// 带区域的账号记录；uid 在两个区域**可以相同**（实测国际版与国服
    /// 各有一套独立 uid 空间，但契约上不保证互不相同）。
    fn regional_account(id: &str, uid: &str, domain: &str) -> Value {
        json!({
            "id": id,
            "uid": uid,
            "domain": domain,
            "nickname": id,
            "access_token": format!("token-{id}"),
            "createdAt": 1,
        })
    }

    #[test]
    fn same_uid_in_different_regions_is_retained() {
        let mut accounts = vec![regional_account("cn", "shared-uid", "www.workbuddy.cn")];
        let saved = upsert_collected_account(
            &mut accounts,
            regional_account("intl", "shared-uid", "www.workbuddy.ai"),
        );

        assert_eq!(accounts.len(), 2, "跨区域同 uid 不得互相覆盖");
        assert_eq!(saved["id"], "intl");
        assert_eq!(
            accounts
                .iter()
                .find(|a| a["id"] == "cn")
                .map(|a| a["domain"].clone()),
            Some(json!("www.workbuddy.cn"))
        );
    }

    #[test]
    fn same_uid_same_region_still_refreshes_in_place() {
        let mut accounts = vec![regional_account("stable", "uid-1", "www.workbuddy.ai")];
        let saved = upsert_collected_account(
            &mut accounts,
            regional_account("generated", "uid-1", "www.workbuddy.ai"),
        );

        assert_eq!(accounts.len(), 1, "同区域同 uid 仍应原地刷新");
        assert_eq!(saved["id"], "stable");
    }

    #[test]
    fn region_identity_ignores_domain_case_and_missing_domain_is_cn() {
        let mut accounts = vec![regional_account("upper", "uid-1", "WWW.WorkBuddy.AI")];
        upsert_collected_account(
            &mut accounts,
            regional_account("lower", "uid-1", "www.workbuddy.ai"),
        );
        assert_eq!(accounts.len(), 1, "domain 比较应忽略大小写");

        // domain 缺失按国服处理：老记录（无 domain）与新采到的国服账号应合并
        let mut legacy = vec![account("legacy", Some("uid-2"), "旧", None)];
        upsert_collected_account(
            &mut legacy,
            regional_account("cn-new", "uid-2", "www.workbuddy.cn"),
        );
        assert_eq!(legacy.len(), 1, "缺 domain 的历史记录按国服合并");
    }

    #[test]
    fn email_fallback_also_respects_region() {
        let mut accounts = vec![json!({
            "id": "cn", "uid": null, "domain": "www.workbuddy.cn",
            "email": "shared@example.com", "access_token": "t",
        })];
        upsert_collected_account(
            &mut accounts,
            json!({
                "id": "intl", "uid": null, "domain": "www.workbuddy.ai",
                "email": "shared@example.com", "access_token": "t2",
            }),
        );
        assert_eq!(accounts.len(), 2, "跨区域同邮箱不得合并");
    }

    #[test]
    fn same_nickname_with_different_uids_is_retained() {
        let mut accounts = vec![account("old", Some("uid-1"), "同名", Some("同名"))];
        let saved =
            upsert_collected_account(&mut accounts, account("new", Some("uid-2"), "同名", None));

        assert_eq!(accounts.len(), 2);
        assert_eq!(saved["id"], "new");
    }

    #[test]
    fn same_uid_refresh_preserves_local_id_and_removes_duplicates() {
        let mut accounts = vec![
            account("stable", Some("uid-1"), "旧名称", Some("old@example.com")),
            account("duplicate", Some("uid-1"), "重复记录", None),
        ];
        let saved = upsert_collected_account(
            &mut accounts,
            account("generated", Some("uid-1"), "新名称", None),
        );

        assert_eq!(accounts.len(), 1);
        assert_eq!(saved["id"], "stable");
        assert_eq!(saved["nickname"], "新名称");
        assert_eq!(saved["access_token"], "token-generated");
    }

    #[test]
    fn different_uids_with_same_real_email_are_retained() {
        let mut accounts = vec![account(
            "old",
            Some("uid-1"),
            "账号一",
            Some("shared@example.com"),
        )];
        upsert_collected_account(
            &mut accounts,
            account("new", Some("uid-2"), "账号二", Some("shared@example.com")),
        );

        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn real_email_is_fallback_only_when_collected_uid_is_missing() {
        let mut accounts = vec![account("stable", None, "旧名称", Some("user@example.com"))];
        let saved = upsert_collected_account(
            &mut accounts,
            account("generated", None, "新名称", Some("USER@example.com")),
        );

        assert_eq!(accounts.len(), 1);
        assert_eq!(saved["id"], "stable");
        assert_eq!(saved["nickname"], "新名称");
    }

    #[test]
    fn legacy_synthetic_email_does_not_merge_accounts() {
        let mut accounts = vec![account("old", None, "同名", Some("同名"))];
        upsert_collected_account(&mut accounts, account("new", None, "同名", Some("同名")));

        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn persisted_same_name_accounts_can_be_found_and_deleted_independently() {
        let test_dir = std::env::temp_dir().join(format!(
            "wb-switch-same-name-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = test_dir.join("accounts.json");
        let mut accounts = vec![];
        upsert_collected_account(
            &mut accounts,
            account("account-1", Some("uid-1"), "同名用户", None),
        );
        upsert_collected_account(
            &mut accounts,
            account("account-2", Some("uid-2"), "同名用户", None),
        );
        save_accounts_to_path(&path, &accounts).expect("same-name accounts should persist");

        let persisted = load_accounts_from_path(&path);
        assert_eq!(
            find_account_in(&persisted, "account-1").unwrap()["uid"],
            "uid-1"
        );
        assert_eq!(
            find_account_in(&persisted, "account-2").unwrap()["uid"],
            "uid-2"
        );

        delete_account_from_path(&path, "account-1").expect("first account should delete");
        let after_first_delete = load_accounts_from_path(&path);
        assert!(find_account_in(&after_first_delete, "account-1").is_none());
        assert_eq!(
            find_account_in(&after_first_delete, "account-2").unwrap()["uid"],
            "uid-2"
        );

        delete_account_from_path(&path, "account-2").expect("second account should delete");
        assert!(load_accounts_from_path(&path).is_empty());
        std::fs::remove_dir_all(&test_dir).expect("temporary account store should clean up");
    }
}

/// 删除账号（按 id）。
pub fn delete_account(account_id: &str) -> Result<(), String> {
    delete_account_from_path(&accounts_file(), account_id)
}

/// 导入本机当前账号（从认证文件读取）。
pub fn import_local() -> Result<Value, String> {
    let acc = crate::modules::auth_file::import_from_auth_file()
        .ok_or("未读取到本地 WorkBuddy 登录信息")?;
    let saved = save_collected_account(acc).map_err(|e| e.to_string())?;
    Ok(account_meta(&saved))
}

/// 从本机一键导入**所有可发现的区域**（国服 + 国际版）。
///
/// CodeBuddy / WorkBuddy 客户端把不同区域的登录态写在同目录的**不同文件**里：
///   workbuddy-desktop.info       国服
///   workbuddy-desktop-ai.info    国际版
/// 因此这里逐个探测，把能读到的全部并入账号库。
///
/// 返回本次实际导入（或更新）的账号元数据列表；全部未发现时给出可操作的错误。
pub fn import_local_all() -> Result<Vec<Value>, String> {
    use crate::modules::config::Region;

    let mut imported: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for region in Region::ALL {
        match crate::modules::auth_file::import_from_auth_file_for(region) {
            None => notes.push(format!("{}：未发现本机登录", region.label())),
            Some(acc) => {
                // 区域以认证文件为准：旧记录可能缺 domain，用文件名兜底标注。
                let mut acc = acc;
                if get_str(&acc, "domain").is_none() {
                    acc["domain"] = json!(match region {
                        Region::Cn => "www.workbuddy.cn",
                        Region::Intl => "www.workbuddy.ai",
                    });
                }
                match save_collected_account(acc) {
                    Ok(saved) => imported.push(account_meta(&saved)),
                    Err(e) => notes.push(format!("{}：保存失败 {e}", region.label())),
                }
            }
        }
    }

    if imported.is_empty() {
        return Err(format!(
            "未发现本机登录信息（已尝试 国服 / 国际版）。{}",
            notes.join("；")
        ));
    }
    Ok(imported)
}

// 手动添加账号（token 方式）已随 UI 入口「手动添加」一并下线；
// `identity_email` 中的 "手动添加" 占位过滤保留，用于兼容历史手动添加的旧账号。
