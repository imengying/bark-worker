#!/usr/bin/env bash
# Cloudflare Workers Builds（网页 + GitHub 在线部署）用的一键构建脚本。
#
# 云端构建镜像（Ubuntu 24.04）只预装了 libssl-dev / build-essential，
# **没有 Rust 工具链，也没有 pkg-config**。而 worker-build 经 ureq/native-tls
# 依赖 openssl-sys，默认要靠 pkg-config 探测 OpenSSL，缺了就直接 panic。
# 本脚本做两件事：
#   1. 幂等安装 Rust 工具链 + wasm32-unknown-unknown 目标；
#   2. 显式给出 OPENSSL_LIB_DIR / OPENSSL_INCLUDE_DIR，让 openssl-sys 走
#      直接链接路径，从而无需 pkg-config（已实测可完整编译 worker-build）。
#
# 版本由仓库根目录的 rust-toolchain.toml 固定，rustup 会自动就位。
set -euo pipefail

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# --- 1. OpenSSL：绕开 pkg-config 探测 -------------------------------------
if [ -z "${OPENSSL_INCLUDE_DIR:-}" ] && [ -d /usr/include/openssl ]; then
  export OPENSSL_INCLUDE_DIR=/usr/include
fi
if [ -z "${OPENSSL_LIB_DIR:-}" ]; then
  for d in /usr/lib/x86_64-linux-gnu /usr/lib64 /usr/lib /usr/local/lib; do
    if [ -e "$d/libssl.so" ] || [ -e "$d/libssl.so.3" ] || [ -e "$d/libssl.a" ]; then
      export OPENSSL_LIB_DIR="$d"
      break
    fi
  done
fi
if [ -z "${OPENSSL_LIB_DIR:-}" ] || [ -z "${OPENSSL_INCLUDE_DIR:-}" ]; then
  log "WARNING: could not locate OpenSSL; worker-build install may fail"
else
  log "OpenSSL: include=${OPENSSL_INCLUDE_DIR} lib=${OPENSSL_LIB_DIR}"
fi

# --- 2. Rust 工具链 -------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "Installing Rust toolchain via rustup"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
else
  log "Rust: $(cargo --version)"
fi

# 触发 rust-toolchain.toml：rustup 会自动安装声明的 channel 与 targets。
if command -v rustup >/dev/null 2>&1; then
  rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install
  if ! rustup target list --installed 2>/dev/null | grep -qx wasm32-unknown-unknown; then
    log "Adding wasm32-unknown-unknown"
    rustup target add wasm32-unknown-unknown
  fi
else
  # 没有 rustup（例如发行版打包的 cargo）时，只能信任 wasm32 目标已就位。
  log "rustup not found; assuming wasm32-unknown-unknown is already installed"
fi

# --- 3. worker-build ------------------------------------------------------
if ! command -v worker-build >/dev/null 2>&1; then
  log "Installing worker-build 0.8.7（首次约 3 分钟）"
  cargo install worker-build --version 0.8.7 --locked
else
  log "worker-build: $(worker-build --version 2>/dev/null || echo installed)"
fi

# --- 4. 构建 --------------------------------------------------------------
log "Building wasm (release)"
worker-build --release

log "Done: build/worker/shim.mjs"
