#!/bin/sh
# WorkBuddy Switch：每次启动服务都用当前最新源码构建出的产物。
#
# 背景：wb-switch-server 用 rust_embed 在「编译时」把 dist/ 内嵌进二进制，
# 所以只改前端源码、甚至只重建 dist/ 都不会让 57890 的页面变新。
# 本脚本在每次服务启动时比较源码时间戳：
#   - 有变化：npm run build → cargo build --release -p wb-switch-server → 用新产物启动
#   - 无变化：直接用已有产物启动（秒起，不重复编译）
#   - 构建失败：回退到全局包里的原始二进制，保证服务仍然能起来
#
# 用法：
#   scripts/serve-latest.sh serve --no-open   # 正常启动（LaunchAgent / .app 调用）
#   scripts/serve-latest.sh --needs-rebuild   # 只检查：退出码 0=需要重建，1=无需重建
#   scripts/serve-latest.sh --build-only      # 只重建产物，不启动服务（手动刷新）
# 状态与日志：~/.wb-switch/serve-latest.stamp、~/.wb-switch/serve-latest.log
set -u

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
USER_HOME="${HOME:-/Users/xiaoshaowei}"
STATE_DIR="$USER_HOME/.wb-switch"
STAMP="$STATE_DIR/serve-latest.stamp"
LOG="$STATE_DIR/serve-latest.log"

# LaunchAgent 的 PATH 只有系统目录，构建工具要显式补上；node 版本目录用 glob 兼容升级。
PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$USER_HOME/.cargo/bin"
for node_bin in "$USER_HOME"/.workbuddy/binaries/node/versions/*/bin; do
  [ -d "$node_bin" ] && PATH="$node_bin:$PATH"
done
export PATH

log() {
  mkdir -p "$STATE_DIR" 2>/dev/null || true
  printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >> "$LOG" 2>/dev/null || true
}

# 监控源码与配置（刻意排除 dist/ target/ node_modules/ 等产物目录）
SOURCE_PATHS="src public crates src-tauri vendor-patched index.html vite.config.ts vitest.config.ts tsconfig.json tsconfig.node.json package.json package-lock.json components.json Cargo.toml Cargo.lock"

needs_rebuild() {
  [ -f "$STAMP" ] || return 0
  [ -f "$ROOT/dist/index.html" ] || return 0
  [ -x "$ROOT/target/release/wb-switch" ] || return 0
  for entry in $SOURCE_PATHS; do
    [ -e "$ROOT/$entry" ] || continue
    if [ -n "$(find "$ROOT/$entry" -newer "$STAMP" 2>/dev/null | head -n 1)" ]; then
      return 0
    fi
  done
  return 1
}

case "${1:-}" in
  --needs-rebuild)
    if needs_rebuild; then exit 0; else exit 1; fi
    ;;
  --build-only)
    log "manual rebuild"
    if (
      cd "$ROOT" &&
      npm run build >> "$LOG" 2>&1 &&
      cargo build --release -p wb-switch-server >> "$LOG" 2>&1
    ); then
      touch "$STAMP"
      log "rebuild ok"
      exit 0
    fi
    log "rebuild FAILED"
    exit 1
    ;;
esac

log "start: $*"

if needs_rebuild; then
  log "source changed -> rebuilding"
  if (
    cd "$ROOT" &&
    npm run build >> "$LOG" 2>&1 &&
    cargo build --release -p wb-switch-server >> "$LOG" 2>&1
  ); then
    touch "$STAMP"
    log "rebuild ok -> using target/release/wb-switch"
  else
    log "rebuild FAILED -> falling back to installed binary"
    for fallback in "$USER_HOME"/.workbuddy/binaries/node/versions/*/lib/node_modules/workbuddy-switch/bin/wb-switch-darwin-arm64.real; do
      if [ -x "$fallback" ]; then
        exec "$fallback" "$@"
      fi
    done
    log "no fallback binary found; service cannot start"
    exit 1
  fi
else
  log "no source change -> using cached build"
fi

exec "$ROOT/target/release/wb-switch" "$@"
