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
    # One binary per platform directory in sokol-tools-bin. `win32` is that repo's name for the
    # Windows build (an x86_64 exe, despite the directory name) and the only one carrying a file
    # suffix, so the suffix travels with the platform instead of being appended at each use site.
    exe=""
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) plat=osx_arm64 ;;
        Darwin-x86_64) plat=osx ;;
        Linux-x86_64) plat=linux ;;
        # Git Bash / MSYS2 / Cygwin, whose `uname -s` is `MINGW64_NT-10.0-26200` and kin.
        MINGW*|MSYS*|CYGWIN*) plat=win32; exe=.exe ;;
        *) echo "no sokol-shdc build known for $(uname -s)-$(uname -m); set SOKOL_SHDC" >&2; exit 1 ;;
    esac
    shdc="$repo/.tools/sokol-shdc$exe"
    if [[ ! -x "$shdc" ]]; then
        echo "fetching sokol-shdc ($plat) into .tools/ ..."
        mkdir -p "$repo/.tools"
        curl -sSLf -o "$shdc" \
            "https://raw.githubusercontent.com/floooh/sokol-tools-bin/master/bin/$plat/sokol-shdc$exe"
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

# --- 3. make shader.rs the same file whoever regenerated it ------------------
# Two things in the sokol_rust output are machine-specific, and neither changes a byte of the
# shader: the absolute -i/-o paths shdc echoes into its header comment, and the Rust emitter's
# formatting, which differs between the per-platform sokol-shdc binaries because they are built
# from different upstream revisions (win32 writes `0x01,0x02` and a trailing comma on the last
# match arm; osx_arm64 writes `0x01, 0x02` and none). Left alone they churn ~2300 lines every
# time the OS regenerating this changes, burying the handful of lines a real shader edit moves.
#
# rustfmt settles the formatting half: the repo carries no rustfmt.toml of its own, so this is
# plain defaults, and the generated file is already clean under them — the two emitters converge
# on the same bytes. `newline_style` is pinned rather than left on Auto because shdc writes CRLF
# on Windows and LF elsewhere, and Auto would preserve each.
if ! command -v rustfmt >/dev/null; then
    echo "rustfmt not found; it ships with the toolchain (rustup component add rustfmt)" >&2
    exit 1
fi
rustfmt --edition 2021 --config newline_style=Unix "$gen/shader.rs"
# The paths, rewritten repo-relative. Anchored to the Cmdline line so nothing else can match.
sed -E '/sokol-shdc -i /s#[^ ]*crates/fire/src/render/#crates/fire/src/render/#g' "$gen/shader.rs" > "$gen/shader.rs.tmp"
mv "$gen/shader.rs.tmp" "$gen/shader.rs"

echo "regenerated:"
ls -1 "$gen"
