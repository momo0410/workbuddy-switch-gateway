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

## 授权说明

- **workbuddy-switch**（[changexbc/workbuddy-switch](https://github.com/changexbc/workbuddy-switch)）：MIT 许可，
  允许使用、修改、再分发，本仓库已保留其 `LICENSE` 与版权声明。
- **workbuddy2api**（[Sliverkiss/workbuddy2api](https://github.com/Sliverkiss/workbuddy2api)）：
  MIT 许可（上游 2026-09-10 采用）。本项目**已取得原作者授权**后公开分发。

来源与授权细节见 [NOTICE](./NOTICE) 与上方的「这是什么」。

## 功能

### 账号管理（来自 workbuddy-switch）
- OAuth 扫码添加账号、从本机导入、手动添加 token
- 一键切换 WorkBuddy / CodeBuddy CLI / CodeBuddy CN IDE 登录账号
- 自动签到、Token 保活、积分到期监控、Token 用量统计
- 会话复制、自动轮换

### 兼容网关（来自 workbuddy2api）

OpenAI 兼容接口：`/v1/chat/completions`（流式 / 非流式）、`/v1/models`，
具备账号池轮转、熔断与冷却、会话粘性、定时签到保活、指纹脱敏、状态持久化。

**猫猫旅行**：随签到时点（09/21 点）自动巡检，对每个可用账号推进一趟 ——
无猫则同意协议并领养，有猫则按状态派出 / 领奖。账号间限速 800ms 避免风控。
禁用账号跳过；查询失败只跳过该账号本轮（不强刷 token，交给 22:00 保活）。

提供**两种工作模式**，可在「兼容网关」页面随时切换：

| 模式 | 行为 | 适用场景 |
|---|---|---|
| **负载均衡**（默认） | 账号池按三因子（积分比例 ×10 + 闲置补偿 + 成功率 ×3）加权随机选号，自动跳过冷却/熔断中的账号 | 多账号均衡使用，追求吞吐与高可用 |
| **指定账号** | 只使用你选定的那一个账号 | 固定身份、单独消耗某个账号的额度、排查单个账号问题 |

> 两种模式都保留熔断、冷却、会话粘性等全部能力 —— 指定账号模式下若该账号
> 触发限流，会正常进入冷却而不是无声失败。

### 整合新增
- **端口自由选择**：GUI 内直接改端口，自动检测占用并给出建议
- **一键启停**：选择端口 → 点启动，无需命令行
- **账号自动同步**：新增账号 30 秒内自动进入网关（网关运行时自动重启加载）
- **单文件分发**：网关二进制压缩后编进主程序，无需额外文件
- **单实例保护**：重复启动不会开出第二个窗口，而是聚焦（必要时从托盘唤回）已有实例。
  这同时也避免多个实例并发写账号库、重复启停网关子进程

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

- **仅 Windows 实测**：macOS / Linux 的构建脚本尚未验证。
- **猫猫旅行有两处入口**：网关侧随签到自动执行（09/21 点）；App 侧也有手动/自动
  旅行。二者调用同一上游接口且按自然日幂等（每日上限 1 次/天），同时开启不会重复派猫。
- **Windows 需 WebView2 Runtime**：Win10/11 一般已内置。若缺失，请安装
  [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。
- **GNU(MinGW) 构建的额外要求**：用 MinGW 而非 MSVC 编译时，需把 `WebView2Loader.dll`
  放在 exe 同目录。本仓库已把它声明为打包资源（`bundle.resources`），
  安装包会自动附带；自行编译请确保该文件存在于 `src-tauri/` 下。

## 许可证

- 本整合部分：MIT
- `workbuddy-switch` 部分：MIT，版权归原作者 changexbc
- `workbuddy2api` 部分：**授权待确认**

原始 `LICENSE` 文件保留在本仓库中。
