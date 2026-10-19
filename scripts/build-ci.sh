#!/usr/bin/env bash
# 构建脚本（Cloudflare Workers Builds 与本地通用）。
#
# 目标环境为 Cloudflare 构建镜像（Ubuntu 24.04）：有 libssl-dev，但没有
# Rust 工具链和 pkg-config。worker-build 经 ureq/native-tls 依赖 openssl-sys，
# 默认靠 pkg-config 探测 OpenSSL，缺失会直接 panic，因此这里显式给出
# OPENSSL_INCLUDE_DIR / OPENSSL_LIB_DIR 走直接链接路径。
set -euo pipefail

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# OpenSSL 路径（Ubuntu 24.04 x86_64）
export OPENSSL_INCLUDE_DIR="${OPENSSL_INCLUDE_DIR:-/usr/include}"
export OPENSSL_LIB_DIR="${OPENSSL_LIB_DIR:-/usr/lib/x86_64-linux-gnu}"
log "OpenSSL: include=${OPENSSL_INCLUDE_DIR} lib=${OPENSSL_LIB_DIR}"

if ! command -v cargo >/dev/null 2>&1; then
  log "Installing Rust toolchain"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
log "Rust: $(cargo --version)"

# rust-toolchain.toml 声明了 wasm32-unknown-unknown，rustup 会自动就位。
if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
  log "Adding wasm32-unknown-unknown"
  rustup target add wasm32-unknown-unknown
fi

if ! command -v worker-build >/dev/null 2>&1; then
  log "Installing worker-build 0.8.7（首次约 3 分钟）"
  cargo install worker-build --version 0.8.7 --locked
fi

log "Building wasm (release)"
worker-build --release

log "Done: build/worker/shim.mjs"
