#!/usr/bin/env bash
# Build the browser version of Terminal (WebAssembly) into a static folder.
#
#   scripts/build-web.sh [out-dir]    # default: site/static/app, served by the site at /app/
#
# It writes two builds: terminal_bg.wasm (WebGPU) and terminal-webgl_bg.wasm (adds the
# WebGL2 fallback, which is larger and draws no charts). The page loads the second only when
# the browser has no WebGPU adapter.
#
# Each build's JS glue and wasm go in immutable/<hash>/, a folder named after a hash of its
# files, and index.html points at those folders. A new build gets new URLs, so the site can
# cache everything under immutable/ for good (site/vercel.json), and a page can't pair one
# build's JS with another build's wasm.
#
# The app gets crypto markets from the Terminal data server at TR_SERVER_URL, which is built
# in. It defaults to the production server; set it to another ws:// or wss:// URL, or to
# nothing (TR_SERVER_URL=) for no server. A page can also ask for a server on the same machine
# with ?server=ws://localhost:8787/ws, or for none with ?server=off.
#
# Needs the wasm32-unknown-unknown target (rustup target add wasm32-unknown-unknown),
# wasm-bindgen-cli at the version pinned in Cargo.lock, and wasm-opt from binaryen.
set -euo pipefail
cd "$(dirname "$0")/.."

out="${1:-site/static/app}"
target=wasm32-unknown-unknown
wasm="${CARGO_TARGET_DIR:-target}/$target/web/terminal.wasm"

export TR_SERVER_URL="${TR_SERVER_URL-wss://terminal-gateway.style.dev/ws}"
case "$TR_SERVER_URL" in
"") echo "no Terminal server: crypto markets come from Binance and Coinbase" ;;
ws://* | wss://*) echo "Terminal server: $TR_SERVER_URL" ;;
*)
	echo "TR_SERVER_URL must be a ws:// or wss:// URL, or empty for no server" >&2
	exit 1
	;;
esac

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
rm -rf "$out"/terminal* "$out/immutable" "$out/fonts"

# sed expressions that replace each {name} in index.html with that build's path
paths=()

# SHA-256 of stdin. Older macOS has shasum but no sha256sum.
sha256() { if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi; }

# build <name> [cargo args...]
build() {
	local name=$1 tmp="$out/immutable/.$1" hash
	shift
	cargo build --profile web --target "$target" -p terminal "$@"
	wasm-bindgen --target web --no-typescript --out-dir "$tmp" --out-name "$name" "$wasm"
	wasm-opt -Os "${features[@]}" --strip-debug --strip-producers "$tmp/${name}_bg.wasm" -o "$tmp/${name}_bg.wasm"
	hash=$(cd "$tmp" && find . -type f | LC_ALL=C sort | xargs cat | sha256 | cut -c 1-16)
	mv "$tmp" "$out/immutable/$hash"
	paths+=(-e "s#{$name}#immutable/$hash/$name#g")
}
build terminal
build terminal-webgl --features webgl
sed "${paths[@]}" app/web/index.html >"$out/index.html"
if grep -n '{terminal[a-z-]*}' "$out/index.html" >&2; then
	echo "index.html names a build that this script doesn't make" >&2
	exit 1
fi

# egui's emoji fonts, which the page fetches after start-up (see app/src/web.rs), with their licenses.
fonts=$(cargo metadata --format-version 1 --locked --filter-platform "$target" | python3 -c '
import json, os, sys
pkgs = json.load(sys.stdin)["packages"]
print(next(os.path.dirname(p["manifest_path"]) for p in pkgs if p["name"] == "epaint_default_fonts"))')/fonts
mkdir -p "$out/fonts"
cp "$fonts"/{NotoEmoji-Regular.ttf,OFL.txt,emoji-icon-font.ttf,emoji-icon-font-mit-license.txt} "$out/fonts/"

size() { wc -c <"$1" | tr -d ' '; }
echo "wrote $out"
for f in "$out"/immutable/*/*_bg.wasm; do
	gz=$(gzip -9c "$f" | wc -c | tr -d ' ')
	br=$(command -v brotli >/dev/null && brotli -c -q 11 "$f" | wc -c | tr -d ' ' || echo "n/a")
	echo "${f#"$out"/}: $(size "$f") bytes raw, $gz gzip -9, $br brotli -q 11; $(basename "${f%_bg.wasm}").js: $(size "${f%_bg.wasm}.js") bytes"
done
