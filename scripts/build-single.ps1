# build-single.ps1 —— 构建单文件 WorkBuddy Switch（内含 OpenAI 兼容网关）
#
# 产物：dist/wb-switch.exe （一个文件，无需额外的 gateway.exe）
#
# 流程：
#   1. 用 Go 构建网关，输出到 crates/wb-switch-core/embedded/gateway.exe
#   2. 构建前端（rust-embed 需要仓库根 dist/）
#   3. cargo build：build.rs 会把网关 gzip 后编进主程序
#
# 依赖：Go >= 1.22、Node >= 16、Rust（MinGW 亦可，无需 Visual Studio）
[CmdletBinding()]
param(
    [string]$GatewaySource = "",
    [string]$OutputDir = "dist-single",
    [switch]$SkipFrontend
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根
Push-Location $root
try {
    $embedded = Join-Path $root "crates/wb-switch-core/embedded"
    New-Item -ItemType Directory -Force -Path $embedded | Out-Null

    # ---- 1) 构建网关 ----
    Write-Host "==> [1/4] 构建网关 (Go)" -ForegroundColor Cyan
    if ($GatewaySource) {
        Copy-Item $GatewaySource (Join-Path $embedded "gateway.exe") -Force
        Write-Host "    直接使用: $GatewaySource"
    } elseif (Test-Path (Join-Path $embedded "gateway.exe")) {
        $age = (Get-Date) - (Get-Item (Join-Path $embedded "gateway.exe")).LastWriteTime
        Write-Host ("    已有 embedded/gateway.exe（{0:N0} 小时前构建），如需重建请加 -GatewaySource" -f $age.TotalHours)
    } else {
        $gwDir = Join-Path $root "go-gateway"
        if (-not (Test-Path $gwDir)) {
            throw "未找到网关源码目录 $gwDir；可用 -GatewaySource 指定已有的 gateway.exe"
        }
        $env:GOFLAGS = "-mod=mod"
        Push-Location $gwDir
        go build -trimpath -ldflags "-s -w" -o (Join-Path $embedded "gateway.exe") ./cmd/server
        $code = $LASTEXITCODE
        Pop-Location
        if ($code -ne 0) { throw "网关构建失败" }
        Write-Host "    已从源码构建网关"
    }

    # ---- 2) 前端 ----
    Write-Host "==> [2/4] 构建前端" -ForegroundColor Cyan
    if (-not $SkipFrontend) {
        Push-Location $root   # 前端源码与输出目录（dist/）都在仓库根
        if (-not (Test-Path node_modules)) { npm install --no-audit --no-fund }
        npm run build
        $code = $LASTEXITCODE
        Pop-Location
        if ($code -ne 0) { throw "前端构建失败" }
    } else {
        Write-Host "    跳过（沿用现有 dist/）"
    }

    # ---- 3) Rust 编译（内嵌网关 + 前端）----
    Write-Host "==> [3/4] 编译 Rust 主体" -ForegroundColor Cyan
    cargo build -p wb-switch-server --release
    if ($LASTEXITCODE -ne 0) { throw "Rust 构建失败" }

    # ---- 4) 输出 ----
    Write-Host "==> [4/4] 拷贝产物" -ForegroundColor Cyan
    # 输出目录必须与 dist/（前端资源，会被 rust-embed 内嵌）分开。
    # 若把 exe 放进 dist/，下一轮构建会把上一轮的 exe 也嵌进去，体积翻倍。
    $outPath = Join-Path $root $OutputDir
    if ($outPath -eq (Join-Path $root "dist")) {
        throw "输出目录不能是 dist/（该目录是前端资源，会被内嵌进二进制，导致体积翻倍）"
    }
    New-Item -ItemType Directory -Force -Path $outPath | Out-Null
    $exe = Join-Path $root "target/release/wb-switch.exe"
    Copy-Item $exe (Join-Path $root "$OutputDir/wb-switch.exe") -Force

    $size = (Get-Item (Join-Path $root "$OutputDir/wb-switch.exe")).Length / 1MB
    if ($size -gt 30) {
        Write-Warning ("产物 {0:N1} MB 偏大，可能把旧的 exe 也内嵌了；请检查 dist/ 下是否混入了 exe" -f $size)
    }
    Write-Host ""
    Write-Host "==> 完成：单文件分发（无需额外的 gateway.exe）" -ForegroundColor Green
    Write-Host ("    {0}  {1:N2} MB" -f "$OutputDir/wb-switch.exe", $size)
    Write-Host ""
    Write-Host "运行: $OutputDir/wb-switch.exe"
    Write-Host "  账号管理: http://127.0.0.1:57890/"
    Write-Host "  兼容网关: 在「兼容网关」页面启动"
}
finally {
    Pop-Location
}
