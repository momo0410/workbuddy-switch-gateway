//! 进程控制：检测、关闭、启动 WorkBuddy。
//!
//! 对照 server.py `is_workbuddy_running` / `_wait_process_gone` /
//! `close_workbuddy` / `launch_workbuddy`。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::modules::auth_file;
use crate::modules::config;

/// 创建子进程命令。Windows 上加 CREATE_NO_WINDOW，避免每次执行 tasklist/powershell
/// 等控制台命令时闪出 cmd 黑窗口（GUI 应用卡顿/跳动的主因）。
pub(crate) fn cmd_builder(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// 运行命令并等待退出，超时则 kill 并返回 None（对应 Python `subprocess.run(timeout=...)`）。
///
/// 注意：stdout/stderr 必须与等待并发读取——先等退出再读会在输出超过
/// 64KB 管道缓冲时死锁（`ps -axo` 全量输出在进程多的机器上很容易超过），
/// 子进程写满阻塞永不退出，最终被超时 kill 并返回 None。
pub(crate) fn run_cmd_timeout(program: &str, args: &[&str], timeout_secs: u64) -> Option<Output> {
    let mut child = cmd_builder(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;

    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut status = None;
    while status.is_none() {
        match child.try_wait() {
            Ok(Some(s)) => status = Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // 管道随进程退出关闭，读线程随即结束。
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    let out = out_reader.join().unwrap_or_default();
    let err = err_reader.join().unwrap_or_default();
    Some(Output {
        status: status.unwrap(),
        stdout: out,
        stderr: err,
    })
}

fn image_stem(name: &str) -> &str {
    let name = name.trim();
    if name.len() >= 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        name[..name.len() - 4].trim()
    } else {
        name
    }
}

/// Windows 路径可能含 `\`；在非 Windows 上 `Path::file_name` 不会按 `\` 切分。
fn image_name_from_path_str(s: &str) -> &str {
    s.rsplit(['\\', '/']).next().unwrap_or(s).trim()
}

/// 本工具自身的映像名（忽略 .exe、大小写）。
pub(crate) fn is_self_image_name(name: &str) -> bool {
    let stem = image_stem(image_name_from_path_str(name));
    stem.eq_ignore_ascii_case("workbuddy-switch") || stem.eq_ignore_ascii_case("wb-switch")
}

/// 精确匹配 WorkBuddy / CodeBuddy 映像，禁止子串命中 workbuddy-switch。
fn is_workbuddy_image_name(name: &str) -> bool {
    let stem = image_stem(image_name_from_path_str(name));
    stem.eq_ignore_ascii_case("WorkBuddy") || stem.eq_ignore_ascii_case("CodeBuddy")
}

pub(crate) fn is_crashpad_helper_name(name: &str) -> bool {
    image_name_from_path_str(name)
        .to_ascii_lowercase()
        .contains("crashpad_handler")
}

/// 解析卸载项 DisplayIcon：去掉引号和可选的 `,0` 图标索引。
pub(crate) fn parse_windows_display_icon(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let path = if let Some(rest) = s.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            rest[..end].trim()
        } else {
            rest.trim().trim_matches('"').trim()
        }
    } else {
        s.trim_matches('"').trim()
    };
    let path = if let Some((p, idx)) = path.rsplit_once(',') {
        if idx.trim().parse::<i32>().is_ok() {
            p.trim().trim_matches('"').trim()
        } else {
            path
        }
    } else {
        path
    };
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn windows_path(parts: &[&str]) -> PathBuf {
    PathBuf::from(parts.join("\\"))
}

/// 环境变量默认目录 + 盘符扫描候选（不访问文件系统，便于非 Windows 单测）。
fn windows_fallback_exe_candidates(
    local_appdata: Option<&str>,
    program_files: Option<&str>,
    program_files_x86: Option<&str>,
    username: Option<&str>,
    drives: &[char],
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.iter().any(|e| e == &p) {
            out.push(p);
        }
    };
    let mut push_win_dir = |parts: &[&str]| {
        let mut wb = parts.to_vec();
        wb.push("WorkBuddy.exe");
        push(windows_path(&wb));
        let mut cb = parts.to_vec();
        cb.push("CodeBuddy.exe");
        push(windows_path(&cb));
    };

    if let Some(local) = local_appdata.map(str::trim).filter(|s| !s.is_empty()) {
        push_win_dir(&[local, "Programs", "WorkBuddy"]);
        push_win_dir(&[local, "Programs", "CodeBuddy"]);
    }
    if let Some(pf) = program_files.map(str::trim).filter(|s| !s.is_empty()) {
        push_win_dir(&[pf, "WorkBuddy"]);
        push_win_dir(&[pf, "CodeBuddy"]);
    }
    if let Some(pf86) = program_files_x86.map(str::trim).filter(|s| !s.is_empty()) {
        push_win_dir(&[pf86, "WorkBuddy"]);
        push_win_dir(&[pf86, "CodeBuddy"]);
    }

    let user = username.map(str::trim).filter(|s| !s.is_empty());
    for drive in drives {
        let letter = drive.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            continue;
        }
        let root = format!("{letter}:");
        if let Some(user) = user {
            push_win_dir(&[
                &root,
                "Users",
                user,
                "AppData",
                "Local",
                "Programs",
                "WorkBuddy",
            ]);
            push_win_dir(&[
                &root,
                "Users",
                user,
                "AppData",
                "Local",
                "Programs",
                "CodeBuddy",
            ]);
        }
        push_win_dir(&[&root, "Program Files", "WorkBuddy"]);
        push_win_dir(&[&root, "Program Files", "CodeBuddy"]);
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowsProcessRow {
    pub(crate) pid: u32,
    pub(crate) name: String,
    pub(crate) exe_path: Option<PathBuf>,
}

fn parse_simple_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

pub(crate) fn parse_tasklist_csv(stdout: &str) -> Vec<WindowsProcessRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with("INFO:") {
                return None;
            }
            let cols = parse_simple_csv_line(line);
            let name = cols.first()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let pid = cols.get(1)?.trim().parse::<u32>().ok()?;
            Some(WindowsProcessRow {
                pid,
                name,
                exe_path: None,
            })
        })
        .collect()
}

