# 开发指南

## 环境要求

Node.js ≥ 20、Rust stable、Windows x64。

> 本项目**仅支持 Windows x64**。macOS / Linux 的构建、打包与 CI 矩阵均已移除。

## 开发命令

```powershell
npm install
npm run tauri dev        # 开发模式
npm run build            # 前端类型检查与构建
npm run tauri build      # 构建 Windows 安装包
```

## 发布新版本

签名密钥（自动更新用）通过 `TAURI_SIGNING_PRIVATE_KEY` 环境变量注入（CI 使用仓库 secret）。
发布新版本时：

1. `npm run tauri build` 生成 Windows 安装包及其签名（CI 以 `--bundles nsis` 构建，产出 `workbuddy-switch_<版本>_x64-setup.exe` + `.exe.sig`）。CI 会先清掉 `target/**/release/bundle`，避免 cargo cache 把旧安装包带进 Release。
2. `UPDATE_OS=windows UPDATE_ARCH=x86_64 sh scripts/gen-update-json.sh` 生成 `latest-windows-x86_64.json`（该脚本现在只支持 `UPDATE_OS=windows`）
3. `python3 scripts/merge-update-manifests.py <产物目录>` 把各 `latest-*.json` 合并为 `latest.json`
4. 将安装包、签名更新包、`latest*.json` 一并上传到 GitHub Release

> CI（`.github/workflows/build.yml`）的矩阵只构建 `win-x64`，产出 NSIS 安装程序
> `workbuddy-switch_<版本>_x64-setup.exe`。

### npm 版（webui）发布

1. CI（`.github/workflows/build.yml`）在 tag 发布时编译 server 二进制，并作为平台包 `workbuddy-switch-win32-x64` 发布到 npm registry
2. `cd npm && npm publish`（包名 `workbuddy-switch`，postinstall 从平台包复制二进制到 `bin/`，不依赖 GitHub）

## 目录结构

```
src-tauri/
  src/
    commands.rs      # Tauri command 薄包装（对应 Python 版 HTTP API）
    modules/         # 已抽离到 crates/wb-switch-core（三宿主复用）
crates/
  wb-switch-core/    # 核心逻辑：account/auth_file/oauth/process/switch/session/checkin/refresh/update/config
  wb-switch-server/  # HTTP server + CLI：axum API + rust-embed 前端
src/                 # 前端：components/pages/lib（api.ts 双通道：Tauri invoke / HTTP fetch）
npm/                 # npm 包：package.json + bin + scripts/install.js
```

## 隐私注意事项

- 仓库不提交本地数据（accounts.json、认证文件、密钥、token 由 `.gitignore` 排除）
- 发布前用 `git grep` 扫描 token 模式（`ghp_`/`npm_`/`gho_` 等）
