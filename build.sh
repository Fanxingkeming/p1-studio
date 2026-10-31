#!/usr/bin/env bash
# Build Metaphyuni玄熊猫P1管理器 as a single portable executable.
#
# WebView2Loader.dll is carried inside the binary (see vendor/webview2-com-sys),
# so the only deliverable is the .exe itself — no DLL next to it.
#
# Size knobs live in src-tauri/Cargo.toml (opt-level "s", fat LTO, one codegen
# unit, abort on panic, stripped). This script adds one optional pass:
#
#   UPX=1   pack the finished exe with upx --best, roughly a 2x reduction in
#           bytes on disk. Off by default: packed binaries are more likely to
#           be flagged by antivirus heuristics, and they unpack into memory at
#           every start, which costs a few hundred milliseconds.
#   OUT_COPY=<dir> also drops a copy of the exe there, for a share or sync folder.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Rust and the MinGW toolchain are usually not on PATH for a non-login shell.
for dir in "$HOME/.cargo/bin" "$HOME/scoop/shims" /c/msys64/mingw64/bin /c/gb/mingw64/mingw64/bin; do
  [ -d "$dir" ] && PATH="$dir:$PATH"
done
export PATH

python - "$ROOT/src-tauri/tauri.conf.json" <<'PY'
import json, sys, io
path = sys.argv[1]
cfg = json.load(io.open(path, encoding='utf-8'))
cfg['productName'] = 'Metaphyuni玄熊猫P1管理器'
cfg['build']['frontendDist'] = '../ui'
cfg['app']['windows'][0]['title'] = 'Metaphyuni玄熊猫P1管理器'
io.open(path, 'w', encoding='utf-8').write(json.dumps(cfg, ensure_ascii=False, indent=2) + '\n')
print('config -> Metaphyuni玄熊猫P1管理器 / ../ui')
PY

# The frontend is embedded in the binary, so its bytes are part of the exe.
# Report it: when this number jumps, an asset grew, not the Rust code.
front="$(cat "$ROOT/ui/index.html" "$ROOT/ui/app.js" "$ROOT/ui/styles.css" | wc -c)"
echo "frontend: ${front} bytes before compression"

(cd "$ROOT/src-tauri" && cargo build --release)

out="$ROOT/out"
mkdir -p "$out"
exe="$out/Metaphyuni玄熊猫P1管理器.exe"
cp "$ROOT/src-tauri/target/release/p1studio.exe" "$exe"
# The loader used to sit next to the exe; it is embedded now, so drop the file.
rm -f "$out/WebView2Loader.dll" "$out/p1studio.exe" "$out/SKIN.txt" "$out/p1studio.exe.upx"

before="$(stat -c%s "$exe")"
if [ "${UPX:-0}" = "1" ]; then
  if command -v upx >/dev/null 2>&1; then
    upx --best --lzma -q "$exe" >/dev/null
    after="$(stat -c%s "$exe")"
    echo "upx: $before -> $after bytes ($((100 - after * 100 / before))% smaller)"
  else
    echo "UPX=1 but no upx on PATH — leaving the exe unpacked"
  fi
fi

# Optional second copy, e.g. a shared folder. Left off unless asked for.
if [ -n "${OUT_COPY:-}" ]; then
  mkdir -p "$OUT_COPY" && cp "$exe" "$OUT_COPY/"
fi
echo "packaged -> $exe ($(stat -c%s "$exe") bytes)"