pub(crate) fn parse_windows_process_rows(stdout: &str) -> Vec<WindowsProcessRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, '|');
            let pid = parts.next()?.trim().parse::<u32>().ok()?;
            let name = parts.next().unwrap_or("").trim().to_string();
            let path_s = parts.next().unwrap_or("").trim();
            let exe_path = if path_s.is_empty() {
                None
            } else {
                Some(PathBuf::from(path_s))
            };
            Some(WindowsProcessRow {
                pid,
                name,
                exe_path,
            })
        })
        .collect()
}

fn keep_windows_workbuddy_row(row: &WindowsProcessRow) -> bool {
    let path_s = row
        .exe_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file_name = image_name_from_path_str(&path_s);
    if is_self_image_name(&row.name) || is_self_image_name(file_name) {
        return false;
    }
    if is_crashpad_helper_name(&row.name) || is_crashpad_helper_name(file_name) {
        return false;
    }
    is_workbuddy_image_name(&row.name) || is_workbuddy_image_name(file_name)
}

fn filter_windows_workbuddy_rows(rows: &[WindowsProcessRow]) -> Vec<WindowsProcessRow> {
    let mut out = Vec::new();
    for row in rows {
        if !keep_windows_workbuddy_row(row) {
            continue;
        }
        if out.iter().any(|r: &WindowsProcessRow| r.pid == row.pid) {
            continue;
        }
        out.push(row.clone());
    }
    out
}

