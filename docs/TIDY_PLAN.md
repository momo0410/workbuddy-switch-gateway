# 仓库整理方案

> 基于 2026-09-12 工作区实际状态勘察。目标：让「版本库内容」与「本地产物」分离，
> 把约 **26 GB** 的构建垃圾和一批该入库却没入库的源码理清，**不改变任何构建/发布行为**。

---

## 一、现状诊断

### 1. 体积：26 GB 本地产物 vs 0.1 GB 真实源码

| 目录 | 体积 | 是否入库 | 判定 |
|---|---:|---|---|
| `target/` | **22.6 GB** | 已忽略 | 唯一的 Rust 构建目录（MSVC 默认） |
| `target-check/` | **4.1 GB** | ❌ 未忽略 | `CARGO_TARGET_DIR` 绕占用的临时目录，属遗留 |
| `node_modules/` | 143 MB | 已忽略 | 正常 |
| `.git/` | 27 MB | — | 正常 |
| `_rel/` | 8.9 MB | 已忽略 | 0.4.0 发布产物 + `.sig` |
| `dist/` | 1.8 MB | 已忽略 | 前端构建产物 |

**`target-check/` 是最大问题**：它是 `target/debug/` 的一份完整副本，但 `.gitignore`
里没有对应规则（只写了 `target/`，不匹配 `target-check/`），所以 `git status` 把它
当成 **7400+ 个待添加文件**。这也是「很乱」的直观来源。

### 2. 版本库：94 处改动混在一起，其实分三类

```
git status --short  →  94 条
```

| 类别 | 数量 | 内容 | 该怎么办 |
|---|---:|---|---|
| **A. 真实功能改动** | ~30 | 国服专属锁定（`region_scope` 移除）、国际版支持、`wb-switch-gateway` crate、`go-gateway/` 入库 | 应当提交 |
| **B. 意外删除** | ~40 | `src-tauri/icons/` 的 android/ios 图标、`scripts/make-dmg.sh`、`fix-app.sh`、`dmgbuild-settings.py`、`README_UPSTREAM.md` | 需确认是「有意瘦身」还是「误删」 |
| **C. 未跟踪垃圾** | 1 个巨型 | `target-check/` | 加忽略规则 |

### 3. 真正「乱」的核心：`go-gateway/` 被静默忽略

这是本次勘察最重要的发现。`go-gateway/` 是 Go 版网关的**长期 vendor 依赖**
（`.trae/documents/workbuddy2api-merge-restructure.md` 第 9–11 行明确记录：Rust 版
`chat_completions` 仍是 503 占位，v0.4.0 的真实构建依赖就是它）。

但它**整个目录在根仓库里一个文件都没入库**：

```
git ls-files go-gateway  →  0 个文件
```

原因是一条**子目录相对规则**在作怪 —— `go-gateway/.gitignore` 的 `docs/` 与 `*.md`：

```gitignore
# go-gateway/.gitignore 第 27、33 行
docs/
*.md
!README.md
```

在 Git 中，**子目录 `.gitignore` 里的 `docs/` 会匹配自身所在的目录**
（`go-gateway/docs/`）。而 `go-gateway/docs/` 并不存在 —— 但 Git 仍把
`go-gateway/` 判定为「已排除目录」，于是 **29 个 Go 源文件、3 个测试文件、
`go.mod`、`Dockerfile` 全部被静默跳过，连 `git status` 都不提示**。

> 验证：`git check-ignore -v go-gateway/go.mod` 目前无输出（文件本可入库），
> 但 `git ls-files go-gateway` 为 0 —— 目录级排除已生效。

**后果**：当前仓库 clone 下来**无法构建内嵌网关**，而 README 却把
「构建网关（Go）」写成了构建流程第一步。仓库实际上是不自洽的。

### 4. 构建链路依赖被忽略的中间产物

`crates/wb-switch-core/build.rs` 的候选路径是：

1. `$WB_SWITCH_GATEWAY_BIN`
2. `crates/wb-switch-core/embedded/gateway.exe` ← `.gitignore` 已忽略
3. `dist/gateway.exe` ← `.gitignore` 已忽略

即：**忽略的两个目录正是构建链路的输入**。这不是错误（产物本就不该入库），
但意味着 README 必须显式写清「先跑哪条命令产出它」，否则新环境必然卡住。

### 5. 文档与配置的四套体系并行

