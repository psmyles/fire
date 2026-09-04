#!/usr/bin/env bash
# Regenerate the viewport shader's per-backend sources and sokol_gfx reflection from the one
# annotated-GLSL source, `crates/fire/src/render/shader.glsl` (D4).
#
# Everything this writes under `crates/fire/src/render/generated/` is checked in, so a plain
# `cargo build` never needs sokol-shdc — build.rs only compiles the generated source for the host's
# backend to bytecode (fxc -> DXBC on Windows, `xcrun metal` -> .metallib on macOS). Run this after
# editing shader.glsl, on any OS, and commit the result: the output is platform-independent text.
#
# sokol-shdc is a prebuilt binary from floooh/sokol-tools-bin. Point SOKOL_SHDC at a copy, or let
# this script fetch one into the repo-ignored `.tools/`.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src="$repo/crates/fire/src/render/shader.glsl"
gen="$repo/crates/fire/src/render/generated"
slangs="hlsl5:metal_macos"

# --- locate sokol-shdc ------------------------------------------------------
shdc="${SOKOL_SHDC:-}"
if [[ -z "$shdc" ]]; then
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) plat=osx_arm64 ;;
        Darwin-x86_64) plat=osx ;;
        Linux-x86_64) plat=linux ;;
        *) echo "no sokol-shdc build known for $(uname -s)-$(uname -m); set SOKOL_SHDC" >&2; exit 1 ;;
    esac
    shdc="$repo/.tools/sokol-shdc"
    if [[ ! -x "$shdc" ]]; then
        echo "fetching sokol-shdc ($plat) into .tools/ ..."
        mkdir -p "$repo/.tools"
        curl -sSLf -o "$shdc" \
            "https://raw.githubusercontent.com/floooh/sokol-tools-bin/master/bin/$plat/sokol-shdc"
        chmod +x "$shdc"
    fi
fi

mkdir -p "$gen"

# --- 1. per-backend shader sources, for build.rs to compile to bytecode ------
"$shdc" -i "$src" -o "$gen/shader" -l "$slangs" -f bare

# --- 2. the sokol_gfx reflection (bindings + entry points) as a Rust module ---
# Generated without --bytecode on purpose: this file is checked in and must be identical whichever
# OS regenerates it, so it carries *source* and build.rs supplies the bytecode. `make_shader`
# overrides the func fields and keeps everything else this describes.
"$shdc" -i "$src" -o "$gen/shader.rs" -l "$slangs" -f sokol_rust

echo "regenerated:"
ls -1 "$gen"
