# 待同步源码清单

> 清点时间：2026-09-12。共 **94 条改动 + 63 个未跟踪文件**，全部可正常 `git add`。

## ⚠️ 先更正我之前的一个错误结论

我在前几轮说过「`go-gateway/` 被自身 `.gitignore` 静默排除、一个文件都入不了库」。
**这是错的**，实际验证结果：

```
git add --dry-run go-gateway   →  列出 49 个文件，exit=0   ✓ 完全可提交
git check-ignore go-gateway/go.mod  →  无输出（= 没有忽略规则）
```

我的推理错在：拿 `git ls-files go-gateway` 返回 0 当成了「被忽略」。但 `ls-files`
**只列已跟踪文件** —— 没 `add` 过自然是 0。而 `git status` 显示 `?? go-gateway/`
只是 git 对未跟踪**目录的折叠显示**（`untracked-files=normal` 的默认行为），
加 `--untracked-files=all` 就展开成 49 个文件。

**结论：不存在「目录级自排除」这个 bug，`go-gateway/` 只是还没提交。**
`go-gateway/.gitignore` 里的 `docs/` 与 `*.md` 规则确实存在，但 `go-gateway/docs/`
不存在，且实测不阻止任何文件入库（`README.md` 由 `!README.md` 显式放行）。

---

## 一、需要同步的内容（三类）

### A. 未跟踪的新文件 —— 63 个（必须 add）

| 路径 | 数量 | 说明 |
|---|---:|---|
| `go-gateway/**` | **49** | Go 版网关源码，v0.4.0+ 的真实构建依赖 |
| `crates/wb-switch-gateway/**` | **12** | Rust 版网关 crate（含 2 个 bin + 1 个测试） |
| `crates/wb-switch-core/examples/oauth_intl_probe.rs` | 1 | 国际版 OAuth 探测示例 |
| `scripts/asar-scan.ps1` | 1 | ASAR 扫描工具 |

> `docs/TIDY_PLAN.md`（我写的整理方案）你自行决定是否入库。
> `.trae/`、`.workbuddy-ai/` 已被 `.gitignore` 第 62 行排除，不会进库。

**`go-gateway/` 明细**：`go.mod`、`go.sum`、`Dockerfile`、`docker-compose.yml`、
`.dockerignore`、`LICENSE`、`README.md`、`config.example.json`、
`credit.sh`、`login.sh`、`signin.sh`、`cmd/`（6 个 .go）、`internal/`（23 个 .go，
含 10 个 `_test.go`）。

### B. 修改的文件 —— 45 个

| 分组 | 文件 | 关联改动 |
|---|---|---|
| 忽略规则 | `.gitignore` | 补 `target-*/`，覆盖 `CARGO_TARGET_DIR` 漏洞 |
| 核心功能 | `crates/wb-switch-core/src/modules/*.rs`（15 个） | 国服专属锁定、网关模块 |
| 核心配置 | `crates/wb-switch-core/Cargo.toml`、`build.rs` | 内嵌网关构建链 |
| server | `crates/wb-switch-server/{Cargo.toml, src/api.rs, src/main.rs}` | — |
| 桌面端 | `src-tauri/{Cargo.toml, tauri.conf.json, src/commands.rs, src/lib.rs, src/tray.rs}` | — |
| 前端 | `src/components/{oauth-login-dialog,update-install-dialog}.tsx`、`src/lib/{api,screenshot-demo,types}.ts`、`src/pages/AccountsPage.tsx` | 移除 `region_scope` |
| 发布脚本 | `scripts/{build-single.ps1,gen-platform-packages.sh,gen-update-json.sh,merge-update-manifests.py}` | **含本次修复的 OWNER/REPO 默认值** |
| npm | `npm/{README.md,package.json,bin/workbuddy-switch.js,scripts/install.js}` | — |
| 文档 | `README.md`、`docs/DEVELOPMENT.md`、`package.json`、`package-lock.json`、`Cargo.lock` | — |

### C. 删除的文件 —— 44 个（需 `git rm`）

全部与 `docs/DEVELOPMENT.md` 声明的「**仅支持 Windows x64**，macOS/Linux 已移除」一致：

| 类别 | 数量 | 文件 |
|---|---:|---|
| iOS 图标 | 18 | `src-tauri/icons/ios/AppIcon-*.png` |
| Android 图标 | 17 | `src-tauri/icons/android/**` |
| macOS 图标 | 1 | `src-tauri/icons/icon.icns` |
| macOS 脚本 | 3 | `scripts/{make-dmg.sh,fix-app.sh,dmgbuild-settings.py}` |
| 平台包 | 4 | `npm/platform/{darwin-arm64,darwin-x64,linux-arm64,linux-x64}/package.json` |
| 文档 | 1 | `README_UPSTREAM.md` |

> `README_UPSTREAM.md` 与 `docs/UPSTREAM_AUTHORIZATION_REQUEST.md` 内容重叠，
> 删除前建议确认后者已保留完整的上游说明。

---

## 二、提交前的安全检查（已逐项验证通过）

