#!/usr/bin/env bash
# Cloudflare Workers Builds（网页 + GitHub 在线部署）用的一键构建脚本。
#
# 设计目标：
#   1. 云端构建镜像没有 Rust 工具链，这里自动安装（幂等）。
#   2. worker-build 的 openssl-sys 依赖编译时**不需要** pkg-config：
#      直接给 OPENSSL_LIB_DIR / OPENSSL_INCLUDE_DIR 即可（已实测）。
#   3. worker-build 本身编译较久，用版本戳缓存避免每次重建。
#
# 本地开发通常不需要它（直接 `bun run build` 即可，前提是已装好工具链）。
set -euo pipefail

RUST_CHANNEL="${RUST_CHANNEL:-stable}"
WORKER_BUILD_VERSION="${WORKER_BUILD_VERSION:-0.8.7}"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# --- 1. OpenSSL：绕开 pkg-config 探测 -------------------------------------
# 云镜像预装 libssl-dev，但可能没有 pkg-config；openssl-sys 在找不到 pkg-config
# 时会 panic。显式给出库目录与头文件目录即可走直接链接路径。
if [ -z "${OPENSSL_INCLUDE_DIR:-}" ] && [ -d /usr/include/openssl ]; then
  export OPENSSL_INCLUDE_DIR=/usr/include
fi
if [ -z "${OPENSSL_LIB_DIR:-}" ]; then
  for d in /usr/lib/x86_64-linux-gnu /usr/lib64 /usr/lib; do
    if [ -e "$d/libssl.so" ] || [ -e "$d/libssl.so.3" ] || [ -e "$d/libssl.a" ]; then
      export OPENSSL_LIB_DIR="$d"
      break
    fi
  done
fi
log "OPENSSL_INCLUDE_DIR=${OPENSSL_INCLUDE_DIR:-<unset>} OPENSSL_LIB_DIR=${OPENSSL_LIB_DIR:-<unset>}"

# --- 2. Rust 工具链 -------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "Installing Rust toolchain ($RUST_CHANNEL)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain "$RUST_CHANNEL"
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
else
  log "Rust already available: $(cargo --version)"
fi

if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
  log "Adding wasm32-unknown-unknown target"
  rustup target add wasm32-unknown-unknown
fi

# --- 3. worker-build（带缓存） -------------------------------------------
STAMP="${CARGO_HOME:-$HOME/.cargo}/.worker-build-${WORKER_BUILD_VERSION}"
if ! command -v worker-build >/dev/null 2>&1 || [ ! -f "$STAMP" ]; then
  log "Installing worker-build $WORKER_BUILD_VERSION（首次约 2-4 分钟）"
  cargo install worker-build --version "$WORKER_BUILD_VERSION" --locked
  touch "$STAMP"
else
  log "worker-build already installed"
fi

# --- 4. 构建 --------------------------------------------------------------
log "Building wasm (release)"
worker-build --release

log "Done: build/worker/shim.mjs"
