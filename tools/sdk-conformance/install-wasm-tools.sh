#!/usr/bin/env sh
# Installs the pinned wasm-tools release into WASM_TOOLS_DIR (default /usr/local/bin).
#
# Used by the conformance harness's container `provision` step for languages that need to
# componentize a core module (Rust builds for wasm32-unknown-unknown). It is a no-op when
# `wasm-tools` is already on PATH. The version is pinned so a run is reproducible.
set -eu

version="${WASM_TOOLS_VERSION:-1.262.0}"
dest="${WASM_TOOLS_DIR:-/usr/local/bin}"

if command -v wasm-tools >/dev/null 2>&1; then
  wasm-tools --version
  exit 0
fi

case "$(uname -m)" in
  x86_64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) echo "install-wasm-tools: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac

url="https://github.com/bytecodealliance/wasm-tools/releases/download/v${version}/wasm-tools-${version}-${arch}-linux.tar.gz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "install-wasm-tools: downloading $url"
curl -fsSL "$url" -o "$tmp/wasm-tools.tar.gz"
tar -xzf "$tmp/wasm-tools.tar.gz" -C "$tmp"
cp "$tmp/wasm-tools-${version}-${arch}-linux/wasm-tools" "$dest/wasm-tools"
chmod +x "$dest/wasm-tools"
"$dest/wasm-tools" --version