| 检查 | 结果 |
|---|---|
| 私钥/密钥文件（`*.key`/`*.pem`/`*.p12`）会否入库 | ✅ 无 |
| `go-gateway/config.json`（含 token） | ✅ 不存在，仅 `config.example.json` |
| `crates/wb-switch-core/embedded/`（内嵌 exe） | ✅ 已被 `.gitignore` L53 排除 |
| `_rel/`（发布产物 + 签名） | ✅ 已被 `.gitignore` L57 排除 |
| `target/`、`target-check/`、`node_modules/`、`dist/` | ✅ 已排除 |

---

## 三、Trellis 结构整理（已完成）

原状态自相矛盾且带一堆失效引用：

```
.gitignore 第 42 行:  .trellis/        ← 声明忽略
git ls-files .trellis:  12 个文件       ← 却已被跟踪（忽略规则对已跟踪文件无效）
```

深挖后发现，这是**一套更完整的 Trellis 结构被裁剪后的残留**：

| `AGENTS.md` / `.gitattributes` 引用的路径 | 实际 |
|---|---|
| `.trellis/workflow.md` | ✗ 不存在 |
| `.trellis/workspace/` | ✗ 不存在 |
| `.trellis/spec/cli/backend/directory-structure.md` | ✗ 不存在 |
| `.agents/skills/` | ✗ 不存在（且被 `.gitignore` 排除） |
| `.codex/agents/` | ✗ 不存在（且被 `.gitignore` 排除） |

`trellis` CLI 本机也不存在（全局 npm 包与 `node_modules` 均无），
所以「跑 `trellis update` 补齐」这条路径不成立。

### 已执行（方案 3：精简保留）

| 动作 | 结果 |
|---|---|
| 两个已完成任务迁出 | `.trellis/tasks/archive/**` → `docs/trellis-archive/`（10 个文件，内容不变） |
| 删除空的 `.trellis/tasks/` | 骨架目录清理 |
| `.gitattributes` | 移除永不匹配的 `merge=union` 规则，留注释说明原因 |
| `AGENTS.md` | 保留 `TRELLIS:START/END` 块（工具生成、会被 `trellis update` 覆写），只把失效路径引用改为指向真正有效的两份 spec |
| `.gitignore` | 删除 `.trellis/` 忽略行 —— 让两份 spec 名正言顺入库 |

### 保留下来的（内容经核对仍准确）

- `.trellis/spec/guides/ui-component-guidelines.md`（5 KB）— 前端 UI 规范
- `.trellis/spec/wb-switch-core/backend/token-statistics.md`（11 KB）— Token 统计跨层契约

核对结果：契约中描述的 `token_stats::get_statistics()`（`token_stats.rs:761`）、
`GET /api/token-stats`（`api.rs:79`）、Tauri `get_token_statistics`（`commands.rs:362`）
与代码**完全一致**，仍具约束力。


---

## 四、建议的提交顺序

```powershell
# 1) 忽略规则与产物清理
git add .gitignore
git commit -m "chore: 补 target-*/ 忽略规则，覆盖 CARGO_TARGET_DIR 自定义目录"

# 2) Go 网关源码入库（49 个新文件）
git add go-gateway
git commit -m "feat: 纳入 Go 版网关源码作为长期构建依赖"

# 3) Rust 网关 crate（12 个新文件）
git add crates/wb-switch-gateway crates/wb-switch-core/examples scripts/asar-scan.ps1
git commit -m "feat: 新增 wb-switch-gateway crate 与 OAuth 探测示例"

# 4) 国服专属锁定
git add crates/wb-switch-core/src/modules src/lib src/pages src/components
git commit -m "feat: 自动签到/自动旅行锁定为仅国服，移除 region_scope"

# 5) 发布脚本修复（含本次发现的 OWNER/REPO 默认值错误）
git add scripts Cargo.lock package.json package-lock.json README.md docs/DEVELOPMENT.md npm
git commit -m "fix: 修正 gen-update-json.sh 的仓库默认值，避免更新清单指向错误仓库"

# 6) 跨平台资源清理（44 个删除）
git add -A src-tauri/icons npm/platform scripts README_UPSTREAM.md
git commit -m "chore: 移除 macOS/iOS/Android 构建资源，对齐仅支持 Windows x64"

# 7) Trellis 骨架整理（详见第六节）
git add .trellis .gitattributes AGENTS.md docs/trellis-archive .gitignore
git commit -m "chore: 精简 Trellis 结构，归档历史任务，清理失效引用"
```

**验收**：

```powershell
git status --short              # 预期：空
git ls-files go-gateway | Measure-Object -Line   # 预期：49
```

---

## 五、当前不应入库的内容（确认已排除）

| 路径 | 原因 |
|---|---|
| `target/`、`target-check/` | 构建产物（已在上一轮清理） |
| `node_modules/`、`dist/` | 依赖与前端产物 |
| `_rel/` | 发布产物 + 签名，由 CI 重新生成 |
| `crates/wb-switch-core/embedded/` | 内嵌 exe，由 `scripts/build-single.ps1` 生成 |
| `.trae/`、`.workbuddy-ai/` | AI 工具本地配置（`.gitignore` L62） |
| `.github/workflows/` | 有意忽略（需 workflow scope，见 L59–61） |
| `*.key`、`*.key.pub` | 更新签名私钥 |
