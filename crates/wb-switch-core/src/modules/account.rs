//! 账号存储：读取/写入 `~/.wb-switch/accounts.json`，与 Python 版共享数据目录。
//!
//! 对照 server.py `load_accounts` / `save_accounts` / `find_account` /
//! `account_display_name` / `account_meta`。

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;

use crate::modules::auth_file::{self, CredentialFreshness};
use crate::modules::config::{accounts_file, atomic_write, Region};

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

/// 账号备注：用户自己写的标签（如「公司号」「备用」「张三的号」）。
///
/// 为什么需要：授权进来的账号往往只带邮箱/手机号/随机 uid，光看这些认不出
/// 「这是谁的号、干什么用的」。备注由用户定义、只存本地账号库，不参与登录。
pub fn account_note(acc: &Value) -> String {
    get_str(acc, "note").unwrap_or_default()
}

/// 设置账号备注并落盘；返回更新后的账号。
///
/// 传空串 = 清除备注。备注是纯展示信息，不触碰任何凭证字段。
pub fn set_account_note(account_id: &str, note: &str) -> Result<Value, String> {
    let mut accounts = load_accounts();
    let trimmed = note.trim();
    let acc = accounts
        .iter_mut()
        .find(|a| get_str(a, "id").as_deref() == Some(account_id))
        .ok_or_else(|| "账号不存在".to_string())?;
    if trimmed.is_empty() {
        // 清除：删字段而不是写空串，保持账号库干净（也避免导出时残留）。
        if let Some(obj) = acc.as_object_mut() {
            obj.remove("note");
        }
    } else {
        if let Some(obj) = acc.as_object_mut() {
            obj.insert("note".to_string(), json!(trimmed));
        }
    }
    let updated = acc.clone();
    save_accounts(&accounts).map_err(|e| e.to_string())?;
    Ok(updated)
}