fn parse_windows_registry_path_lines(stdout: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let Some(parsed) = parse_windows_display_icon(line) else {
            continue;
        };
        let name = image_name_from_path_str(&parsed);
        if is_self_image_name(name) {
            continue;
        }
        if !name.is_empty() && !is_workbuddy_image_name(name) {
            continue;
        }
        let pb = PathBuf::from(parsed);
        if !out.iter().any(|e| e == &pb) {
            out.push(pb);
        }
    }
    out
}

/// 路径最后一段必须是 WorkBuddy/CodeBuddy 映像名（忽略 .exe）。
fn is_workbuddy_exe_file_name(path: &Path) -> bool {
    let owned = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| image_name_from_path_str(&path.to_string_lossy()).to_string());
    is_workbuddy_image_name(&owned)
}

fn is_existing_workbuddy_exe(path: &Path) -> bool {
    path.is_file() && is_workbuddy_exe_file_name(path)
}

/// 记住已存在的 exe；缓存已是同一路径则不重复写。
fn persist_workbuddy_exe(path: &Path) {
    if !is_existing_workbuddy_exe(path) {
        return;
    }
    if config::load_workbuddy_exe_cache().as_deref() == Some(path) {
        return;
    }
    let _ = config::save_workbuddy_exe_cache(path);
}

