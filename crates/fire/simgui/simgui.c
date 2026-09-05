// The sokol_imgui implementation, compiled by build.rs.
//
// sokol_imgui.h is the Dear ImGui *renderer* backend for sokol_gfx (SOKOL_IMGUI_NO_SOKOL_APP: input
// comes from winit through dear-imgui-winit, not from sokol_app): it uploads
// ImGui's textures (fonts included, through the 1.92 texture protocol) as sokol_gfx images and
// draws the draw lists into the current sokol_gfx pass. It talks to Dear ImGui through the cimgui
// C API — the same API `dear-imgui-sys` compiles and links — so this translation unit sees the
// exact struct layouts the Rust side does. The three ImGui defines (CIMGUI_DEFINE_ENUMS_AND_STRUCTS,
// IMGUI_USE_WCHAR32, IMGUI_DISABLE_OBSOLETE_FUNCTIONS) and the sokol backend define come from
// build.rs, which keeps them in one place next to the same defines dear-imgui-sys uses.
//
// cimgui.h here is a copy of dear-imgui-sys 0.17.0's (cimgui bf9b984, Dear ImGui 1.92.9b docking);
// bump both together.
#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>
#include "sokol_gfx.h"
#include "cimgui.h"
#define SOKOL_IMGUI_IMPL
#include "sokol_imgui.h"