/// 展示字段安全化：只接受字符串，其余（数字 / 布尔 / 对象 / 数组 / 缺失）一律归一成 null。
///
/// 上游鉴权文件里的加密信封 `{ $wbEncrypted, envelope }` 是对象，若原样透传到
/// 前端，会被 React 当作子节点渲染而抛 `#31`（"Objects are not valid as a React
/// child"），导致账号页整页白屏。见 issue #36 / #38。
pub fn display_string(v: Option<&Value>) -> Value {
    match v.and_then(Value::as_str) {
        Some(s) => Value::String(s.to_string()),
        None => Value::Null,
    }
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
        "uid": display_string(acc.get("uid")),
        "email": display_string(acc.get("email")),
        "nickname": display_string(acc.get("nickname")),
        "enterpriseName": display_string(acc.get("enterpriseName")),
        "expiresAt": acc.get("expiresAt"),
        "refreshExpiresAt": acc.get("refreshExpiresAt"),
        "refreshedAt": acc.get("refreshedAt"),
        "createdAt": acc.get("createdAt"),
        "needsRelogin": acc.get("needs_relogin").and_then(|v| v.as_bool()) == Some(true),
        "needsReloginReason": acc.get("needs_relogin_reason"),
        // 备注：用户自定义标签，用于认出「这是谁的号」。
        // 强制为字符串或 null：账号库里若混入了对象（如上游鉴权文件的加密信封
        // `{ $wbEncrypted, envelope }` 被误存进 note），原样透传会在前端渲染时
        // 触发 React #31（"Objects are not valid as a React child"）导致整页白屏。
        // 见 issue #36 / #38。
        "note": display_string(acc.get("note")),
        // 原始域名（如 www.workbuddy.ai / copilot.tencent.com）：
        // 区域标签只给「国服/国际版」，排查问题时常需要看确切域名。
        "domain": display_string(acc.get("domain")),
        // 手机号（国服账号的真实身份线索，邮箱常为空）。
        // 同样强制为字符串或 null，避免 profile_raw 字段类型异常时把对象透传到前端。
        "phoneNumber": display_string(acc.get("profile_raw").and_then(|p| p.get("phoneNumber"))),
        // 账号类型（personal / enterprise）：影响可用模型与额度口径。
        "accountType": display_string(acc.get("profile_raw").and_then(|p| p.get("type"))),
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
    use crate::modules::config::now_ms;
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

    /// 账号详情所需字段必须透出（供「查看账号详情」弹窗回答「这是谁的号」）。
    ///
    /// 此前 account_meta 只有昵称/uid/邮箱，而实际能辨认账号的线索还包括
    /// 手机号（国服账号邮箱常为空）、原始域名、账号类型 —— 都不在返回值里。
    #[test]
    fn account_meta_exposes_identity_fields() {
        let acc = json!({
            "id": "a1",
            "uid": "u1",
            "nickname": "小明",
            "email": "",
            "domain": "copilot.tencent.com",
            "note": "公司号",
            "profile_raw": {"phoneNumber": "13800138000", "type": "personal"},
            "access_token": "SECRET",
        });
        let meta = account_meta(&acc);
        assert_eq!(meta["note"], "公司号");
        assert_eq!(meta["domain"], "copilot.tencent.com");
        assert_eq!(meta["phoneNumber"], "13800138000", "手机号是国服账号的主要身份线索");
        assert_eq!(meta["accountType"], "personal");
        assert!(meta.get("access_token").is_none(), "新增字段不得带出 token");
    }

    /// 字段缺失时不应 panic，也不应伪造值（界面渲染成「—」）。
    #[test]
    fn account_meta_tolerates_missing_optional_fields() {
        let meta = account_meta(&json!({"id": "a1", "uid": "u1"}));
        assert!(meta["note"].is_null(), "无备注应为 null，而非空串");
        assert!(meta["domain"].is_null());
        assert!(meta["phoneNumber"].is_null(), "无 profile_raw 时不应 panic");
        assert!(meta["accountType"].is_null());
    }

    /// 回归：账号库里若混入了非字符串字段（如上游鉴权文件的加密信封
    /// `{ $wbEncrypted, envelope }` 被误存进 note / profile_raw），`account_meta`
    /// 必须强制为 null，绝不能把对象透传到前端——否则前端直接渲染该对象会抛
    /// React error #31（"Objects are not valid as a React child"）导致整页白屏。
    /// 见 issue #36。
    #[test]
    fn account_meta_coerces_non_scalar_fields_to_null() {
        let acc = json!({
            "id": "a1",
            "uid": "u1",
            "note": { "$wbEncrypted": true, "envelope": "abc" },
            "domain": 12345,
            "profile_raw": { "phoneNumber": { "encrypted": "x" }, "type": 7 }
        });
        let meta = account_meta(&acc);
        assert!(meta["note"].is_null(), "对象型 note 必须被丢弃为 null，而非透传对象");
        assert!(meta["domain"].is_null(), "数字型 domain 必须被丢弃为 null");
        assert!(meta["phoneNumber"].is_null(), "对象型 phoneNumber 必须被丢弃为 null");
        assert!(meta["accountType"].is_null(), "数字型 accountType 必须被丢弃为 null");
    }

    /// 反向验证：故意把对象型 note 透传（旧行为），确认它会确实产出对象——
    /// 以此证明上面的 coerce 修复确实消除了 React #31 的触发条件。
    #[test]
    fn account_meta_old_behavior_leaks_object_note() {
        let acc = json!({ "id": "a1", "uid": "u1", "note": { "$wbEncrypted": true, "envelope": "abc" } });
        // 旧实现等价于 `acc.get("note")`，会原样保留对象。
        let leaked = acc.get("note").expect("测试数据含 note");
        assert!(leaked.is_object(), "旧行为：对象型 note 原样透传，正是白屏根因");
    }

    /// 备注读取：有则取值，无则空串（区别于 account_meta 的 null 语义）。
    #[test]
    fn account_note_reads_and_defaults_empty() {
        assert_eq!(account_note(&json!({"note": "备用"})), "备用");
        assert_eq!(account_note(&json!({"note": "  备用  "})), "备用", "应去掉首尾空白");
        assert_eq!(account_note(&json!({})), "");
        assert_eq!(account_note(&json!({"note": ""})), "");
        assert_eq!(account_note(&json!({"note": "   "})), "", "纯空白视为无备注");
    }

    /// `display_string` 是 issue #36 / #38 的统一防线：只收字符串，其余一律归一成 null。
    #[test]
    fn display_string_keeps_string_and_nulls_everything_else() {
        assert_eq!(display_string(Some(&json!("小明"))), "小明");
        assert!(display_string(Some(&json!(12345))).is_null(), "数字必须归一成 null");
        assert!(display_string(Some(&json!(true))).is_null(), "布尔必须归一成 null");
        // 加密信封对象正是 issue #38 的复现数据。
        let envelope = json!({ "$wbEncrypted": 1, "envelope": "eyJzdWl0ZSI6MX0=" });
        assert!(display_string(Some(&envelope)).is_null(), "信封对象必须归一成 null");
        assert!(display_string(Some(&json!([1, 2, 3]))).is_null(), "数组必须归一成 null");
        assert!(display_string(None).is_null(), "缺失必须为 null");
    }

    /// 回归：account_meta 的身份展示字段（uid / email / nickname / enterpriseName）
    /// 若混入加密信封对象，必须归一成 null，绝不能透传对象到前端触发 React #31。
    /// 这是 issue #38 在 get_status 之外的同源隐患（get_accounts 走 account_meta）。
    #[test]
    fn account_meta_coerces_envelope_identity_fields_to_null() {
        let acc = json!({
            "id": "a1",
            "uid": { "$wbEncrypted": 1, "envelope": "e1" },
            "email": "x@y.z",
            "nickname": { "$wbEncrypted": 1, "envelope": "e2" },
            "enterpriseName": { "$wbEncrypted": 1, "envelope": "e3" },
        });
        let meta = account_meta(&acc);
        assert!(meta["uid"].is_null(), "信封对象 uid 必须归一成 null");
        assert!(meta["nickname"].is_null(), "信封对象 nickname 必须归一成 null");
        assert!(meta["enterpriseName"].is_null(), "信封对象 enterpriseName 必须归一成 null");
        assert_eq!(meta["email"], "x@y.z", "正常邮箱字符串不受影响");
        assert_eq!(meta["id"], "a1", "id 保持原样（前端要求必填字符串）");
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

    // ---- 本机历史账号发现与导入 ----

    /// 同区域同 uid 出现多份文件时，只留凭证最新的一份。
    #[test]
    fn scan_prefers_freshest_credential_per_identity() {
        use crate::modules::auth_file::{discover_local_accounts_in, CredentialFreshness};

        let auth = temp_scan_dir("scan-auth");
        let backups = temp_scan_dir("scan-backup");
        let now = now_ms();
        let fresh = now + 30 * 24 * 3600 * 1000;
        let stale = now - 3600 * 1000;

        let payload = |access: &str, refresh: &str, at_exp: i64, rt_exp: i64| {
            json!({
                "account": {"uid": "uid-1", "nickname": "同一人"},
                "auth": {"accessToken": access, "refreshToken": refresh,
                         "expiresAt": at_exp, "refreshExpiresAt": rt_exp},
            })
            .to_string()
        };
        std::fs::write(
            auth.join("workbuddy-desktop.2026-09-01T00-00-00Z.1.a.info"),
            payload("AT-OLD", "RT-OLD", stale, stale),
        )
        .unwrap();
        std::fs::write(
            auth.join("workbuddy-desktop.2026-09-10T00-00-00Z.1.b.info"),
            payload("AT-NEW", "RT-NEW", fresh, fresh),
        )
        .unwrap();

        let found = discover_local_accounts_in(&auth, &backups);
        assert_eq!(found.len(), 1, "同区域同 uid 只保留一份");
        assert_eq!(
            get_str(&found[0].account, "access_token").as_deref(),
            Some("AT-NEW"),
            "必须保留凭证最新的那份，否则会导入已轮换失效的 refresh token"
        );
        assert_eq!(found[0].freshness, CredentialFreshness::Refreshable);
        assert_eq!(found[0].duplicate_count, 2);

        std::fs::remove_dir_all(&auth).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// 导入同一候选两次必须幂等：第二次原地刷新，不产生重复账号。
    #[test]
    fn reimporting_same_candidate_is_idempotent() {
        use crate::modules::auth_file::discover_local_accounts_in;

        let auth = temp_scan_dir("import-auth");
        let backups = temp_scan_dir("import-backup");
        let now = now_ms();
        let fresh = now + 30 * 24 * 3600 * 1000;

        std::fs::write(
            auth.join("workbuddy-desktop.2026-09-01T00-00-00Z.1.a.info"),
            json!({
                "account": {"uid": "uid-new", "nickname": "新账号"},
                "auth": {"accessToken": "AT-NEW", "refreshToken": "RT-NEW",
                         "expiresAt": fresh, "refreshExpiresAt": fresh},
            })
            .to_string(),
        )
        .unwrap();

        let found = discover_local_accounts_in(&auth, &backups);
        assert_eq!(found.len(), 1, "扫描应先看到 1 个候选");

        // 用账号库的同一套合并规则验证语义（不触碰真实账号库文件）。
        let mut store: Vec<Value> = vec![];
        let saved = upsert_collected_account(&mut store, found[0].account.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(saved["uid"], "uid-new");

        let again = upsert_collected_account(&mut store, found[0].account.clone());
        assert_eq!(store.len(), 1, "重复导入必须幂等");
        assert_eq!(again["id"], saved["id"], "本地 id 必须保持不变");

        std::fs::remove_dir_all(&auth).ok();
        std::fs::remove_dir_all(&backups).ok();
    }

    /// 造一个独立的临时扫描目录。
    fn temp_scan_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-account-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
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

/// 本机扫描的候选账号（脱敏，供界面展示与勾选）。
///
/// 序列化为 camelCase 以对接前端；Rust 侧保持 snake_case。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalImportCandidate {
    /// 候选在本次扫描结果中的稳定索引。
    pub index: usize,
    /// 来源文件的绝对路径（同时是跨扫描稳定的选择键）。
    pub path: String,
    /// 账号元数据（不含 token）。
    pub meta: Value,
    /// 来源类型键：current / snapshot / backup。
    pub source: String,
    /// 来源类型展示名。
    pub source_label: String,
    /// 凭证可用性键：refreshable / access_only / expired。
    pub freshness: String,
    /// 凭证可用性展示名。
    pub freshness_label: String,
    /// 同账号在本机共有多少份文件（>1 表示存在更旧的重复快照）。
    pub duplicate_count: usize,
    /// 是否已在账号库中（按 区域+uid 命中）。
    pub already_imported: bool,
    /// 账号库中同区域同 uid、但凭证更旧：导入会用这本新凭证覆盖。
    pub updates_stored: bool,
    /// 文件最后修改时间（毫秒）。
    pub modified_at: i64,
}

/// 凭证可用性的展示名。
fn freshness_label(freshness: CredentialFreshness) -> &'static str {
    match freshness {
        CredentialFreshness::Refreshable => "可保活",
        CredentialFreshness::AccessOnly => "仅 access 有效",
        CredentialFreshness::Expired => "凭证已过期",
    }
}

/// 本机扫描结果（含来源目录与文件数统计，供界面说明扫描范围）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalScanResult {
    /// 去重后的候选账号，按凭证到期时间降序。
    pub candidates: Vec<LocalImportCandidate>,
    /// 识别出的认证文件总数（含被去重掉的旧快照）。
    pub files_scanned: usize,
    /// 认证文件目录。
    pub auth_dir: String,
    /// 本工具备份目录。
    pub backup_dir: String,
    /// 可导入（凭证未完全过期）的候选数。
    pub usable: usize,
}

/// 扫描本机全部历史登录态（当前认证文件 + 客户端快照 + 本工具备份）。
///
/// 这是「从本机导入」的数据来源：`import_local_all` 只看两个固定文件名，
/// 因此每区域最多 1 个账号；本函数额外扫出历史快照里的账号。
/// 同一 (区域, uid) 的多份文件已在 `discover_local_accounts` 内按凭证新旧去重。
///
/// 结果按凭证到期时间降序（越新越靠前），并标注哪些账号已在库中。
pub fn scan_local_accounts() -> LocalScanResult {
    let stored = load_accounts();
    let discovery = auth_file::discover_local();
    let files_scanned = discovery.files_scanned;

    let candidates: Vec<LocalImportCandidate> = discovery
        .candidates
        .into_iter()
        .enumerate()
        .map(|(index, candidate)| {
            let meta = account_meta(&candidate.account);
            // 已在库中的判定与 upsert 的身份规则一致：同区域 + 同 uid。
            let candidate_uid = get_str(&candidate.account, "uid");
            let existing = candidate_uid.as_deref().and_then(|uid| {
                stored.iter().find(|a| {
                    Region::of(a) == candidate.region && get_str(a, "uid").as_deref() == Some(uid)
                })
            });
            let candidate_exp = auth_file::credential_expiry(&candidate.account).unwrap_or(0);
            let stored_exp = existing
                .and_then(|a| auth_file::credential_expiry(a))
                .unwrap_or(0);
            LocalImportCandidate {
                index,
                path: candidate.path.to_string_lossy().into_owned(),
                meta,
                source: candidate.source.key().to_string(),
                source_label: candidate.source.label().to_string(),
                freshness: candidate.freshness.key().to_string(),
                freshness_label: freshness_label(candidate.freshness).to_string(),
                duplicate_count: candidate.duplicate_count,
                already_imported: existing.is_some(),
                // 库里没有 → 是新增；库里有但凭证比本机旧 → 导入会刷新它。
                updates_stored: existing.is_some() && candidate_exp > stored_exp,
                modified_at: candidate.modified_at,
            }
        })
        .collect();

    let usable = candidates
        .iter()
        .filter(|c| c.freshness != CredentialFreshness::Expired.key())
        .count();

    LocalScanResult {
        candidates,
        files_scanned,
        auth_dir: discovery.auth_dir.to_string_lossy().into_owned(),
        backup_dir: discovery.backup_dir.to_string_lossy().into_owned(),
        usable,
    }
}

/// 按来源路径（或扫描索引）把本机候选账号并入账号库。
///
/// 选择键用**文件路径**而非索引：扫描与导入之间隔着一次前端往返，期间
/// 客户端可能刚好写入新的快照而改变排序，路径才是稳定标识。`indexes`
/// 同时接受扫描顺序索引，兼容按序号调用的调用方。
///
/// 缺 uid 的候选按「区域 + 真实邮箱」去重（非空 uid 始终优先，与
/// `upsert_collected_account` 同一套规则）；完全无法定身份的记录照旧入库。
pub fn import_local_selected(
    paths: &[String],
    indexes: &[usize],
) -> Result<LocalImportResult, String> {
    let selected: Vec<auth_file::LocalCandidate> = auth_file::discover_local_accounts()
        .into_iter()
        .enumerate()
        .filter(|(index, candidate)| {
            paths.iter().any(|p| candidate.path.to_string_lossy() == p.as_str())
                || indexes.contains(index)
        })
        .map(|(_, candidate)| candidate)
        .collect();

    if selected.is_empty() {
        return Err("未选择任何本机账号（或所选文件已不存在，请重新扫描）".to_string());
    }

    let mut accounts = load_accounts();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut outcomes: Vec<Value> = Vec::new();

    for candidate in selected {
        // upsert 命中已有身份时会保留本地 id，因此用「保存结果的 id 是否早已
        // 存在」判断本次是覆盖还是新增，可同时覆盖 uid 命中与邮箱兜底命中。
        let known_ids: Vec<String> = accounts.iter().filter_map(|a| get_str(a, "id")).collect();
        let saved = upsert_collected_account(&mut accounts, candidate.account);
        let saved_id = get_str(&saved, "id").unwrap_or_default();
        let was_update = known_ids.iter().any(|id| id == &saved_id);
        if was_update {
            updated += 1;
        } else {
            added += 1;
        }
        outcomes.push(json!({
            "name": account_display_name(&saved),
            "region": Region::of(&saved).label(),
            "source": candidate.source.key(),
            "file": candidate.file_name,
            "freshness": candidate.freshness.key(),
            "updated": was_update,
        }));
    }

    save_accounts(&accounts).map_err(|e| format!("保存账号库失败：{e}"))?;

    Ok(LocalImportResult {
        imported: added + updated,
        added,
        updated,
        outcomes,
    })
}

/// 本机导入的结果计数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImportResult {
    /// 本次实际写入账号库的数量（新增 + 覆盖）。
    pub imported: usize,
    /// 其中新增的账号数。
    pub added: usize,
    /// 其中覆盖刷新既有账号的数量。
    pub updated: usize,
    /// 逐个账号的结果明细。
    pub outcomes: Vec<Value>,
}

/// 从本机一键导入**所有可发现的区域**（国服 + 国际版）。
///
/// CodeBuddy / WorkBuddy 客户端把不同区域的登录态写在同目录的**不同文件**里：
///   workbuddy-desktop.info       国服
///   workbuddy-desktop-ai.info    国际版
/// 因此这里逐个探测，把能读到的全部并入账号库。
///
/// 只读**当前**登录态（每区域 1 个）。历史登录过的账号要靠
/// `scan_local_accounts` / `import_local_selected` 才能发现。
///
/// 返回本次实际导入（或更新）的账号元数据列表；全部未发现时给出可操作的错误。
pub fn import_local_all() -> Result<Vec<Value>, String> {
    let mut imported: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for region in Region::ALL {
        match crate::modules::auth_file::import_from_auth_file_for(region) {
            None => notes.push(format!("{}：未发现本机登录", region.label())),
            Some(acc) => {
                // 区域以认证文件为准：旧记录可能缺 domain，用文件名兜底标注。
                let mut acc = acc;
                if get_str(&acc, "domain").is_none() {
                    acc["domain"] = json!(region.auth_domain());
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
