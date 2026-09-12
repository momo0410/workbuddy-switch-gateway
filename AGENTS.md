<!-- TRELLIS:START -->
# 编码契约（Coding Contracts）

These instructions are for AI assistants working in this project.

编写代码前，请先阅读 `.trellis/spec/` 下与当前改动层次相关的契约：

- `.trellis/spec/guides/ui-component-guidelines.md` — 前端 UI 规范（shadcn 优先、Rhea 主题、
  统计页布局与图表语义、交互要求）
- `.trellis/spec/wb-switch-core/backend/token-statistics.md` — Token 统计的跨层接口契约
  （核心签名 / Tauri 命令 / HTTP 路由 / 数据源 / 响应结构）

历史任务的开发记录不再随仓库分发，如需查阅请向维护者索取。

> 2026-09-12 整理说明：本项目曾由 Trellis 工具初始化，但 `workflow.md`、
> `workspace/`、`spec/cli/`、`.agents/skills/`、`.codex/agents/` 均不存在于本仓库，
> 相关引用已清理。本块仅保留仍然生效的编码契约指引。

<!-- TRELLIS:END -->

## UI Component Policy

- For frontend UI, prefer the project's existing shadcn components and compose them before writing custom interactive primitives.
- If a required component is missing, add the matching shadcn/Radix component and wrap it under `src/components/ui/` so styling, accessibility, focus management, and behavior stay consistent.
- Write a custom component only when shadcn components and their composition APIs cannot satisfy the requirement. Record the reason before doing so.
- Custom UI must still reuse the project's Rhea theme tokens, spacing, radii, states, and accessibility conventions. Do not substitute native interactive shortcuts such as `details/summary` when an appropriate shadcn component exists.

## Git Commit Language

- Use Conventional Commit type prefixes such as `feat:`, `fix:`, and `docs:`.
- Write the commit subject and body in Chinese by default. Use English only when the user explicitly requests it.
