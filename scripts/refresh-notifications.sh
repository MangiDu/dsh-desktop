#!/usr/bin/env bash
# 替换 dsh desktop.app 之后，刷新 macOS 的通知身份记录（排查工具）。
#
# 为什么存在：macOS 把通知授权绑定在应用的代码签名身份上
# （Identifier + CDHash + 绑定的 Info.plist）。替换 bundle 后，
# usernoted（通知守护进程）和 LaunchServices 可能残留旧记录，导致
# requestAuthorization 以 "UNErrorDomain error 1（Notifications are not
# allowed for this application）" 直接拒绝，且永不弹出授权对话框。
#
# 注意：正常情况下替换后首次启动就会弹出授权对话框（新 CDHash →
# 系统无记录 → 主动询问），无需本脚本。仅当授权提示没有出现时运行
# 本脚本：重启 usernoted、清除隔离属性并向 LaunchServices 重新注册。
#
# 用法：
#   scripts/refresh-notifications.sh ["/path/to/dsh desktop.app"]
# 默认路径 /Applications/dsh desktop.app；若不存在则回退到刚构建的
# apps/desktop/src-tauri/target/release/bundle/macos/ 下的包。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILT="$ROOT/apps/desktop/src-tauri/target/release/bundle/macos/dsh desktop.app"

APP="${1:-/Applications/dsh desktop.app}"
if [[ ! -d "$APP" && -d "$BUILT" ]]; then
    echo "提示：'$APP' 不存在，改用刚构建的包：$BUILT"
    APP="$BUILT"
fi
if [[ ! -d "$APP" ]]; then
    echo "错误：找不到 dsh desktop.app。" >&2
    echo "用法：$0 [\"/path/to/dsh desktop.app\"]" >&2
    exit 1
fi

LSREGISTER="${LSREGISTER:-/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister}"

echo "== 1/3 刷新通知守护进程 usernoted =="
if pkill -x usernoted 2>/dev/null; then
    echo "已结束 usernoted（下次通知请求时自动重启）"
else
    echo "usernoted 未在运行（无需处理，按需自动启动）"
fi

echo "== 2/3 清除隔离属性 =="
if xattr -dr com.apple.quarantine "$APP" 2>/dev/null; then
    echo "已清除 com.apple.quarantine"
else
    echo "无隔离属性（本地构建，正常）"
fi

echo "== 3/3 向 LaunchServices 重新注册 =="
"$LSREGISTER" -f "$APP"
echo "已注册：$APP"

echo
echo "完成。请启动应用并验证："
echo "  open \"$APP\""
echo "启动时（或首次审批/提问事件）应弹出 macOS 通知授权对话框，请点「允许」。"
echo "注意：ad-hoc 签名的 CDHash 每次构建都会变，替换新构建后需重新授权一次；"
echo "永久方案是 Developer ID 签名（M5-3）。"
