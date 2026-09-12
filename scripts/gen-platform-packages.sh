#!/bin/bash
# 生成平台包 package.json（Windows-only：平台包发布在 npm，二进制由 npm registry 下载）
# 用法：sh scripts/gen-platform-packages.sh <版本号，如 0.1.6>
set -e
V=$1
[ -z "$V" ] && echo "用法: sh scripts/gen-platform-packages.sh <版本号>" && exit 1

cd "$(dirname "$0")/../npm/platform" || exit 1

gen() {
  local tag="$1" os="$2" cpu="$3" binfile="$4"
  local dir="workbuddy-switch-$tag"
  mkdir -p "$dir/bin"
  cat > "$dir/package.json" << JSON
{
  "name": "workbuddy-switch-$tag",
  "version": "$V",
  "description": "workbuddy-switch platform binary ($tag)",
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["bin"],
  "license": "MIT"
}
JSON
  echo "生成 $dir (bin=$binfile)"
}

gen win32-x64 win32 x64 wb-switch-win32-x64.exe

echo "平台包生成完成（版本 $V），把对应二进制复制到各包 bin/ 后 npm publish。"
