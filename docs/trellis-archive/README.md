# 已归档任务（原 .trellis/tasks/archive）

2026-09-12 整理时从 `.trellis/tasks/archive/` 迁出。原因：`.trellis/` 里的
Trellis 工作流骨架（`workflow.md`、`workspace/`、`spec/cli/`）已被裁掉，
这些归档失去了所属的工具链，但内容仍是有效的开发记录，故保留于此。

`.trellis/spec/` 中仍有两份**现行有效**的编码契约，写代码前应阅读：

- `.trellis/spec/guides/ui-component-guidelines.md` — 前端 UI 规范
- `.trellis/spec/wb-switch-core/backend/token-statistics.md` — Token 统计跨层接口契约

## 归档内容

| 任务 | 完成日期 | 内容 |
|---|---|---|
| `08-30-fix-oauth-browser-opener` | 2026-08-30 | 修复 OAuth 授权链接在 WebUI 与 Tauri 下的打开方式 |
| `08-30-token-stats-layout` | 2026-08-30 | Token 统计页统一为积分统计页的布局层级 |

每个任务目录含：`prd.md`（需求）、`design.md`（设计，如有）、
`implement.md` / `implement.jsonl`（实现记录）、`check.jsonl`（校验）、
`task.json`（元数据）。

均为 `status: completed`，其代码产物仍在仓库中生效。