| 位置 | 内容 | 状态 |
|---|---|---|
| `.trellis/` | spec / tasks / workspace | 被 `.gitignore` 忽略，**却又追踪着 12 个文件** —— 自相矛盾 |
| `.trae/documents/` | 合并重构方案 | 未忽略、未追踪 |
| `.workbuddy-ai/memory/` | AI 工作记忆 | 未忽略、未追踪 |
| `.vscode/` | 只保留 `extensions.json` | 已忽略其余，正常 |

`.trellis/` 的矛盾最需要注意：`.gitignore` 第 44 行写了 `.trellis/`，
但 `git ls-files` 显示 12 个文件仍被追踪 —— 追加的忽略规则对**已追踪文件无效**。
要么承认它入库（删掉忽略行），要么彻底退出（`git rm --cached`），目前两边都不是。

### 6. 其他次要问题

- `README.md` 480 行 / 35 KB，同时承担「用户说明书」与「开发者文档」两种角色，
  而 `docs/DEVELOPMENT.md` 只有 53 行 —— 职责划分与体量严重倒挂。
- `Cargo.lock` 在根 workspace 和 `src-tauri/` 各有一份（157 KB + 150 KB），
  workspace 下 `src-tauri/Cargo.lock` 是冗余的。
- `.github/workflows/` 被 `.gitignore` 忽略（第 56–58 行注明「需 workflow scope」），
  但目录里有 2 个 workflow 文件 —— 属于有意为之，保留即可。

---

## 二、整理方案

分四个阶段，**每阶段可独立停止**，风险从低到高。

### 阶段 1：止血 —— 消除 `git status` 噪声（零风险）

1. `.gitignore` 增加规则，覆盖所有 `CARGO_TARGET_DIR` 变体：

   ```gitignore
   # 任意 cargo target 目录（含 CARGO_TARGET_DIR 自定义的 target-*）
   target/
   target-*/
   src-tauri/target/
   ```

2. `.gitignore` 增加 `go-gateway` 的产物规则，与根规则保持一致：

   ```gitignore
   go-gateway/wb2api
   go-gateway/auths/
   go-gateway/data/
   go-gateway/config.json
   ```

3. **删除 `target-check/`**（4.1 GB）。它是 `CARGO_TARGET_DIR` 临时绕占用目录，
   属于一次性产物，可随时重建。

**预期结果**：`git status` 从 94 条降到 ~90 条，但**不再出现 7400 个文件**，
剩下的每一条都是需要人做判断的真实改动。

### 阶段 2：让 `go-gateway/` 真正入库（低风险，高价值）

修复 `go-gateway/.gitignore` 的目录级自排除：

```gitignore
# 改前（会排除 go-gateway/ 自身）
docs/
*.md
!README.md

# 改后（只在有 docs/ 时排除其内容；*.md 规则下移到子目录层）
*.md
!README.md
```

说明：`go-gateway/docs/` 当前并不存在，直接删掉 `docs/` 行即可；
若将来确有设计文档需要排除，改用 `docs/**` 或 `!/cmd/**/*.md` 这类不匹配自身的写法。

然后确认收录清单（预计 32 个文件）：

```
go-gateway/go.mod  go.sum
go-gateway/Dockerfile  docker-compose.yml  .dockerignore
go-gateway/README.md  LICENSE  config.example.json
go-gateway/credit.sh  login.sh  signin.sh
go-gateway/cmd/       (6 个 .go)
go-gateway/internal/  (23 个 .go，含 10 个 _test.go)
```

**同时需要做的取舍**：`go-gateway/` 是上游 `Sliverkiss/workbuddy2api` 的代码，
而 `docs/UPSTREAM_AUTHORIZATION_REQUEST.md` 记录该上游**没有 LICENSE**、
README 声明「再分发需向所有者确认授权」。当前仓库是 private 的，问题不大；
但**若计划公开，必须先解决这一条**。建议在本阶段把这个前置条件写进 README
的「上游来源与许可证」小节，避免公开时才发现。

**预期结果**：新 clone 的仓库具备完整的 Go 源码，README 的构建流程自洽。

### 阶段 3：收口版本库改动（需人工确认）

把 94 条改动**按主题拆成 5 个提交**，而不是一次性 `git add -A`：