/// Windows：执行 PowerShell 并取 stdout。
pub(crate) fn ps_output(script: &str, timeout_secs: u64) -> Option<String> {
    let out = run_cmd_timeout(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        timeout_secs,
    )?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

pub(crate) fn windows_tasklist_image_rows(image: &str) -> Vec<WindowsProcessRow> {
    let filter = format!("IMAGENAME eq {image}");
    let Some(out) = run_cmd_timeout("tasklist", &["/FI", &filter, "/FO", "CSV", "/NH"], 5) else {
        return Vec::new();
    };
    parse_tasklist_csv(&String::from_utf8_lossy(&out.stdout))
}

/// 精确映像名收集 WorkBuddy/CodeBuddy PID，排除本工具、自身 PID 与 crashpad。
fn windows_workbuddy_process_rows() -> Vec<WindowsProcessRow> {
    let self_pid = std::process::id();
    let mut rows = Vec::new();
    rows.extend(windows_tasklist_image_rows("WorkBuddy.exe"));
    rows.extend(windows_tasklist_image_rows("CodeBuddy.exe"));
    filter_windows_workbuddy_rows(&rows)
        .into_iter()
        .filter(|r| r.pid != self_pid)
        .collect()
}

fn is_windows_pid_running(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    match run_cmd_timeout("tasklist", &["/FI", &filter, "/FO", "CSV", "/NH"], 5) {
        Some(out) => parse_tasklist_csv(&String::from_utf8_lossy(&out.stdout))
            .iter()
            .any(|r| r.pid == pid),
        None => true,
    }
}

pub(crate) fn wait_windows_pids_gone(pids: &[u32], timeout: Duration) -> Vec<u32> {
    if pids.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + timeout;
    loop {
        let alive: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|pid| is_windows_pid_running(*pid))
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub(crate) fn existing_windows_drives() -> Vec<char> {
    ('A'..='Z')
        .filter(|c| Path::new(&format!(r"{c}:\")).exists())
        .collect()
}

fn windows_running_workbuddy_exe() -> Option<PathBuf> {
    let script = "Get-Process -Name WorkBuddy,CodeBuddy -ErrorAction SilentlyContinue | \
         ForEach-Object { $p = ''; try { $p = $_.Path } catch {}; '{0}|{1}|{2}' -f $_.Id, $_.ProcessName, $p }";
    let stdout = ps_output(script, 5)?;
    for row in filter_windows_workbuddy_rows(&parse_windows_process_rows(&stdout)) {
        if let Some(p) = row.exe_path {
            if is_existing_workbuddy_exe(&p) {
                return Some(p);
            }
        }
    }
    None
}

fn windows_registry_exe_candidates() -> Vec<PathBuf> {
    let script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$out = @()
$appNames = @('WorkBuddy.exe','CodeBuddy.exe')
$appHives = @(
  'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths',
  'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths',
  'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\App Paths'
)
foreach ($hive in $appHives) {
  foreach ($n in $appNames) {
    $key = Join-Path $hive $n
    $props = Get-ItemProperty -LiteralPath $key
    if ($props) {
      $def = $props.'(default)'
      if ($def) { $out += [string]$def }
      if ($props.Path) { $out += [string](Join-Path $props.Path $n) }
    }
  }
}
$unHives = @(
  'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
  'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall',
  'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
)
foreach ($hive in $unHives) {
  Get-ChildItem -LiteralPath $hive | ForEach-Object {
    $dn = $_.GetValue('DisplayName')
    if (-not $dn) { return }
    $dnl = [string]$dn
    if ($dnl -match 'workbuddy-switch|wb-switch') { return }
    if ($dnl -notmatch 'WorkBuddy|CodeBuddy') { return }
    $icon = $_.GetValue('DisplayIcon')
    if ($icon) { $out += [string]$icon }
    $loc = $_.GetValue('InstallLocation')
    if ($loc) {
      $out += [string](Join-Path $loc 'WorkBuddy.exe')
      $out += [string](Join-Path $loc 'CodeBuddy.exe')
    }
  }
}
$out | ForEach-Object { $_ }
"#;
    let Some(stdout) = ps_output(script, 8) else {
        return Vec::new();
    };
    parse_windows_registry_path_lines(&stdout)
}

/// Windows：动态查找 WorkBuddy 可执行文件路径。
///
/// 顺序：运行中进程 Path → 上次成功路径缓存 → 注册表 App Paths / Uninstall
/// （含 WOW6432Node、DisplayIcon）→ LOCALAPPDATA/Program Files 与各盘符常见目录。
/// 命中且文件存在则写入缓存；缓存指向丢失文件则丢弃。
pub fn windows_workbuddy_exe_path() -> Option<PathBuf> {
    if let Some(p) = windows_running_workbuddy_exe() {
        persist_workbuddy_exe(&p);
        return Some(p);
    }

    if let Some(cached) = config::load_workbuddy_exe_cache() {
        if is_existing_workbuddy_exe(&cached) {
            return Some(cached);
        }
        config::clear_workbuddy_exe_cache();
    }

    for p in windows_registry_exe_candidates() {
        if is_existing_workbuddy_exe(&p) {
            persist_workbuddy_exe(&p);
            return Some(p);
        }
    }

    let local = std::env::var("LOCALAPPDATA").ok();
    let pf = std::env::var("PROGRAMFILES").ok();
    let pf86 = std::env::var("PROGRAMFILES(X86)").ok();
    let user = std::env::var("USERNAME").ok();
    let drives = existing_windows_drives();
    let candidates = windows_fallback_exe_candidates(
        local.as_deref(),
        pf.as_deref(),
        pf86.as_deref(),
        user.as_deref(),
        &drives,
    );
    for p in candidates {
        if is_existing_workbuddy_exe(&p) {
            persist_workbuddy_exe(&p);
            return Some(p);
        }
    }
    None
}


/// WorkBuddy 是否在运行。
pub fn is_workbuddy_running() -> bool {
    !windows_workbuddy_process_rows().is_empty()
}

/// 轮询等待 WorkBuddy 进程全部退出，返回是否已退出。对照 `_wait_process_gone`。
pub fn wait_process_gone(timeout_secs: f64) -> bool {
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
    while Instant::now() < deadline {
        if !is_workbuddy_running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    !is_workbuddy_running()
}

/// 关闭 WorkBuddy：优雅退出 → 超时后强杀 → 确认进程消失。对照 `close_workbuddy`。
pub fn close_workbuddy(timeout_secs: i64) -> Result<(), String> {
    close_workbuddy_windows(timeout_secs)
}

/// 先对目标 PID `taskkill /PID /T`（无 /F），超时再 `/F`；按 PID 等待，不按名称子串。
fn close_workbuddy_windows(timeout_secs: i64) -> Result<(), String> {
    let rows = windows_workbuddy_process_rows();
    if rows.is_empty() {
        return Ok(());
    }
    let pids: Vec<u32> = rows.iter().map(|r| r.pid).collect();
    for pid in &pids {
        let pid_s = pid.to_string();
        let _ = run_cmd_timeout("taskkill", &["/PID", &pid_s, "/T"], 10);
    }

    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_secs.max(1) as u64);
    let graceful_budget = Duration::from_secs(8).min(timeout);
    let remaining = wait_windows_pids_gone(&pids, graceful_budget);
    if remaining.is_empty() {
        return Ok(());
    }

    for pid in &remaining {
        let pid_s = pid.to_string();
        let _ = run_cmd_timeout("taskkill", &["/PID", &pid_s, "/T", "/F"], 10);
    }
    let rest = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(1));
    let leftover = wait_windows_pids_gone(&remaining, rest);
    if leftover.is_empty() {
        return Ok(());
    }
    Err("WorkBuddy 进程无法关闭，请手动结束 WorkBuddy/CodeBuddy 进程".to_string())
}

/// 启动 WorkBuddy。失败返回可读错误。对照 `launch_workbuddy`。
///
/// progress 参数已被忽略，保留签名以兼容既有调用点。
pub fn launch_workbuddy(progress: Option<&dyn Fn(&str)>) -> Result<(), String> {
    let _ = progress;
    let app = auth_file::workbuddy_app_path();
    let exe = if app
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
    {
        app.clone()
    } else {
        app.join("WorkBuddy.exe")
    };
    if !exe.exists() {
        return Err(format!(
            "未找到 WorkBuddy 程序（尝试路径: {}）。请在 Windows 上打开 WorkBuddy 后重试。",
            exe.display()
        ));
    }
    persist_workbuddy_exe(&exe);
    cmd_builder(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 WorkBuddy 失败: {e}（路径: {}）", exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_image_names_are_detected() {
        assert!(is_self_image_name("workbuddy-switch"));
        assert!(is_self_image_name("workbuddy-switch.exe"));
        assert!(is_self_image_name("WB-SWITCH.EXE"));
        assert!(is_self_image_name("wb-switch"));
        assert!(is_self_image_name(r"C:\apps\workbuddy-switch.exe"));
        assert!(!is_self_image_name("WorkBuddy.exe"));
        assert!(!is_self_image_name("WorkBuddy"));
        assert!(!is_self_image_name("CodeBuddy.exe"));
    }

    #[test]
    fn workbuddy_image_name_is_exact_not_substring() {
        assert!(is_workbuddy_image_name("WorkBuddy.exe"));
        assert!(is_workbuddy_image_name("workbuddy"));
        assert!(is_workbuddy_image_name("CodeBuddy.exe"));
        assert!(is_workbuddy_image_name("CODEBUDDY"));
        assert!(!is_workbuddy_image_name("workbuddy-switch.exe"));
        assert!(!is_workbuddy_image_name("wb-switch"));
        assert!(!is_workbuddy_image_name("WorkBuddy Helper.exe"));
        assert!(!is_workbuddy_image_name("MyWorkBuddy.exe"));
        assert!(is_workbuddy_exe_file_name(Path::new("WorkBuddy.exe")));
        assert!(is_workbuddy_exe_file_name(Path::new(
            r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe"
        )));
        assert!(!is_workbuddy_exe_file_name(Path::new(
            "workbuddy-switch.exe"
        )));
        assert!(!is_workbuddy_exe_file_name(Path::new("Uninstall.exe")));
    }

    #[test]
    fn parse_display_icon_strips_quotes_and_index() {
        assert_eq!(
            parse_windows_display_icon(
                r#""D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe,0""#
            ),
            Some(r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe".to_string())
        );
        assert_eq!(
            parse_windows_display_icon(
                r#""D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe",0"#
            ),
            Some(r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe".to_string())
        );
        assert_eq!(
            parse_windows_display_icon(r"C:\Program Files\WorkBuddy\WorkBuddy.exe"),
            Some(r"C:\Program Files\WorkBuddy\WorkBuddy.exe".to_string())
        );
        assert_eq!(parse_windows_display_icon("  "), None);
    }

    #[test]
    fn fallback_candidates_include_d_drive_for_zhou() {
        let cands = windows_fallback_exe_candidates(
            Some(r"C:\Users\Zhou\AppData\Local"),
            Some(r"C:\Program Files"),
            Some(r"C:\Program Files (x86)"),
            Some("Zhou"),
            &['C', 'D'],
        );
        let want = r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe";
        assert!(
            cands.iter().any(|p| p.to_string_lossy() == want),
            "missing {want} in {cands:?}"
        );
        let d_pf = r"D:\Program Files\WorkBuddy\WorkBuddy.exe";
        assert!(
            cands.iter().any(|p| p.to_string_lossy() == d_pf),
            "missing {d_pf} in {cands:?}"
        );
        let local_default = r"C:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe";
        assert!(
            cands.iter().any(|p| p.to_string_lossy() == local_default),
            "missing {local_default} in {cands:?}"
        );
    }

    #[test]
    fn process_rows_drop_self_and_crashpad() {
        let stdout = "\
1001|workbuddy-switch|C:\\apps\\workbuddy-switch.exe
1002|wb-switch|
1003|WorkBuddy|D:\\Users\\Zhou\\AppData\\Local\\Programs\\WorkBuddy\\WorkBuddy.exe
1004|crashpad_handler|C:\\x\\crashpad_handler.exe
1005|CodeBuddy|
1006|WorkBuddy Helper|
";
        let kept = filter_windows_workbuddy_rows(&parse_windows_process_rows(stdout));
        let pids: Vec<u32> = kept.iter().map(|r| r.pid).collect();
        assert_eq!(pids, vec![1003, 1005]);
    }

    #[test]
    fn only_switcher_process_is_not_workbuddy_running() {
        let stdout = "4400|workbuddy-switch.exe|C:\\Users\\Zhou\\AppData\\Local\\Programs\\wb-switch\\workbuddy-switch.exe\n";
        let kept = filter_windows_workbuddy_rows(&parse_windows_process_rows(stdout));
        assert!(kept.is_empty());

        let csv = "\
\"workbuddy-switch.exe\",\"4400\",\"Console\",\"1\",\"10,000 K\"
\"wb-switch.exe\",\"4401\",\"Console\",\"1\",\"8,000 K\"
";
        let kept = filter_windows_workbuddy_rows(&parse_tasklist_csv(csv));
        assert!(kept.is_empty());
    }

    #[test]
    fn tasklist_csv_keeps_exact_workbuddy_image() {
        let csv = "\
\"WorkBuddy.exe\",\"1234\",\"Console\",\"1\",\"50,123 K\"
\"workbuddy-switch.exe\",\"4400\",\"Console\",\"1\",\"10,000 K\"
INFO: No tasks are running which match the specified criteria.
";
        let kept = filter_windows_workbuddy_rows(&parse_tasklist_csv(csv));
        assert_eq!(kept.iter().map(|r| r.pid).collect::<Vec<_>>(), vec![1234]);
    }

    #[test]
    fn registry_lines_parse_display_icon_and_skip_self() {
        let stdout = r#"
"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe,0"
C:\Users\Zhou\AppData\Local\Programs\workbuddy-switch\workbuddy-switch.exe
D:\Program Files\WorkBuddy\Uninstall.exe
D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe
"#;
        let paths = parse_windows_registry_path_lines(stdout);
        let s: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            s,
            vec![r"D:\Users\Zhou\AppData\Local\Programs\WorkBuddy\WorkBuddy.exe".to_string(),]
        );
    }

}
