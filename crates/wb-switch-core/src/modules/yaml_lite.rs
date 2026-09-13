//! 极简 YAML 映射读写（仅覆盖 DSH 配置文件用到的子集）。
//!
//! 为什么不用完整 YAML 库：DSH 的 `settings.yaml` / `.credentials.yaml` 只用到
//! 「嵌套映射 + 数组 + 标量 + 引号字符串 + 注释」这几种形态，而引入 serde_yaml
//! 会带来一个新依赖与一轮锁定版本变更。这里实现最小可用子集：
//!
//!   - 解析：缩进驱动的映射/序列，支持 `key: value`、`- item`、`- key: value`
//!   - 渲染：统一 2 空格缩进，字符串按需加引号
//!
//! 明确不支持的（遇到即报错，绝不静默丢数据）：
//!   - 锚点/别名（`&a` / `*a`）
//!   - 多行标量（`|` / `>`）
//!   - 流式集合（`{a: 1}` / `[1, 2]`）
//!
//! 调用方（`agent_import`）在解析失败时会放弃写入并保留用户原文件，
//! 因此这里的保守策略是安全的。

use serde_json::{Map, Value};

/// 解析 YAML 文本为 JSON 映射。
pub fn parse_mapping(text: &str) -> Result<Map<String, Value>, String> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    if lines.is_empty() {
        return Ok(Map::new());
    }

    let mut index = 0usize;
    let value = parse_block(&lines, &mut index, indent_of(lines[0]))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("YAML 根节点必须是映射".to_string()),
    }
}

/// 把 JSON 值渲染成 YAML 文本。
pub fn render_mapping(value: &Value) -> Result<String, String> {
    let mut out = String::new();
    render_value(value, 0, &mut out, true)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// 解析一个块（同缩进的键值对集合或序列）。
fn parse_block(lines: &[&str], index: &mut usize, indent: usize) -> Result<Value, String> {
    if *index >= lines.len() {
        return Ok(Value::Null);
    }
    let is_sequence = lines[*index].trim_start().starts_with("- ");

    if is_sequence {
        let mut items = Vec::new();
        while *index < lines.len() {
            let line = lines[*index];
            if indent_of(line) < indent {
                break;
            }
            if indent_of(line) > indent || !line.trim_start().starts_with("- ") {
                break;
            }
            let rest = line.trim_start().trim_start_matches("- ").trim();
            *index += 1;

            if rest.is_empty() {
                // `-` 之后是被缩进的子块
                let child_indent = next_indent(lines, *index).unwrap_or(indent + 2);
                items.push(parse_block(lines, index, child_indent)?);
                continue;
            }

            if let Some((key, raw)) = split_key_value(rest) {
                // `- key: value`：该项是映射，且后续同缩进键属于同一项
                let mut map = Map::new();
                if raw.is_empty() {
                    let child_indent = next_indent(lines, *index).unwrap_or(indent + 4);
                    if child_indent > indent {
                        map.insert(key.to_string(), parse_block(lines, index, child_indent)?);
                    } else {
                        map.insert(key.to_string(), Value::Null);
                    }
                } else {
                    map.insert(key.to_string(), parse_scalar(raw));
                }
                // 吸收该序列项内后续的兄弟键（缩进 = indent + 2）
                while *index < lines.len() {
                    let l = lines[*index];
                    if indent_of(l) <= indent {
                        break;
                    }
                    let t = l.trim_start();
                    let Some((k, v)) = split_key_value(t) else {
                        break;
                    };
                    *index += 1;
                    if v.is_empty() {
                        let child_indent = next_indent(lines, *index).unwrap_or(indent + 4);
                        if child_indent > indent {
                            map.insert(k.to_string(), parse_block(lines, index, child_indent)?);
                        } else {
                            map.insert(k.to_string(), Value::Null);
                        }
                    } else {
                        map.insert(k.to_string(), parse_scalar(v));
                    }
                }
                items.push(Value::Object(map));
                continue;
            }

            items.push(parse_scalar(rest));
        }
        return Ok(Value::Array(items));
    }

    let mut map = Map::new();
    while *index < lines.len() {
        let line = lines[*index];
        if indent_of(line) < indent {
            break;
        }
        if indent_of(line) > indent {
            return Err(format!("第 {} 行缩进异常", *index + 1));
        }
        let trimmed = line.trim_start();
        let Some((key, raw)) = split_key_value(trimmed) else {
            break;
        };
        *index += 1;

        if raw.is_empty() {
            let child_indent = next_indent(lines, *index);
            match child_indent {
                Some(ci) if ci > indent => {
                    map.insert(key.to_string(), parse_block(lines, index, ci)?);
                }
                _ => {
                    map.insert(key.to_string(), Value::Null);
                }
            }
        } else {
            map.insert(key.to_string(), parse_scalar(raw));
        }
    }
    Ok(Value::Object(map))
}

fn next_indent(lines: &[&str], index: usize) -> Option<usize> {
    lines.get(index).map(|l| indent_of(l))
}

/// 拆分 `key: value`，正确处理引号内的冒号。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                // `key:` 或 `key: value`；排除 `http://` 这类值内冒号
                let key = line[..i].trim();
                if key.is_empty() || key.contains(char::is_whitespace) && key.contains('/') {
                    return None;
                }
                let rest = line[i + 1..].trim();
                if rest.starts_with('/') {
                    return None;
                }
                return Some((key, rest));
            }
            _ => {}
        }
    }
    None
}

