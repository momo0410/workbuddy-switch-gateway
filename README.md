<div align="center">

# WorkBuddy Switch Gateway

**账号管理 + OpenAI 兼容网关，一个桌面应用搞定**

把 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch) 的账号管理能力
与 [workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) 的网关后端整合进同一个 GUI 软件。

</div>

---

## 这是什么

本项目**基于两个开源项目整合改造**，不是从零开发：

| 来源 | 提供的部分 | 许可证 |
|---|---|---|
| [changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch) | 桌面 GUI 外壳、账号管理、签到、积分/Token 统计、托盘等全部界面与核心逻辑 | MIT |
| [Sliverkiss/workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) | OpenAI 兼容网关（账号池轮转、熔断冷却、会话粘性、SSE 规范化） | 见下方「授权说明」 |

整合部分（本仓库新增）：

- **兼容网关页面**：在 GUI 内选择端口、一键启停网关、查看账号池运行态
- **账号自动同步**：账号库变更自动推送到网关凭证目录，网关运行中自动重启加载
- **网关单文件内嵌**：网关二进制压缩后编进主程序，**只需分发一个 exe**
- **多端口检测**：试绑 + 主动连接双重判定，避免误判「端口空闲」导致启动假成功

> **郑重声明**：本项目的绝大部分代码来自上述两个上游项目，我只是做了整合与少量修补。
> 所有功劳归于原作者，请优先支持上游项目。

---

## ⚠️ 授权说明（请先阅读）

- **workbuddy-switch**（MIT）：允许使用、修改、再分发，本仓库已保留其 `LICENSE` 与版权声明。
- **workbuddy2api**：上游仓库**未包含 LICENSE 文件**，其 README 明确写道
  「如需使用或再分发，请向仓库所有者确认授权条款」。

因此本仓库**当前为私有状态**，仅作者自用，**尚未获得公开分发 workbuddy2api 代码的授权**。
在取得原作者书面许可前，请勿公开传播本仓库或其构建产物。

如果你是该上游项目的作者并希望调整署名或许可方式，欢迎提 Issue 联系。

---

## 功能

### 账号管理（来自 workbuddy-switch）
- OAuth 扫码添加账号、从本机导入、手动添加 token
- 一键切换 WorkBuddy / CodeBuddy CLI / CodeBuddy CN IDE 登录账号
- 自动签到、Token 保活、积分到期监控、Token 用量统计
- 会话复制、自动轮换

### 兼容网关（来自 workbuddy2api）
- OpenAI 兼容接口：`/v1/chat/completions`（流式 / 非流式）、`/v1/models`
- 多账号池：三因子加权随机选号、熔断与冷却、会话粘性
- 定时签到保活、指纹脱敏、状态持久化

### 整合新增
- **端口自由选择**：GUI 内直接改端口，自动检测占用并给出建议
- **一键启停**：选择端口 → 点启动，无需命令行
- **账号自动同步**：新增账号 30 秒内自动进入网关（网关运行时自动重启加载）
- **单文件分发**：`wb-switch.exe` 内已含网关，无需额外文件

---

## 安装与使用

### 方式一：安装包（推荐）

从 [Releases](https://github.com/momo0410/workbuddy-switch-gateway/releases/latest) 下载：

- Windows：`wb-switch-gateway_x.y.z_x64-setup.exe`
- 免安装：解压 `*.zip` 后直接运行 `wb-switch-gateway.exe`

### 方式二：从源码构建

需要：Go ≥ 1.22、Node.js ≥ 16、Rust（MSVC 或 MinGW 均可）

```powershell
# 1) 构建网关（Go），输出到 crates/wb-switch-core/embedded/gateway.exe
cd path\to\workbuddy2api
go build -trimpath -ldflags "-s -w" -o ..\workbuddy-switch-gateway\crates\wb-switch-core\embedded\gateway.exe .\cmd\server

# 2) 构建前端 + 打包 GUI
cd path\to\workbuddy-switch-gateway
.\scripts\build-single.ps1          # 产出 dist-single\wb-switch.exe（单文件）
npm run tauri build                 # 产出安装包
```

### 使用网关

1. 打开应用 → 左侧「兼容网关」
2. 填写服务端口（会自动检测是否可用，占用时可一键换端口）
3. 点「启动网关」
4. 客户端接入：

```bash
export OPENAI_BASE_URL=http://127.0.0.1:7863/v1
export OPENAI_API_KEY=<你在页面里设置的 api_key>
```

---

## 托盘行为

关闭主窗口**不会退出应用**，而是隐藏到系统托盘继续运行（网关和签到等后台任务不受影响）。
托盘右键菜单提供：

| 菜单项 | 说明 |
|---|---|
| 打开主界面 | 显示并聚焦主窗口 |
| 打开 GitHub | 打开本项目仓库 |
| 一键签到 | 立即对所有账号执行签到 |
| 轻量模式 | 降低界面开销（复选框） |
| 退出应用 | 真正退出（网关子进程随之结束） |

---

## 数据位置

| 内容 | 路径 |
|---|---|
| 账号库（真源） | `~/.wb-switch/accounts.json` |
| 网关凭证（副本，自动同步） | `~/.wb-switch/gateway/gateway_auths/` |
| 网关配置 | `~/.wb-switch/gateway/gateway_config.json` |
| 内嵌网关释放位置 | `~/.wb-switch/gateway/bin/` |

> 账号库与网关凭证是**两个不同格式**的存储（时间戳单位分别为毫秒/秒），
> 由应用的自动同步机制保持一致，无需手动维护。

---

## 已知限制

- **自动更新未启用**：更新签名密钥需由发布者自行生成，当前构建未附带有效签名。
- **仅 Windows 实测**：macOS / Linux 的构建脚本尚未验证。
- **未获上游授权**：见上方「授权说明」。

---

## 许可证

- 本整合部分：MIT
- `workbuddy-switch` 部分：MIT，版权归原作者 changexbc
- `workbuddy2api` 部分：**授权待确认**

原始 `LICENSE` 文件保留在本仓库中。
