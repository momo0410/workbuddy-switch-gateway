<!-- TRELLIS:START -->
# 编码契约（Coding Contracts）

These instructions are for AI assistants working in this project.

本仓库曾由 Trellis 工具初始化。Trellis 的工作流骨架（`workflow.md`、`workspace/`、
`spec/cli/`、`.agents/skills/`、`.codex/agents/`）与 spec 目录均未随仓库分发，
相关引用已于 2026-09-12 清理。

当前生效的编码约定见下方各节（UI Component Policy、Git Commit Language）。

## Shell Policy（强制）

- 本仓库运行环境为 Windows。所有 shell 命令**必须**显式使用 PowerShell（`pwsh` / `powershell`），
  禁止调用 `bash` / `sh` / `zsh` / WSL 及其启动器 `C:\WINDOWS\system32\bash.exe`。
- 调用命令行工具时必须显式指定 shell 为 PowerShell，不要依赖宿主默认值：默认值在
  仅安装了 WSL 启动器、未安装 Linux 发行版的机器上会落到 `bash` 并直接失败。
- 路径一律使用 Windows 形式（`D:\workbuddy2api\...`）、`\` 作为分隔符；环境变量用
  `$env:NAME`。禁止 `curl | sh`、`export VAR=...`、`/dev/null` 等 POSIX 写法。
- 脚本、构建、测试命令一律写成 PowerShell 形式（如 `Get-ChildItem`、`Remove-Item -LiteralPath`、
  `$LASTEXITCODE`）。仓库内已有 `scripts/*.ps1` 的，优先复用而不是重写为 shell 脚本。
- 若某工具确实只提供 POSIX 方式，先确认 `pwsh` 下无等价方案，再在 `AGENTS.md` 记录原因，
  不得静默改用 bash。

Token 统计与网关的接口契约可直接查阅实现本身：

- `crates/wb-switch-core/src/modules/token_stats.rs` — Token 统计聚合
  （`get_statistics(days: Option<i64>)`）
- `crates/wb-switch-server/src/api.rs` — HTTP 路由（含 `GET /api/token-stats`）
- `src-tauri/src/commands.rs` — 对应的 Tauri 命令包装

<!-- TRELLIS:END -->

## UI Component Policy

- For frontend UI, prefer the project's existing shadcn components and compose them before writing custom interactive primitives.
- If a required component is missing, add the matching shadcn/Radix component and wrap it under `src/components/ui/` so styling, accessibility, focus management, and behavior stay consistent.
- Write a custom component only when shadcn components and their composition APIs cannot satisfy the requirement. Record the reason before doing so.
- Custom UI must still reuse the project's Rhea theme tokens, spacing, radii, states, and accessibility conventions. Do not substitute native interactive shortcuts such as `details/summary` when an appropriate shadcn component exists.

## Git Commit Language

- Use Conventional Commit type prefixes such as `feat:`, `fix:`, and `docs:`.
- Write the commit subject and body in Chinese by default. Use English only when the user explicitly requests it.
