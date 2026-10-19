#!/usr/bin/env bash
# 构建 Worker：安装缺失的 Rust 工具链 -> worker-build。
#
# 目标环境为 Cloudflare 构建镜像（Ubuntu 24.04）：有 libssl-dev，但没有
# Rust 与 pkg-config。worker-build 经 ureq/native-tls 依赖 openssl-sys，
# 默认靠 pkg-config 探测 OpenSSL，缺失会 panic，因此显式给出路径。
#
# 本脚本可能在一次部署中被调用两次（Dashboard 的 Build command 一次，
# wrangler deploy 的 build.command 一次），因此已构建时直接复用产物。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

log() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# 产物已存在且比所有源文件新 -> 跳过，避免重复构建。
if [ -f build/worker/shim.mjs ] && [ -z "$(find src Cargo.toml Cargo.lock -newer build/worker/shim.mjs 2>/dev/null)" ]; then
  log "Already built: build/worker/shim.mjs"
  exit 0
fi

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

# rust-toolchain.toml 声明了 wasm32-unknown-unknown。
if ! rustup target list --installed | grep -qx wasm32-unknown-unknown; then
  log "Adding wasm32-unknown-unknown"
  rustup target add wasm32-unknown-unknown
fi

if ! command -v worker-build >/dev/null 2>&1; then
  log "Installing worker-build 0.8.7（首次约 5 分钟）"
  cargo install worker-build --version 0.8.7 --locked
fi

log "Building wasm (release)"
worker-build --release

log "Done: build/worker/shim.mjs"
