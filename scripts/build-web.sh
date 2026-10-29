#!/usr/bin/env bash
# Build the browser version of Terminal (WebAssembly) into a static folder.
#
#   scripts/build-web.sh [out-dir]    # default: site/static/app, served by the site at /app/
#
# It writes two builds: terminal_bg.wasm (WebGPU) and terminal-webgl_bg.wasm (adds the
# WebGL2 fallback, which is larger and draws no charts). The page loads the second only when
# the browser has no WebGPU adapter.
#
# Needs the wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown),
# wasm-bindgen-cli at the version pinned in Cargo.lock, and wasm-opt from binaryen.
set -euo pipefail
cd "$(dirname "$0")/.."

out="${1:-site/static/app}"
target=wasm32-unknown-unknown
wasm="target/$target/web/terminal-rs.wasm"

want=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
have=$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)
if [ "$want" != "$have" ]; then
	echo "wasm-bindgen-cli ${have:-is missing}; Cargo.lock wants $want:" >&2
	echo "  cargo install wasm-bindgen-cli --version $want --locked" >&2
	exit 1
fi
command -v wasm-opt >/dev/null || { echo "wasm-opt is missing: install binaryen" >&2; exit 1; }

# The wasm features rustc enables for this target (rustc --print cfg --target wasm32-unknown-unknown).
features=(--enable-bulk-memory --enable-bulk-memory-opt --enable-call-indirect-overlong --enable-multivalue
	--enable-mutable-globals --enable-nontrapping-float-to-int --enable-reference-types --enable-sign-ext)

mkdir -p "$out"
rm -f "$out"/terminal*.js "$out"/terminal*.wasm

# build <name> [cargo args...]
build() {
	local name=$1
	shift
	cargo build --profile web --target "$target" -p terminal-rs "$@"
	wasm-bindgen --target web --no-typescript --out-dir "$out" --out-name "$name" "$wasm"
	wasm-opt -Os "${features[@]}" --strip-debug --strip-producers "$out/${name}_bg.wasm" -o "$out/${name}_bg.wasm"
}
build terminal
build terminal-webgl --features webgl
cp app/web/index.html "$out/index.html"

size() { wc -c <"$1" | tr -d ' '; }
echo "wrote $out"
for name in terminal terminal-webgl; do
	f="$out/${name}_bg.wasm"
	gz=$(gzip -9c "$f" | wc -c | tr -d ' ')
	br=$(command -v brotli >/dev/null && brotli -c -q 11 "$f" | wc -c | tr -d ' ' || echo "n/a")
	echo "${name}_bg.wasm: $(size "$f") bytes raw, $gz gzip -9, $br brotli -q 11; ${name}.js: $(size "$out/$name.js") bytes"
done
