<!-- TRELLIS:START -->
# 编码契约（Coding Contracts）

These instructions are for AI assistants working in this project.

本仓库曾由 Trellis 工具初始化。Trellis 的工作流骨架（`workflow.md`、`workspace/`、
`spec/cli/`、`.agents/skills/`、`.codex/agents/`）与 spec 目录均未随仓库分发，
相关引用已于 2026-09-12 清理。

当前生效的编码约定见下方各节（UI Component Policy、Git Commit Language）。

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