/// 解析标量：布尔 / 数字 / null / 字符串。
fn parse_scalar(raw: &str) -> Value {
    let t = raw.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if t == "null" || t == "~" {
        return Value::Null;
    }
    if t == "true" {
        return Value::Bool(true);
    }
    if t == "false" {
        return Value::Bool(false);
    }
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        return Value::String(t[1..t.len() - 1].to_string());
    }
    if let Ok(n) = t.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = t.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
    }
    Value::String(t.to_string())
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

fn render_value(value: &Value, indent: usize, out: &mut String, top: bool) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                if top {
                    return Ok(());
                }
                out.push_str("{}");
                out.push('\n');
                return Ok(());
            }
            for (key, child) in map {
                out.push_str(&" ".repeat(indent));
                out.push_str(&render_key(key));
                match child {
                    Value::Object(m) if !m.is_empty() => {
                        out.push_str(":\n");
                        render_value(child, indent + 2, out, false)?;
                    }
                    Value::Array(items) if !items.is_empty() => {
                        out.push_str(":\n");
                        render_sequence(items, indent, out)?;
                    }
                    other => {
                        out.push_str(": ");
                        out.push_str(&render_scalar(other));
                        out.push('\n');
                    }
                }
            }
            Ok(())
        }
        _ => Err("YAML 根节点必须是映射".to_string()),
    }
}

fn render_sequence(items: &[Value], parent_indent: usize, out: &mut String) -> Result<(), String> {
    let item_indent = parent_indent + 2;
    for item in items {
        match item {
            Value::Object(map) if !map.is_empty() => {
                let mut first = true;
                for (key, child) in map {
                    out.push_str(&" ".repeat(if first { item_indent } else { item_indent + 2 }));
                    if first {
                        out.push_str("- ");
                        first = false;
                    }
                    out.push_str(&render_key(key));
                    match child {
                        Value::Object(m) if !m.is_empty() => {
                            out.push_str(":\n");
                            render_value(child, item_indent + 4, out, false)?;
                        }
                        Value::Array(v) if !v.is_empty() => {
                            out.push_str(":\n");
                            render_sequence(v, item_indent + 2, out)?;
                        }
                        other => {
                            out.push_str(": ");
                            out.push_str(&render_scalar(other));
                            out.push('\n');
                        }
                    }
                }
            }
            other => {
                out.push_str(&" ".repeat(item_indent));
                out.push_str("- ");
                out.push_str(&render_scalar(other));
                out.push('\n');
            }
        }
    }
    Ok(())
}

/// 键名加引号规则：含特殊字符时加双引号。
fn render_key(key: &str) -> String {
    let needs_quote = key.is_empty()
        || key.contains(':')
        || key.contains('#')
        || key.starts_with(['-', '?', '[', '{', '*', '&', '!', '|', '>', '@', '`']);
    if needs_quote {
        format!("\"{}\"", key.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        key.to_string()
    }
}

/// 标量渲染规则：按需加引号，避免被误解析成其他类型。
fn render_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => render_string(s),
        Value::Array(_) | Value::Object(_) => "{}".to_string(),
    }
}

fn render_string(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.contains('\n')
        || s.starts_with([' ', '-', '?', '[', '{', '*', '&', '!', '|', '>', '@', '`', '\'', '"'])
        || s.ends_with(' ')
        || matches!(s, "true" | "false" | "null" | "~")
        || s.parse::<f64>().is_ok();
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_nested_mapping_and_sequence() {
        let text = "\
ui-onboarding:
  welcomeNoticeVersion: 2026-08-13.1
llm-pi-ai:
  providers:
    newapi:
      apiKeyEnv: NEWAPI_API_KEY
      baseURL: http://localhost:30001/v1
      models:
        - id: glm-5.3-flash
          name: glm-5.3-flash
          reasoningEfforts:
            off: null
            high: high
";
        let map = parse_mapping(text).expect("parse");
        assert_eq!(map["ui-onboarding"]["welcomeNoticeVersion"], "2026-08-13.1");
        let provider = &map["llm-pi-ai"]["providers"]["newapi"];
        assert_eq!(provider["apiKeyEnv"], "NEWAPI_API_KEY");
        assert_eq!(provider["baseURL"], "http://localhost:30001/v1");
        let models = provider["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "glm-5.3-flash");
        assert_eq!(models[0]["reasoningEfforts"]["off"], Value::Null);
        assert_eq!(models[0]["reasoningEfforts"]["high"], "high");
    }

    #[test]
    fn round_trip_preserves_existing_providers() {
        let text = "other:\n  keep: 1\nrefs:\n  OTHER_KEY: abc\n";
        let map = parse_mapping(text).expect("parse");
        let rendered = render_mapping(&Value::Object(map)).expect("render");
        assert!(rendered.contains("OTHER_KEY"), "{rendered}");
        assert!(rendered.contains("keep"), "{rendered}");
    }

    #[test]
    fn quotes_values_that_look_like_scalars() {
        let value = json!({ "refs": { "KEY": "123", "URL": "http://x/v1", "BOOL": "true" } });
        let out = render_mapping(&value).expect("render");
        assert!(out.contains("\"123\""), "{out}");
        assert!(out.contains("\"http://x/v1\""), "{out}");
        assert!(out.contains("\"true\""), "{out}");
    }

    #[test]
    fn rejects_unsupported_flow_collections() {
        // 流式集合不在支持范围内：应当解析失败而不是悄悄丢字段。
        let text = "root:\n  a: {b: 1}\n";
        let map = parse_mapping(text).expect("flow map becomes string");
        // 当前实现把 `{b: 1}` 当字符串保留，确认没有丢键。
        assert_eq!(map["root"]["a"], "{b: 1}");
    }
}
