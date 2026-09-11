<div align="center">

# WorkBuddy Switch Gateway

**账号管理 + OpenAI 兼容网关，一个桌面应用搞定**

[![License](https://img.shields.io/badge/License-MIT-green.svg)](./LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows-0078D4.svg)](#系统要求)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB.svg)](https://tauri.app)
[![Gateway](https://img.shields.io/badge/API-OpenAI%20Compatible-412991.svg)](#兼容网关)

把 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch) 的账号管理能力
与 [workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) 的 OpenAI 兼容网关，
整合进同一个桌面应用：**一个安装包、一个界面、一个进程树**。

[功能特性](#功能特性) · [架构](#架构设计) · [快速开始](#快速开始) · [使用指南](#使用指南) · [常见问题](#常见问题) · [上游与许可](#上游来源与许可证)

</div>

---

## 项目简介

腾讯 CodeBuddy / WorkBuddy 客户端本身不提供 OpenAI 形态的开放接口。若想在
第三方 SDK、IDE 插件或自建服务里使用自己的账号额度，通常需要一段「把客户端凭证
变成标准 API」的胶水层，并同时管理多个账号的登录、保活与额度。

本项目把两件原本分离的事合并到一处：

| 能力 | 说明 |
|---|---|
| **账号生命周期** | 扫码登录、导入、切换、签到、Token 保活、积分到期监控 |
| **API 网关** | 把账号池暴露为 OpenAI 兼容接口，供任意客户端零改造接入 |

两者共享同一份账号库：在图形界面里新增的账号，会自动进入网关的账号池，无需手工
同步或重启容器。

> **来源声明**：本项目是**整合改造**而非从零开发。绝大多数代码来自上述两个上游项目，
> 整合部分（网关页面、账号同步、单文件内嵌、单实例保护及若干缺陷修复）由
> [momo0410](https://github.com/momo0410) 完成。详见 [上游来源与许可证](#上游来源与许可证)。

---

## 功能特性

### 账号管理

源自 [workbuddy-switch](https://github.com/changexbc/workbuddy-switch)。

| 模块 | 能力 |
|---|---|
| 账号入库 | OAuth 扫码登录、从本机客户端导入、手动添加 Token |
| 账号切换 | WorkBuddy 客户端、CodeBuddy CLI、CodeBuddy CN IDE 三套登录态互相独立切换 |
| 自动签到 | 启动即核验，运行期周期性补签，保留 30 天签到日志 |
| Token 保活 | 惰性刷新（操作前低于阈值即刷新）+ 每日保活，避免 refresh token 过期失效 |
| 积分监控 | 查询各账号积分资源、剩余量与到期时间，7 天内到期高亮并优先排序 |
| 积分统计 | 汇总官方请求用量：每日趋势、模型分布、账号消耗、请求明细 |
| Token 统计 | WorkBuddy / CodeBuddy CLI / CodeBuddy CN IDE 分别统计输入、输出、缓存读写、调用次数 |
| 会话复制 | 将当前账号的会话以新 ID 复制给目标账号（含 jsonl 正文、数据库索引、edge-sync 注册） |
| 自动轮换 | 定时把「积分最紧迫」的账号设为后续会话默认账号，避免额度过期浪费 |

### 兼容网关

源自 [workbuddy2api](https://github.com/Sliverkiss/workbuddy2api)。

- **OpenAI 兼容接口**：`POST /v1/chat/completions`（流式 / 非流式）、`GET /v1/models`
- **账号池调度**：三因子加权随机选号 —— 积分比例 ×10 + 闲置补偿 + 成功率 ×3
- **熔断与冷却**：429/404 软冷却、余额不足硬冷却至次日 04:00、连续失败指数退避熔断、在途租约限流
- **会话粘性**：同一会话尽量绑定同一账号，TTL 滚动续期，失败自动解绑
- **定时任务**：每日 09:00 / 21:00 签到 + 余额查询解冻；22:00 全账号 Token 刷新保活
- **猫猫旅行**：随签到时点自动巡检（详见下节）
- **出站脱敏**：请求体黑名单指纹字段清洗（可关闭）
- **状态持久化**：池状态本地原子落盘，可选 Upstash Redis 镜像

#### 网关工作模式

可在「兼容网关」页面随时切换，两种模式都完整保留熔断、冷却、会话粘性：

| 模式 | 行为 | 适用场景 |
|---|---|---|
| **负载均衡**（默认） | 账号池加权随机选号，自动跳过冷却 / 熔断中的账号 | 多账号均衡使用，追求吞吐与高可用 |
| **指定账号** | 只使用你选定的那一个账号 | 固定身份、单独消耗某账号额度、排查单个账号问题 |

> 实现方式：网关依据凭证目录建立账号池，指定账号模式只需**只导出该账号的凭证**。
> 因此无需改动网关注册逻辑，也不会损失其任何治理能力。切换时旧凭证会被自动清理。

#### 猫猫旅行

随签到时点（09:00 / 21:00）对每个可用账号推进一趟状态机：

| 账号状态 | 动作 |
|---|---|
| 尚未领养 | 同意协议 + 领养 |
| 空闲（idle） | 派出 |
| 在途（traveling） | 跳过 |
| 到站（arrived） | 领取奖励 |

- 账号间限速 800ms，避免触发风控
- 禁用账号跳过；查询失败仅跳过该账号本轮（不强刷 Token，交给 22:00 保活）
- 派出地点固定为「古镇客栈」（4 个地点收益/时长区间完全相同，无最优解）

> 网关侧与 App 侧都有旅行入口，二者调用同一上游接口且按自然日幂等
>（每日上限 1 次/天），同时开启不会重复派猫。

### 整合增强

本项目在整合过程中新增或修复的部分：

| 能力 | 说明 |
|---|---|
| **账号自动同步** | 账号库变更后自动推送到网关凭证目录；网关运行中则自动重启加载。约 30 秒内生效 |
| **端口自由选择** | 界面内直接改端口，实时检测占用并给出建议，可一键切换空闲端口 |
| **单文件分发** | 网关二进制 gzip 压缩后编进主程序，运行时按内容指纹释放到缓存，**只需分发一个 exe** |
| **单实例保护** | 重复启动不会开出第二个窗口，而是聚焦（必要时从托盘唤回）已有实例 |
| **官方身份校验** | 用网关 `/healthz` 的 `service` 标识确认应答者身份，避免「假启动成功」 |
| **双区域支持** | 国服（codebuddy.cn）与国际版（workbuddy.ai）账号可共存于同一账号库，按账号 `domain` 自动路由 |

关于**单实例保护**的必要性：应用启动后会运行 8 个后台任务（签到、保活、自动轮换、
旅行派发/领取、网关同步等），它们都会写同一份账号库。若允许多开，多个实例会并发
写入导致 Token 被旧值覆盖；同时网关子进程的托管状态是进程内变量，实例之间互不可见，
会重复启停造成端口冲突与孤儿进程。

### 桌面集成

- **系统托盘**：关闭主窗口不退出，隐藏到托盘继续运行（后台任务与网关不受影响）
- **托盘菜单**：打开主界面 / 打开 GitHub / 一键签到 / 轻量模式 / 退出应用
- **开机自启**：可选，自启时静默进入托盘，不弹窗打扰
- **自动更新**：基于 Tauri updater，更新包经签名校验

---

## 服务区域

支持**国服**与**国际版**两个区域，两边的账号可同时存在于同一账号库。

| | 国服 | 国际版 |
|---|---|---|
| 网页 / API | `www.codebuddy.cn` | `www.workbuddy.ai` |
| 聊天端点 | `copilot.tencent.com`（与 API **分域**） | `www.workbuddy.ai`（**同域**） |
| 凭证 `domain` | `www.workbuddy.cn` | `www.workbuddy.ai` |
| 本机认证文件 | `workbuddy-desktop.info` | `workbuddy-desktop-ai.info` |
| 代表模型 | `deepseek-v4-flash`、`glm-5.2`、`kimi-k2.7` | `gpt-5.6-*`、`gemini-3.5-flash`、`deepseek-v4.1-flash` |

**区域判定**：按账号库 `domain` 字段后缀（`.cn` → 国服，`.ai` → 国际版）。
网关与客户端的所有请求都据此选择域名，无需手工切换配置。

**一键导入**：「账号管理」页的「从本机导入」会**同时探测两个区域的认证文件**，
把本机已登录的账号全部并入账号库，提示中会标明各自区域。

> **模型名不通用**：两区域模型名不同（国服 `deepseek-v4-flash` / 国际版 `deepseek-v4.1-flash`）。
> `/v1/models` 返回两区域模型的并集，但**某个名称能否用取决于实际选中的账号属于哪个区域**；
> 不匹配时上游返回 `11102 model service info not found`，网关会自动换号重试。
> 需要精确控制时，请在网关页选择「**指定账号**」模式并锁定对应区域的账号。

## 架构设计

```
┌──────────────────── wb-switch.exe（单一可执行文件）────────────────────┐
│                                                                        │
│  桌面壳（Tauri 2 / Rust）                                               │
│  ├─ 主窗口：内嵌 React 前端（账号管理 · Token 统计 · 积分统计 · 兼容网关）│
│  ├─ 系统托盘与单实例保护                                                │
│  └─ 后台任务：签到 · 保活 · 自动轮换 · 旅行 · 账号同步                   │
│                                                                        │
│  wb-switch-core（Rust 库）                                              │
│  ├─ account / auth_file / switch / session …   账号与登录态             │
│  ├─ checkin / refresh / rotate / travel …      定时任务                 │
│  ├─ gateway.rs                                 网关托管与账号桥接        │
│  └─ gateway_embed.rs                           内嵌网关的释放与缓存      │
│                                                                        │
│  内嵌网关二进制（Go，gzip 压缩，构建期写入）                              │
│  └─ 运行时释放为 ~/.wb-switch/gateway/bin/gateway-<指纹>.exe            │
└────────────────────────────────────────────────────────────────────────┘
            │                                        │
            ▼                                        ▼
  ~/.wb-switch/accounts.json              ~/.wb-switch/gateway/
     （账号库 · 唯一真源）                    └─ gateway_auths/
                                                 （网关凭证 · 派生素材）
```

**两个存储为什么分开**：账号库与网关凭证的格式不同 —— 时间戳单位分别为毫秒与秒，
字段集也不同（账号库含 `profile_raw`、`auth_raw` 等客户端字段）。网关是独立进程，
需要自己能读懂的凭证目录。因此二者由自动同步机制保持一致：账号库是唯一真源，
网关凭证是派生素材。

**同步规则**：
- 账号库 → 网关：按 uid 生成嵌套形凭证；账号删除或模式切换后自动清理残留
- 网关 → 账号库：网关自身刷新 Token 后，按过期时间较新者回写（空 refresh token 不覆盖已有值）
- 内容无变化时不写盘，避免无意义的文件时间戳变动与网关重启

---

## 快速开始

### 系统要求

| 项目 | 要求 |
|---|---|
| 操作系统 | Windows 10 / 11（x64） |
| 运行时 | [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)（Win10/11 一般已内置） |
| 磁盘 | 约 50 MB |
| 其他 | 无需安装 Docker、Node.js 或 Go |

> macOS / Linux 的构建脚本已在仓库中，但尚未实际验证运行效果。

### 安装方式一：安装包（推荐）

从 [Releases](https://github.com/momo0410/workbuddy-switch-gateway/releases/latest) 下载：

| 文件 | 说明 |
|---|---|
| `WorkBuddy_Switch_Gateway_<版本>_x64-setup.exe` | 安装向导，自动创建开始菜单与卸载项 |
| `WorkBuddy_Switch_Gateway_<版本>_x64_en-US.msi` | MSI 包，适合批量部署 |
| `WorkBuddy_Switch_Gateway_<版本>_免安装版.zip` | 解压即用，不写入注册表 |

### 安装方式二：免安装

解压 zip 后直接双击 `WorkBuddy-Switch-Gateway.exe`。

> `WebView2Loader.dll` 必须与 exe 位于同一目录，请勿删除。

### 安装方式三：从源码构建

需要 Go ≥ 1.22、Node.js ≥ 16、Rust 工具链（MSVC 或 MinGW 均可）。

```powershell
# 1) 构建网关（Go），产物直接作为内嵌资源
git clone --depth 1 https://github.com/Sliverkiss/workbuddy2api.git
cd workbuddy2api

#    应用「国际版支持」补丁（需要国际版时才要；只做国服可跳过）
git apply ..\workbuddy-switch-gateway\patches\intl-support.patch

go build -trimpath -ldflags "-s -w" `
  -o ..\workbuddy-switch-gateway\crates\wb-switch-core\embedded\gateway.exe `
  .\cmd\server

# 2) 构建前端并打包为单一可执行文件
cd ..\workbuddy-switch-gateway
.\scripts\build-single.ps1        # 产出 dist-single\wb-switch.exe

# 3) 生成安装包（可选）
npm install
npm run tauri build
```

---

## 使用指南

### 账号管理

1. 打开应用，进入「账号管理」页面
2. 点击「扫码登录」，用微信 / 企业微信完成授权；也可「从本机导入」已登录的账号
3. 账号卡片显示登录状态、签到状态、积分余额与到期时间
4. 「切换」按钮可将该账号写入 WorkBuddy 客户端 / CodeBuddy CLI / CodeBuddy CN IDE

### 启用网关

1. 进入「兼容网关」页面
2. 设置**服务端口**（默认 `7863`）— 输入框右侧会实时显示端口是否可用
   - 若显示「已被占用」，点击「自动」自动挑选空闲端口，或点建议端口一键切换
3. 设置 **API Key**（留空表示不鉴权；公网部署务必设置）
4. 选择**工作模式**：负载均衡 / 指定账号（后者需选择具体账号）
5. 点击「启动网关」

### 客户端接入

启动成功后，页面会显示完整的接入地址，可直接复制。以环境变量方式接入：

```bash
export OPENAI_BASE_URL=http://127.0.0.1:7863/v1
export OPENAI_API_KEY=<你设置的 api_key>
```

验证连通性：

```bash
curl $OPENAI_BASE_URL/models -H "Authorization: Bearer $OPENAI_API_KEY"

curl $OPENAI_BASE_URL/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4-flash","messages":[{"role":"user","content":"hi"}]}'
```

现有 OpenAI SDK / 客户端通常只需替换 `base_url` 与 `api_key` 即可使用。

### 网关配置项

配置文件位于 `~/.wb-switch/gateway/gateway_config.json`，也可在界面中修改：

| 字段 | 默认 | 说明 |
|---|---|---|
| `port` | `7863` | 服务端口（权威字段，`listen` 由它派生） |
| `api_key` | 空 | 接口鉴权密钥；空 = 不鉴权 |
| `mode` | `balance` | 工作模式：`balance` 负载均衡 / `pinned` 指定账号 |
| `pinned_uid` | `null` | 指定账号模式锁定的账号 uid |
| `auto_start` | `false` | 随应用启动自动拉起网关 |
| `checkin_enabled` | `true` | 是否启用签到排程（猫猫旅行随之启停） |
| `keepalive_enabled` | `true` | 是否启用 Token 保活排程 |

### 托盘与单实例

- **关闭窗口**：隐藏到托盘而非退出，后台任务与网关继续运行
- **再次启动**：聚焦已有窗口；若窗口已隐藏到托盘则自动唤回，不会开启第二个实例
- **彻底退出**：右键托盘图标 → 「退出应用」（网关子进程会一并结束）

---

## 数据与备份

| 内容 | 路径 | 说明 |
|---|---|---|
| 账号库 | `~/.wb-switch/accounts.json` | **唯一真源**，含所有账号凭证，建议单独备份 |
| 网关凭证 | `~/.wb-switch/gateway/gateway_auths/` | 由账号库派生，删除后可自动重建 |
| 网关配置 | `~/.wb-switch/gateway/gateway_config.json` | 端口、API Key、模式等 |
| 网关原生配置 | `~/.wb-switch/gateway/gateway_native_config.json` | 转换后交给网关进程的配置 |
| 内嵌网关副本 | `~/.wb-switch/gateway/bin/` | 按内容指纹命名，版本升级后自动更新 |
| 签到 / 轮换日志 | `~/.wb-switch/*_logs.json` | 最多保留 30 天 |

> `accounts.json` 包含可直接登录的凭证，请勿分享或提交到版本库。

---

## 常见问题

**Q：网关启动失败，提示端口被占用？**
> 常见于本机已有其他服务占用同一端口（例如先前用 Docker 部署过网关）。
> 在「兼容网关」页面点「自动」换一个空闲端口即可。注意 Docker 映射的端口在
> 部分 Windows 配置下无法通过常规方式探测，本应用已采用「试绑 + 主动连接」双重
> 判定来提高识别率，但若仍提示启动失败，请手动指定其他端口。

**Q：提示「找不到 WebView2Loader.dll」？**
> 系统缺少 WebView2 相关组件。本仓库的安装包已内置该加载器；若使用免安装版，
> 请确认 `WebView2Loader.dll` 与主程序位于同一目录。仍报错则需安装
> [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。

**Q：添加了新账号，但网关里看不到？**
> 正常情况下约 30 秒内自动同步。若网关正在运行，同步后会短暂重启以加载新账号
> （这是网关只在启动时扫描凭证目录的设计所限）。也可在页面点「立即同步」手动触发。

**Q：窗口关闭后应用消失了吗？**
> 没有。窗口被隐藏到系统托盘，后台任务与网关仍在运行。右键托盘图标可重新打开
> 主界面或彻底退出。

**Q：国服与国际版账号能混用吗？**
> 可以放在同一账号库，按 `domain` 自动路由。但**模型名不通用**：混合账号池下
> 用某个区域的模型名请求，可能被路由到另一区域账号而返回 `11102`，网关会自动
> 换号重试（表现为偶发变慢）。需要稳定时，用「指定账号」模式锁定对应区域的账号。

**Q：国际版的模型列表为什么是固定的？**
> 上游 `/console/enterprises/personal/models` 在国际版返回 500，无法动态拉取，
> 因此国际版模型来自内置静态表（取自客户端本地配置 `acc-product-config-v3.json`）。
> 上游新增模型时需要同步更新该表。

**Q：能同时运行上游的 workbuddy-switch 吗？**
> 可以。两者的应用标识与安装目录不同，互不冲突。

---

## 开发

### 项目结构

```
crates/wb-switch-core/        核心逻辑（不依赖 Tauri，可被桌面端与 HTTP 服务复用）
  src/modules/account.rs        账号存储
  src/modules/gateway.rs        网关托管与账号桥接（本项目新增）
  src/modules/gateway_embed.rs  内嵌网关的释放与缓存（本项目新增）
  src/modules/travel.rs         猫猫旅行（App 侧）
  build.rs                      构建期压缩内嵌网关（本项目新增）
crates/wb-switch-server/      HTTP 服务形态（npm / webui）
src/                          React 前端
  src/pages/GatewayPage.tsx     兼容网关页面（本项目新增）
src-tauri/                    桌面壳（Tauri 2）
  src/tray.rs                   托盘与单实例行为
  src/commands.rs               前端可调用的命令
scripts/build-single.ps1      构建单一可执行文件（本项目新增）
```

### 测试

```bash
cargo test -p wb-switch-core    # 核心逻辑单元测试
npm run build                   # 前端类型检查与构建
```

网关侧（Go）自带完整测试套件：

```bash
cd path/to/workbuddy2api && go test ./...
```

---

## 对上游的改动

本项目对 `workbuddy2api`（Go 网关）做了少量改动以支持国际版，以补丁形式维护：

```
patches/intl-support.patch
```

改动内容：

- `internal/upstream/client.go`：新增 `isIntl()` 区域判定（按 `auth.Domain` 后缀）
  与 `BaseIntl` 字段；`chatBase()` / `billingBase()` 改为**按账号区域返回域名**
- `internal/upstream/headers.go`：`Origin` / `Referer` 跟随账号区域
  （国服 `codebuddy.cn`、国际版 `workbuddy.ai`）
- `internal/server/handler.go`：新增国际版静态模型表，`/v1/models` 返回两区域并集
  （国际版的模型列表接口返回 500，无法动态拉取）

补丁基于上游 `cfb1713` 生成，已验证可在更新的上游提交上干净应用并编译通过。

> 若你只使用国服，可跳过该补丁，功能与上游一致。

## 上游来源与许可证

本项目基于以下两个开源项目整合改造，**绝大部分代码来自上游**：

| 项目 | 作者 | 提供的部分 | 许可证 |
|---|---|---|---|
| [workbuddy-switch](https://github.com/changexbc/workbuddy-switch) | [changexbc](https://github.com/changexbc) | 桌面 GUI 外壳、账号管理、签到、积分与 Token 统计、托盘、CLI 切换等全部界面与核心逻辑 | **MIT** |
| [workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) | [Sliverkiss](https://github.com/Sliverkiss) | OpenAI 兼容网关（账号池轮转、熔断冷却、会话粘性、SSE 规范化、猫猫旅行等） | **MIT** |

两个上游项目均采用 **MIT 许可证**，允许使用、修改与再分发。本仓库已保留其原始
版权声明（见 [`LICENSE`](./LICENSE)），并在此基础上补充整合部分的版权声明。

整合部分（本项目新增）同样以 MIT 许可证发布。逐项来源说明与改动清单见
[`NOTICE`](./NOTICE)。

### 许可证

```
MIT License

Copyright (c) 2026 wb-switch        （workbuddy-switch 原作者）
Copyright (c) 2026 Sliverkiss        （workbuddy2api 原作者）
Copyright (c) 2026 momo0410          （本项目整合部分）
```

完整条款见 [`LICENSE`](./LICENSE)。

---

## 免责声明

- 本项目为**非官方**工具，与腾讯公司及 CodeBuddy / WorkBuddy 官方无任何关联。
- 项目通过 OAuth 设备授权使用**用户本人**的账号凭证，不提供、不托管任何账号。
- 使用本项目需遵守 CodeBuddy / WorkBuddy 的服务条款。因使用本项目产生的
  账号封禁、条款违约等风险由使用者自行承担。
- 本项目仅供学习与研究使用，请勿用于商业用途或大规模分发。
- 作者不对因使用本项目造成的任何直接或间接损失负责。

---

<div align="center">

如果这个项目对你有帮助，欢迎给上游项目点个 Star ⭐

[workbuddy-switch](https://github.com/changexbc/workbuddy-switch) ·
[workbuddy2api](https://github.com/Sliverkiss/workbuddy2api)

</div>
