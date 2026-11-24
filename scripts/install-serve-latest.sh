#!/bin/sh
# 把「启动即用最新源码」的入口包装安装到全局 workbuddy-switch 包。
#
# 幂等：可重复执行。全局包被 `npm i -g workbuddy-switch` 覆盖后，重跑一次即可恢复。
# 真实二进制保留为同目录下的 wb-switch-darwin-arm64.real，供构建失败时回退。
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
USER_HOME="${HOME:-/Users/xiaoshaowei}"
ENTRY="$ROOT/scripts/serve-latest.sh"

if [ ! -x "$ENTRY" ]; then
  echo "missing $ENTRY" >&2
  exit 1
fi

installed=0
for bin in "$USER_HOME"/.workbuddy/binaries/node/versions/*/lib/node_modules/workbuddy-switch/bin/wb-switch-darwin-arm64; do
  [ -e "$bin" ] || continue
  if [ ! -x "$bin.real" ]; then
    mv "$bin" "$bin.real"
    echo "saved real binary -> $bin.real"
  else
    rm -f "$bin"
  fi
  cat > "$bin" <<EOF
#!/bin/sh
# WorkBuddy Switch 启动入口包装（由 scripts/install-serve-latest.sh 生成）。
# LaunchAgent 与 WorkBuddySwitch.app 都通过这里启动，构建逻辑随项目源码维护。
SCRIPT="$ENTRY"
if [ -x "\$SCRIPT" ]; then
  exec "\$SCRIPT" "\$@"
fi
# 项目脚本缺失时回退到原始二进制，避免服务完全起不来。
exec "\$(dirname -- "\$0")/wb-switch-darwin-arm64.real" "\$@"
EOF
  chmod +x "$bin"
  echo "installed entrypoint -> $bin"
  installed=$((installed + 1))
done

if [ "$installed" -eq 0 ]; then
  echo "no global workbuddy-switch binary found" >&2
  exit 1
fi