| # | 提交 | 内容 |
|---|---|---|
| 1 | `chore: 忽略 target-* 与 go-gateway 产物，删除 target-check` | 阶段 1 的 `.gitignore` 与目录删除 |
| 2 | `fix: 修复 go-gateway 目录被自身 .gitignore 排除，Go 源码入库` | 阶段 2 |
| 3 | `feat: 自动签到/自动旅行锁定为仅国服，移除 region_scope` | `config.rs` / `checkin.rs` / `travel.rs` / `AccountsPage.tsx` / `SettingsPage.tsx` / `types.ts` / `api.ts` |
| 4 | `feat: 新增 wb-switch-gateway crate 与内嵌网关构建链` | `crates/wb-switch-gateway/` / `build.rs` / `gateway_embed.rs` / `wb-switch-core/Cargo.toml` |
| 5 | `chore: 清理跨平台构建脚本与图标资源` | `scripts/make-dmg.sh` 等删除 + `src-tauri/icons` android/ios 删除 |

**提交 5 需要先确认**：`scripts/make-dmg.sh`、`fix-app.sh`、`dmgbuild-settings.py`
和 apple/android 图标属于 macOS/Linux 支持，而 `docs/DEVELOPMENT.md` 明确写着
「本项目仅支持 Windows x64，macOS / Linux 的构建、打包与 CI 矩阵均已移除」——
**删除是与文档一致的**，但删掉后若将来要恢复会有成本。建议确认后再执行。

### 阶段 4：结构优化（可选，按需做）

1. **`README.md` 瘦身**：480 行拆成两份 —— README 保留
   「是什么 / 怎么装 / 怎么用」（面向用户，目标 ≤ 200 行），
   「架构设计 / 项目结构 / 测试 / 上游改动」移入 `docs/DEVELOPMENT.md`（面向开发者）。
   两个文件互相链接即可。

2. **`Cargo.lock` 去重**：删除 `src-tauri/Cargo.lock`，workspace 根的唯一一份是
   权威来源。执行前确认没有独立于 workspace 的构建方式依赖它。

3. **`.trellis/` 定位二选一**（当前自相矛盾）：
   - **方案 A（推荐）**：承认它入库 —— 删掉 `.gitignore` 第 44 行的 `.trellis/`，
     让 spec 与任务归档随仓库走。理由：`AGENTS.md` 把 `.trellis/spec/` 描述为
     「写代码前必读的分层规范」，这类约束应当与代码同版本。
   - **方案 B**：彻底退出 —— `git rm -r --cached .trellis/`，纯本地工作流。
     理由：`.trellis/workspace/` 是个人日志，入公共库噪音大。

4. **工具目录归置**：`.trae/documents/` 与 `.workbuddy-ai/memory/` 都是 AI 协作产物。
   建议统一忽略（加入 `.gitignore`），与已忽略的 `.codebuddy/`、`.cursor/`、`.agents/`、
   `.codex/`、`.grok/`、`/.workbuddy/` 保持一致 —— 目前这两条是唯一的例外。

---

## 三、执行顺序与验收

```
阶段 1  →  git status 不再有 7400 个 target-check 文件
阶段 2  →  git ls-files go-gateway | Measure-Object  →  32（不再是 0）
阶段 3  →  5 个主题提交，工作区干净
阶段 4  →  按需
```

每个阶段结束后的验证命令：

```powershell
# 阶段 1
git status --short | Measure-Object -Line      # 预期 ~90，无 target-check
# 阶段 2
git ls-files go-gateway | Measure-Object -Line # 预期 32
# 阶段 3
git status --short                             # 预期空
# 全量构建回归（沿用 .workbuddy-ai 中记录的环境）
$env:CARGO_TARGET_DIR="$PWD\target"
cargo check --workspace
cargo test -p wb-switch-gateway
```

---

## 四、不做的事

- **不**清理 `target/`（22.6 GB）。`:04` 起有 `wb-switch-rust` 进程（PID 36652）
  正在运行并持有 `target/debug/.cargo-lock`，删除会破坏运行中的应用。
  确实需要时：先退出应用，再 `cargo clean`。
- **不**改动 `_rel/` 与 `.workbuddy-ai/` 的内容。
- **不**动 `.github/workflows/`（有意忽略，见 `.gitignore` 第 56–58 行说明）。
- **不**在本轮做 Go→Rust 的网关迁移（`.trae` 文档已明确其为后续独立任务）。
