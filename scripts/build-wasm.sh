#!/usr/bin/env sh
# Builds the browser-side LCF1 codec (lz4 chunk compression) to WebAssembly and
# places it where the server embeds it from (web/lcf.wasm).
set -eu
cd "$(dirname "$0")/.."
cargo build -p lcf-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/lcf_wasm.wasm web/lcf.wasm
ls -l web/lcf.wasm
