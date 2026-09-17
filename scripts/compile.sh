#!/usr/bin/env bash
# 在 x86_64 主机交叉编译 aarch64 musl 静态二进制，产物：dist/rs485-test
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET="aarch64-unknown-linux-musl"
TOOLS_DIR="$ROOT/tools"
# Bootlin 可从本机网络访问；musl.cc 在部分网络会 SSL 失败。
# 也可手动放置任意 aarch64 musl 交叉工具链目录到 tools/ 下。
BOOTLIN_VER="${BOOTLIN_VER:-aarch64--musl--stable-2024.05-1}"
BOOTLIN_URL="${BOOTLIN_URL:-https://toolchains.bootlin.com/downloads/releases/toolchains/aarch64/tarballs/${BOOTLIN_VER}.tar.xz}"
TARBALL="$TOOLS_DIR/${BOOTLIN_VER}.tar.xz"
CROSS_PREFIX=""

log() { printf '[compile] %s\n' "$*"; }
die() { printf '[compile] 错误: %s\n' "$*" >&2; exit 1; }

find_cross_gcc() {
  local gcc
  shopt -s nullglob
  local candidates=(
    "$TOOLS_DIR"/*/bin/aarch64-linux-gcc
    "$TOOLS_DIR"/*/bin/aarch64-*-linux-musl-gcc
    "$TOOLS_DIR"/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc
  )
  shopt -u nullglob
  for gcc in "${candidates[@]}"; do
    if [[ -e "$gcc" ]]; then
      printf '%s' "$gcc"
      return 0
    fi
  done
  return 1
}

ensure_rustup() {
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  if command -v rustup >/dev/null 2>&1 && command -v cargo >/dev/null 2>&1; then
    return 0
  fi
  log "未检测到 rustup，正在安装到用户目录（无需 sudo）..."
  command -v curl >/dev/null 2>&1 || die "需要 curl"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
  command -v cargo >/dev/null 2>&1 || die "rustup 安装后仍找不到 cargo"
}

ensure_musl_cross() {
  if CROSS_GCC="$(find_cross_gcc)"; then
    CROSS_PREFIX="$(dirname "$CROSS_GCC")"
    return 0
  fi
  mkdir -p "$TOOLS_DIR"
  if [[ ! -f "$TARBALL" ]]; then
    log "下载 aarch64 musl 交叉工具链: $BOOTLIN_URL"
    log "若失败，请手动将 tar.xz 放到: $TARBALL"
    curl -fL --retry 3 --retry-delay 2 -o "$TARBALL.partial" "$BOOTLIN_URL" \
      || die "下载工具链失败。请手动下载后放到 $TARBALL 再重跑"
    mv "$TARBALL.partial" "$TARBALL"
  fi
  log "解压工具链到 $TOOLS_DIR ..."
  tar -xJf "$TARBALL" -C "$TOOLS_DIR"
  CROSS_GCC="$(find_cross_gcc)" || die "解压后未找到 aarch64-*-gcc"
  CROSS_PREFIX="$(dirname "$CROSS_GCC")"
}

ensure_rustup
ensure_musl_cross

export PATH="$CROSS_PREFIX:$PATH"
export CC_aarch64_unknown_linux_musl="$CROSS_GCC"
export AR_aarch64_unknown_linux_musl="$(dirname "$CROSS_GCC")/$(basename "$CROSS_GCC" gcc)ar"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$CROSS_GCC"

if [[ ! -x "${AR_aarch64_unknown_linux_musl}" ]]; then
  export AR_aarch64_unknown_linux_musl="$(command -v llvm-ar || command -v ar || true)"
fi

log "rustc: $(rustc --version)"
log "linker: $CROSS_GCC"

log "添加 Rust target: $TARGET"
rustup target add "$TARGET"

log "开始编译 --release --target $TARGET"
cargo build --release --target "$TARGET"

BIN_SRC="$ROOT/target/$TARGET/release/rs485-test"
[[ -f "$BIN_SRC" ]] || die "未找到编译产物 $BIN_SRC"

mkdir -p "$ROOT/dist"
cp -f "$BIN_SRC" "$ROOT/dist/rs485-test"
chmod +x "$ROOT/dist/rs485-test"

log "产物: $ROOT/dist/rs485-test"
if command -v file >/dev/null 2>&1; then
  file "$ROOT/dist/rs485-test"
fi
if command -v readelf >/dev/null 2>&1; then
  if readelf -d "$ROOT/dist/rs485-test" 2>/dev/null | grep -q NEEDED; then
    log "警告: 仍有动态依赖（期望静态链接）"
    readelf -d "$ROOT/dist/rs485-test" | grep NEEDED || true
  else
    log "动态段: 无 NEEDED，静态链接检查通过"
  fi
fi

log "完成后把 dist/rs485-test 与 scripts/start.sh、scripts/stop.sh 拷到目标板即可。"
