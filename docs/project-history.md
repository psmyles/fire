# Fire — Project History: Every Conversation and What Came of It

*A chronological record of all the Claude Code sessions behind Fire, with the resulting actions,
commits, and every benchmark number recovered from the transcripts. Compiled 2026-07-18 as source
material for a blog post.*

A note on sources: sessions from July 1 onward are reconstructed from the actual conversation
transcripts. The June 25–30 period predates the oldest surviving transcript and is reconstructed
from git history alone. The two long-form writeups produced during these sessions are included
verbatim at the end of this document — **Appendix A** (flipbook detection) and **Appendix B**
(the ImGui migration) — so this file is fully self-contained; everything in the timeline that
isn't in those two writeups was mined directly from the session transcripts.

---

## Phase 1: Foundation (June 25–30) — *from git history; no transcripts survive*

- **Jun 25 — Project inception.** Base project, architecture doc, workspace crates + walking
  skeleton, zune-based decoding (phases 1–3 of the original plan), binaries renamed, icon assets
  applied. The renderer changed twice on day one: wgpu/winit/egui → CPU-only renderer (PR #1),
  then the old stack was deleted.
- **Jun 26 — The D3D11 pivot & core viewer.** DX11 GPU renderer, pan/zoom controls, offline shader
  precompile via `fxc`, AVIF/HEIF/HEIC/ICO support, installer flow (Inno Setup), drag-and-drop
  open, folder ←/→ navigation, hot-reload of changed files on disk, docs cleaned of old tech-stack
  references.
- **Jun 29 — Formats & chrome.** Basic camera RAW support (embedded preview extraction), TGA
  loading + false-alpha fixes, toolbar icons (four commits!), image outline system, viewport
  startup/switching fixes, tooltips, the context menu system, alpha-handling polish, a
  code-quality pass.
- **Jun 30 — Behavior polish.** Fit-to-window adjustments, nested submenu entries from config,
  showing file format after Explorer association.

No measured numbers survive from this period, except one legacy figure quoted later: an early docs
claim of **≈392 ms to decode an 8192×4096 PNG via zune** (exposed as stale in session 16 below).

---

## Phase 2: Feature sprint (July 1–5)

### 1. Full-screen mode *(Jul 1)*
Icon button from an SVG asset, Esc to exit, middle-click on the viewport to toggle.
→ `c27af1f`

### 2. .gitignore housekeeping *(Jul 1)*
Quick `.gitignore` update.

### 3. HDR exposure reset button *(Jul 1)*
An EV-reset button between the exposure decrease/increase buttons, shown only for HDR images.
→ `c32bfe7`

### 4. Animated GIF playback *(Jul 1)*
GIFs loaded but showed only the first frame; made them actually play. One number worth quoting:
**per-frame delays under 20 ms (including the common `0`) clamp to 100 ms**, matching browser
behavior. Established the "one texture re-upload per *animation* frame, paced by a Win32 timer,
never per render frame" exception to the upload-once rule.
→ `bbca17f`

### 5. HDR artifacts → decoder benchmarks → decoder switch *(Jul 3)*
Started as a bug report (`T_Skybox_NightFog05.hdr` rendered with artifacts vs. Photoshop) and
turned into a data-driven decoder investigation.

- Root cause traced into zune-hdr's RGBE conversion (an exponent bug — a pixel at E=96 must return
  `0.5 × 2⁻³²`, zune returned `0.5`). After the fix, output matched the reference decoder on
  **all 8.4M pixels**, where it previously mismatched on **306k pixels (3.65%)**.
- **HDR benchmark** (27.4 MB, 4096×2048 Radiance HDR, 10 runs, release, median):

  | Path | Time |
  |---|---|
  | zune-hdr raw decode (f32 RGB) | 164–184 ms |
  | zune via zune-image + RGBA f32 (Fire's then-current path) | 218–**283 ms** |
  | image crate decode (f32 RGB) | 63–90 ms |
  | image crate + RGBA f32 (proposed) | 94–**135 ms** |

  Verdict: image crate **~2× faster**; Fire went from ~283 ms → ~135 ms on that file. → `e64d76f`
- **PNG benchmark** (two 8192×4096 RGB8 textures, 39.7 MB and 60.0 MB, median of 8): zune hot path
  **339 / 367 ms** vs image crate **192 / 208 ms** — **~1.8× win**, gap is in the core decode
  (zune-png raw alone: 307/333 ms vs image 170/186 ms). Expected real-world effect:
  **~340 ms → ~190 ms** on textures that size. → `292bb4f`
- **JPEG benchmark** (73.9 MB scan → 17784×12168, 216 MP, median of 8): zune path **1055 ms** vs
  image crate **1006 ms** — a wash, so **JPEG stayed on zune**.
- Implementation gotcha: the image crate's default `ImageReader` limit would have rejected the
  216 MP JPEG at **512 MB**; direct construction with `no_limits()` plus a `MAX_DECODE_DIM` header
  check replaced it.

The blog-worthy shape: "benchmark before you believe the 'fast' crate" — the zune-is-the-hot-path
assumption held for JPEG, but not for HDR or PNG.

### 6. Empty-state hint + double-click to open *(Jul 5)*
Centered "Drop an image file here / double click to open file browser" text on the empty viewport,
plus double-click wiring to the file dialog. Included a screenshot-verification comedy of errors
(the app was fine; the capture was off-center).
→ `48bf4a8`

---

## Phase 3: Flipbook epic (July 11–12)

### 7. Toolbar overflow menu + minimum window size *(Jul 11)*
Icons overlapped at small window sizes; options were discussed, an overflow (hamburger) menu was
picked, then a minimum window size set.
→ `0080233`

### 8. The flipbook viewer & detection system *(Jul 11 — the 8.7 MB transcript)*
A saga in acts (full numeric story in **Appendix A** below):

- A flipbook-viewer plan had accidentally been run against the *wrong repo*; it was validated
  against fire and adapted (full transport bar, numeric fields with drag + wheel + typed input).
- Bring-up bugs: the flipbook hint floated outside the window and persisted over other apps; the
  flipbook toolbar didn't appear on mode entry.
- Detection tuned against a real test corpus: **23 files** (16 flipbooks named `_NxM_FB`, 7
  non-flipbook textures).
- The old detector was confidently wrong: 5×5 sheets read as 2×2, 6×6 as 2×4, 8×8 as 16×2 — and a
  5×5 grid on a 2048 canvas was *never even a candidate* (2048 ÷ 5 = **409.6 px** fractional cells).
- Plain autocorrelation failed too (`FireFar` 8×8 misread as 5×5 — the animation's growing blobs
  contaminated the period). **YIN** — a pitch-detection algorithm from audio — fixed it and handles
  the 409.6 px fractional period natively.
- The confidence data came out cleanly bimodal: real grids scored **< 0.15**, non-grids **> 0.55**,
  nothing in between → acceptance threshold set at **0.35** in the empty gap (an earlier 0.55
  threshold sat a razor's edge from a bat-wing texture's 0.554).
- Tiling guard: reject only at far-half-loop similarity **≥ 0.999**; real smoke sheets measured up
  to **0.9955** and pass.
- **The alpha-channel insight** (user-supplied): `WispySteam` has data *only* in alpha (RGB solid
  white), `FireFar`'s alpha is crisper than its smoky RGB. A luma→alpha cascade (alpha pass only
  when luminance finds nothing) recovered both without regressing `Ground_Shockwave`.
- Strip guard for 1-row/1-column sheets: adjacent-frame similarity threshold **~0.45** (bat-wing
  false 1×3 scored **0.25**, real 3×1 plant strip **0.59**).
- Final score: **14 of 16** flipbooks from content alone (2 fall back to the filename token),
  **0 false positives**, **0** content-vs-correct-token disagreements. Content is authoritative;
  the filename is a last resort only.
- **Speed** (release, medians): 64–256 px images **0.02–0.6 ms**; 512 px **1.5–3 ms**; 1024 px
  **3–6 ms**; 2048×2048 **~3.3 ms** (~6.5 ms if the alpha pass also runs); average across all 23
  files **~3.3 ms**.
- The constant-time optimization: 2×2 subsampled block averaging capped read cost — 2K went
  **~9 ms → ~3.3 ms**, and a 16K sheet (64× the pixels of 2K, naïvely **~0.5 s**) now costs about
  the same as 2K. Verified byte-for-byte identical detection across the corpus.
- Ordering requirement (user-driven, for 8K/16K images): the image displays first, detection posts
  a second message later — so none of this time touches time-to-first-pixel.

→ `262a779`, `95a258b`, `227a95b`, `2b6f2fa`, `2979df6`

### 9. Image outline rendering bug *(Jul 12)*
Outline missing on some sides until zooming; fixed.
→ `ef1b376`

### 10. Flipbook toolbar usability *(Jul 12)*
"Are these not native controls?" — scrubber too thin to grab, toolbar flickered while dragging,
checkbox looked broken, clicking "view as flipbook" stole keyboard focus, scrubbing should pause
playback. This dissatisfaction with the hand-rolled controls directly set up the ImGui migration.
→ `899858e`

---

## Phase 4: The Dear ImGui migration (July 12) — *the centerpiece*

### 11. Settings window → full UI rewrite in ImGui *(the 33 MB transcript, two context compactions)*
Full writeup in **Appendix B** below. The arc:

- Started as "plan a proper tabbed settings window" (General / Flipbook / Keybinds / Context menu),
  built with hand-rolled Win32/GDI controls first → non-native look, spacing off. After an honest
  pros/cons of undocumented dark-mode APIs vs. a lightweight UI framework, and one incremental fix
  round (metrics + system accent color + real EDIT controls), the trigger was pulled: *"I'd rather
  go for imgui at this point… plan for a full replacement of all UI."*
- What was replaced: **~4,700 lines** of hand-rolled GDI UI (`settings/mod.rs` 1,879; `chrome.rs`
  1,211; `transport.rs` 979; `hint_chip.rs` 381; `tooltip.rs` 267), 2 HWNDs + 2 window procedures,
  3 undocumented uxtheme ordinals (133/135/136), 1 nested message pump.
- Framework comparison quoted during the decision: ImGui **+2–4 MB** exe, can share the D3D11
  device; WebView2 rejected for a **100–300 ms** cold start and a browser process.

**Phase-0 gate spike** (budget: ≤15 ms TTFP hit, idle must be 0%):

- Baseline TTFP median **143.7 ms** (range 140–153, n=12).
- ImGui eager-init on a 38 KB image (the deliberately unfair case — decode is instant, nothing to
  hide behind): **+4.7 ms**. On an 8.9 MB image: **168.5 vs 168.7 ms — +0**, the cost fully hidden
  behind off-thread decode (chrome ready at 145.8 ms, before the image at 168.5 ms).
- Idle **0.00%** of a core (the baseline's 0.16% was the file watcher).
- **The font-atlas question answered with data**: the entire font path (load 1 MB Segoe UI TTF
  **0.42 ms** + lazy glyph baking + texture upload) ≈ **1.4 ms**, paid on the *first frame*, not
  startup — ImGui 1.92's atlas is lazy. An offline atlas cache would save ~1 ms, and **60% of
  ImGui's init cost is D3D11 device-object creation**, which no font cache touches. Idea correctly
  killed by measurement. A deferred-init mitigation also worked (+0 ms, chrome 6 ms later) but was
  unnecessary — kept in the back pocket, not built.
- Exe: **10.11 → 11.48 MB (+1.34 MB)**; clean builds ~**10 s** slower for the C++.

**Phases 1–5** (single-window collapse; toolbar/status bar/transport/chip/tooltips; settings modal;
popup menus + deleting the uxtheme ordinals; polish and layout):

- Phase 1 verification: TTFP **146.5 ms** (+2.8, unfair case) and **169.0 ms** (+0.3, noise); idle
  **0.00% across 3 trials** — a scary 3.59% reading was real mouse input doing real work, and an
  earlier 0.94% was startup tail in a too-short sample window.
- Test/code health throughout: **61 tests passing** (12 GDI hit-test tests deleted, 2 icon-atlas
  tests added; the settings phase briefly showed 70), clippy clean.
- ImGui bugs found by driving the real app: Escape doesn't close modals by default;
  `WantCaptureKeyboard` vs `WantTextInput`; open popups aren't modal (Escape fell through and
  closed the *window*); `InputInt` steppers ate the field width (`step(0)`); tabs filled the
  *unselected* state; `✕` tofu → `×` too tiny → plain capital `X`; a slider mis-sized by reading
  `cursor_pos_x()` before `same_line()`.
- **The best bug — `SV_Position` is render-target space**: every image opened exactly `toolbar_h`
  too high (measured image center y≈**492** vs region center **550** — off by **58 px**, toolbar_h
  at 144 DPI = **57 px**). The old child window had made viewport-relative coordinates true by
  construction; the single-window collapse silently invalidated the assumption. Fix: one shader
  subtraction. Now a documented invariant, because the symptom points at the wrong file.
- **The idle scare**: 0.20% with settings open vs 0.00% closed → instrumented `WM_PAINT`:
  **zero paints across 6 idle seconds**. The 15.6 ms sample was exactly one Windows scheduler
  quantum (**15.625 ms**) — measuring the ruler, not the thing. The caret-blink timer arms only
  while `WantTextInput` and dies with focus.
- Small gems: stock ImGui `WindowBg` is **94% opaque** (the empty-state text bled through the
  settings window); the modal dim measured **179→188** grey — ImGui's stock `ModalWindowDimBg`
  (0.8 grey at 35%) *lightens* a dark app (foreshadowing session 15); a playing flipbook idles at
  **1.17%** driving its 60 Hz timer, by design.

**The final A/B benchmark** (interleaved runs in throwaway git worktrees, A/B order flipped every
iteration, n=20 per cell, `GetProcessTimes` process-creation → `Present` timestamps at the single
shared call site):

| Build | Image | Median | Mean | SD | Δ vs GDI |
|---|---|---|---|---|---|
| GDI | 38 KB | 143.8 ms | 143.8 | 1.5 | — |
| **ImGui** | 38 KB | **141.1 ms** | 141.3 | 1.6 | **−2.7 ms** |
| GDI | 8.9 MB | 170.4 ms | 170.2 | 1.8 | — |
| **ImGui** | 8.9 MB | **164.4 ms** | 165.0 | 2.0 | **−6.0 ms** |

Faster on both images, by several standard errors, despite a **1.4 MB bigger exe
(10.35 → 11.77 MB)** — proof the UI is off the critical path (decode dominates) and the win came
from deleting the second window + GDI startup paint, not from ImGui being fast. Interleaving
mattered because the ~1.5 ms spread was bigger than the effect under test. The GDI numbers
reproduced the older baseline almost exactly (143.8 vs 143.7).

Net diff: **26 files, +3,641 / −5,750** (≈ −2,100 lines); UI code **4,700 → 2,570 lines**;
settings window **1,879 → 672 lines**; transport band **979 → 35 lines** of pure data; undocumented
APIs **3 → 0**; nested message pumps **1 → 0**; windows **2 → 1**.

→ `cfcad10`, `d8a30e4` (PR #2)

---

## Phase 5: Post-migration polish (July 13)

### 12. Flipbook playback smoothness *(two bugs, one session)*
- Playback stopped on mouse movement — A/B'd with before/after executables and a probe harness
  moving the mouse at **~500 Hz** over the window.
- The inverse: transport jittered *without* mouse movement on a **120 Hz** panel. Root cause:
  vblank is **8.3 ms** but `SetTimer` resolution is **~15.6 ms** — a timer cannot feed the pump
  faster than ~64 Hz. The timer was the wrong pacer; playback became vsync-driven. Position stays
  time-derived, so a 24 fps sheet plays at 24 fps whether sampled 60 or 240 times a second — holds
  at 144/240 Hz. 61 tests passing.

→ `721015e`, `f038598`

### 13. Codebase quality audit + decode-bomb hardening
Audit written to [code-quality-audit-2026-07.md](code-quality-audit-2026-07.md), then executed:

- The per-axis decode caps had a hole: GIF dimensions are `u16`, so a 65535×65535 GIF
  (**17 GB per frame**) sailed under the **131072** per-axis cap. The guard now checks the
  *product* against a **4 GiB** budget — the same hole existed in PNG/HDR/EXR. (The audit's own
  prescription had been wrong; the fix corrected it.)
- PSD had no size guard at all: a **40-byte** file claiming 60000×60000 made psd_sdk try to
  allocate **~10 GB** and abort before Rust saw a dimension — now guarded in C++ at the allocation
  site.
- A zune JPEG header probe was added and measured: **13.6 µs vs 129 ms** full decode on a 16 MP
  (4928×3264) JPEG = **0.011% overhead** (7.5 µs / 37 ms = 0.020% on 2500×1667). TTFP untouched;
  the measured number was put in the code comment.
- Verified with a real bomb: a **197 KB** progressive JPEG with its SOF patched to declare
  65535×65535 (~17 GiB of RGBA) is refused by the shipped release exe.

→ `b222fad`

### 14. Theme system + hot reload
All ImGui styling moved into one commented `theme.toml` with debug-build hot reload
(`hotstyle.rs`); release builds embed it via `include_str!` with a validity test so a broken
stylesheet can't ship. Then the **checkbox-size saga**: control sizes exposed per-widget, but the
property seemed to do nothing — pushing a physical px font size gets DPI-scaled a *second* time, so
a **16 px box came out 36 px at 150%**. Also: settings vs. transport checkboxes rendered
differently, vertical alignment fixes, and click/drag on the transport bar now pauses playback.
→ `6ff008b`, `827c148`, `351ec77`

### 15. Modal overlay dimming
ImGui's stock `ModalWindowDimBg` (0.8 grey at 35%) *brightens* a dark app — measured **179→188**
in session 11 and initially left as stock behavior. Now made to actually darken; took three
screenshot rounds to nail.
→ `1df09d9`

---

## Phase 6: Docs & new features (July 16–18)

### 16. Documentation refresh + architecture Q&A *(Jul 16–17)*
Updated `architecture.md`, `README.md`, and `CLAUDE.md` to current state; plain-English explainers
(DXGI flip-model swapchain, DXBC, the **128-byte** constant buffer; whether Direct2D could replace
D3D11). A genuine contradiction was caught in the docs: "**≈392 ms** for an 8192×4096 PNG via
zune" vs. the benchmarked "image crate ~1.8× faster." Resolution: the 392 ms figure was a stale
pre-switch number describing routing that no longer exists; the measured **~340 → ~190 ms**
(zune → image crate) figures are the valid ones.

### 17. Octagon overlay mode *(Jul 17)*
Unity VFX Graph-style octagon overlay for artists to visualize texture clipping:

- Crop factor 0→**0.5**, linear vertex interpolation; geometry fixed so all eight sides stay equal
  length like Unity's wireframe.
- Line-opacity slider (0–1, default 1), **3-decimal** number formatting, numeric-only input
  sanitization, "Octagon Overlay" naming, corner radii unified with the main settings window.
- TTFP impact review: one more **64×64 A8** icon-atlas master (**~4 KB** in `.rodata`) and one
  extra cell in the one-time atlas downsample — microseconds, never per-frame. The
  shader-vs-ImGui-lines question resolved in favor of ImGui lines (a handful of vertices, not
  worth a shader path).

→ `196f2bd`, `dee0c36`

### 18. This retrospective *(Jul 18)*
The session that produced this document.

---

## The recurring numbers that make the narrative

- **~140 ms** — the number the app lives and dies by (time-to-first-pixel), the yardstick every
  decision was measured against.
- **2× / 1.8× / ~0** — the HDR, PNG, JPEG decoder verdicts: benchmark each format, switch only
  where the data says so.
- **−2.7 / −6.0 ms** — the UI rewrite that was supposed to cost startup time and instead bought
  some back.
- **0.00%** — idle CPU, defended three separate times (the 0.20% scheduler-quantum scare, the
  3.59% mouse scare, the 0.94% sample-window scare) and never actually lost.
- **14/16, 0 false positives, ~3.3 ms** — the flipbook detector's final report card.

## Themes worth pulling out for the blog

- **Performance as a first-class requirement**: every big decision (decoder choice, ImGui
  adoption, flipbook detection ordering, octagon overlay) was gated on measurement — the TTFP A/B
  benchmark before committing to the UI rewrite is the standout story.
- **Three rewrites of the render/UI stack** (wgpu → CPU → D3D11; GDI → ImGui), each justified
  empirically.
- **Measure the ruler before you believe the measurement**: the scheduler-quantum idle scare, the
  58-px `SV_Position` offset that looked like a centering bug, the screenshot captures that kept
  framing the wrong window.
- **Borrow from other fields**: the flipbook detector's core breakthrough is an audio
  pitch-detection algorithm (YIN).
- **Reliability is mostly about *not* firing**: half the detection work was guards that keep the
  detector quiet on non-flipbooks.

---
---

# Appendix A — Teaching an Image Viewer to Recognize Sprite Sheets

*How we built automatic flipbook-grid detection for a native image viewer — the dead ends, the
insights, and the algorithm that finally worked.*

---

## What we were trying to do

**Fire** is a fast, native Windows image viewer. One of its modes is a *flipbook viewer*: a lot of
game and VFX art ships as a **sprite sheet** — a single image that's actually a grid of animation
frames laid out left-to-right, top-to-bottom. Play the cells in sequence and you get an explosion,
a puff of smoke, a running character.

To play a sprite sheet as an animation, the viewer needs to know one thing: **how many columns and
rows is the grid?** An `8×8` sheet is 64 frames; a `5×5` sheet is 25. Get that number wrong and you
get garbage — frames sliced in half, playing at the wrong speed.

The dream is that you double-click a sheet and it *just knows*. No dialog, no manual entry. So the
job was: **given the raw pixels of an image, automatically figure out the grid.**

This turned out to be a surprisingly deep little problem, and the path to a good solution went
through several wrong turns. This is the story of that path.

---

## Why it's hard

A few things make grid detection tricky:

- **The grid can be anything.** `2×2`, `8×8`, `5×5`, `6×6`, even a single row like `4×1`.
- **Real sheets aren't tidy.** Game engines love power-of-two canvases (2048×2048) but the artist
  might pack a `5×5` grid into it. 2048 ÷ 5 = 409.6 — the cells don't land on whole pixels.
- **Frames vary wildly.** In a fire animation, frame 0 is a tiny spark and frame 63 is a faint
  cloud of smoke. They barely look alike.
- **Lots of images aren't sprite sheets at all.** A single character portrait, a tiling texture, a
  photo. The detector has to say "no grid here" for those, or it'll pester you with wrong guesses.

So the detector needs to be right on real sheets, *and* stay quiet on everything else. That second
requirement — not crying wolf — is where most of the difficulty lives.

---

## The starting point: a filename hint plus a correlation score

The first version of the detector used two ideas:

1. **Filename tokens as a hint.** Artists often name files like `T_fx_FireFar_8x8_FB.png`. That
   `8x8` is a strong clue. The code parsed it as a *prior*.
2. **Content analysis to confirm it.** It scanned candidate grids (divisors of the image size),
   scored each with a mix of "shift-correlation" (does the image look like itself when slid over by
   one cell?) and "boundary-anomaly" (are there regular seams?), and picked a winner.

On paper this is reasonable. In practice it fell apart the moment we tested it on real files.

### Building a test bench

The user handed over a folder of real game-VFX sheets — the ground truth. The flipbooks were named
with a `_NxM_FB` convention (like `_8x8_FB`), and the non-flipbook textures had ordinary names.

Rather than eyeball things, we wrote a small **diagnostic harness**: run every file through the
real detector and print, side by side, the filename's grid, the content detector's grid, and the
final answer. This turned out to be the single most valuable tool in the whole effort. Every
decision afterward was backed by a table of real results instead of a hunch.

### What the test bench revealed

The old detector was **confidently wrong**:

| File | True grid | Content guessed |
|------|-----------|-----------------|
| Zombie_Run (×3) | 5×5 | 2×2 |
| Dust_Cloud | 6×6 | 2×4 |
| Portal / Waterfall / Wisp | 8×8 | 16×2 |

Two separate failure modes were tangled together:

1. **Non-divisible sizes were invisible.** A `5×5` grid on a 2048-pixel canvas was *never even a
   candidate*, because 2048 isn't divisible by 5. The whole "scan the divisors" approach
   structurally couldn't see the most common real-world layout.

2. **Even when the grid *was* a divisor, the score ranked it wrong.** On an `8×8` sheet, the score
   preferred degenerate grids like `16×2`. The root cause: the "shift-correlation" signal is really
   just measuring *how smooth the image is* — smaller shifts always correlate more — so it drifted
   toward fine or lopsided grids, not the true one.

And the worst part: because content was confidently wrong, it was **overriding correct filename
tokens**. A file literally named `8x8` was being displayed as `16×2`.

---

## Wrong turn #1: "just trust the filename"

The quickest fix was to flip the trust relationship: **make the filename token authoritative**, and
only let content analysis *refine* it (e.g. an artist labels a sheet `4x4` when it's really a finer
`8x8` — a clean multiple). Content could no longer replace a token with an unrelated grid.

This immediately fixed all 16 named files. But it wasn't a real solution, and the user rightly
pushed back with two objections that shaped the rest of the work:

- **Filenames can be wrong or missing.** Trusting them blindly is fragile.
- **The content detector should actually work** — "can we detect this better in some other visual
  way?"

Fair. So we went back to the pixels — but this time, we *looked* at them.

---

## The turning point: actually looking at the images

Instead of reasoning about the algorithm in the abstract, we generated thumbnails of the real sheets
and studied them. This changed everything.

**Real flipbooks share a visual signature:** a *regular grid of content blobs separated by empty
gutters*. In a fire sheet, each cell has a flame/smoke blob roughly centered, with blank margins
around it. In a running-character sheet, each cell has the character with white space between.

**Non-flipbooks don't have this:**

- A **bat-wing texture** is one continuous object — no repeating cells.
- A **caustic/water texture** is uniform noise everywhere — no gutters.
- A **single spark sprite** is one soft blob filling the frame — nothing repeats.

That's the insight the whole solution is built on. The thing to detect isn't "self-similarity" or
"seams" — it's **periodic gutters**. If you collapse the image into a 1-D profile of "how much
content activity is in each column" (and each row), a real sprite sheet's profile is *periodic*:
high over cells, low over gutters. A single object is one broad bump; a uniform texture is flat.

So the problem reduces to a classic one: **find the period of a 1-D signal.**

---

## Wrong turn #2: plain autocorrelation

The textbook way to find a period is autocorrelation: slide the signal over itself and look for the
lag where it lines up. We tried it. Better than before, but two problems surfaced.

**Problem A — the tiling guard killed real flipbooks.** To avoid mistaking a repeating *texture*
(like a checkerboard) for an animation, we compared neighboring cells: if they were nearly
identical, reject it as tiling. But it turns out that in a smooth smoke animation, consecutive
frames *are* nearly identical too (at least when you downscale them for comparison). The guard
couldn't tell a checkerboard from a cloud of smoke, so it threw out real sheets.

**Problem B — the period was contaminated by the animation itself.** On the `FireFar` 8×8 sheet,
autocorrelation reported **5×5**. Why? Because the blobs *grow* across the sheet — small at the
start, large at the end. That slowly-changing brightness envelope dominates the autocorrelation and
drags it away from the true cell size. The animation's own evolution was corrupting the measurement.

---

## The breakthrough: YIN period detection

Both problems have a known answer in a different field — **pitch detection in audio**. Finding the
fundamental frequency of a musical note is *exactly* this problem: find the period of a signal whose
amplitude drifts over time. The go-to algorithm is **YIN** (a cumulative-mean-normalized difference
function).

Instead of asking "how well does the signal correlate with itself at lag τ?", YIN asks "how
*different* is the signal from itself at lag τ?" — and then normalizes that by a running average.
That normalization is the magic: it cancels out the slow amplitude envelope (the growing blobs) and
leaves only the true periodicity behind.

We swapped autocorrelation for YIN, applied to the column and row activity profiles. The
contamination vanished — `FireFar` stopped reporting 5×5. And crucially, YIN handles **fractional
cell sizes for free**: it finds the true period even when it's 409.6 pixels, so non-power-of-two
grids on power-of-two canvases finally worked.

### Replacing the tiling guard

We still needed to reject tilings, but "neighbors look identical" didn't work. The fix came from
measuring similarity at a *distance*: compare each cell to the one **half a loop away**. A real
animation has evolved by then (frame 0 vs frame 32 are different), so this "far" similarity drops
below 1. A perfect tiling, on the other hand, is identical no matter how far apart you look — its
far-similarity sits at exactly 1.0. So the guard became: *reject only if far-similarity ≥ 0.999*
(a near-perfect repeat), which lets real smoke sheets (measured up to 0.9955) through while catching
true tilings.

### A clean threshold, by luck of the data

When we plotted YIN's "confidence" across every file, the result was beautifully bimodal:

- **Real grids** clustered at very deep, confident dips (below 0.15).
- **Textures and full-bleed non-grids** sat above 0.55.
- **Nothing** fell in between.

So we set the acceptance threshold right in the empty gap (0.35), giving a wide safety margin on
both sides. An earlier attempt had the threshold at 0.55 — sitting *right* on top of a bat-wing's
score (0.554 vs 0.55, a razor's edge). Moving it into the gap made the whole thing robust.

**Result at this stage:** every one of the 23 test files was classified correctly. Content alone
(ignoring filenames entirely) now correctly detected 11 of the 16 real sheets, and — just as
important — produced **zero false positives** on the 7 non-flipbook textures.

---

## The alpha-channel insight

A few full-bleed sheets still couldn't be detected from content: `FireFar` and `WispySteam`. They
relied on their filename token.

Then the user pointed out something we'd completely missed: **look at the alpha channel.**

> "WispySteam specifically has data only in the alpha channel — the RGB channels are solid white."

That was the whole problem. Our detector only looked at brightness (RGB luminance). For a sheet
whose RGB is a flat white field and whose actual shape lives in *transparency*, the brightness
signal is a featureless blank — no gutters, nothing to find. And `FireFar`'s alpha edges are
crisper than its smoky RGB, too.

### First attempt: premultiplied luma

The obvious move was to fold alpha into the signal by multiplying: `brightness × alpha`. For an
opaque image this changes nothing (alpha = 1); for a transparent one it reveals the shape.

It fixed `WispySteam`... but *regressed* `Ground_Shockwave`, whose shockwave reads cleaner in plain
RGB than in the muddied premultiplied signal. One step forward, one step back.

### The fix: a luma→alpha cascade

The user's phrasing was the clue: try alpha *in addition to* brightness, not instead of it. So the
detector now runs **two passes**:

1. Try the **luminance** channel (the usual case).
2. If that finds nothing, fall back to the **alpha** channel (skipped entirely for fully-opaque
   images, since their alpha carries no information).

Using *pure* alpha for the fallback (not premultiplied) mattered: it recovered `FireFar` too,
exactly as the user predicted — its alpha is clean even though its RGB isn't. Each channel
independently rejects non-grids, so the fallback widens what we can find *without* adding false
positives.

**Result:** content detection climbed to 13 of 16, including both previously-stubborn sheets, still
with zero false positives.

---

## Making content the authority (not the filename)

With a genuinely good content detector in hand, it was time to fix the trust relationship properly.
The user's requirement was explicit: **the pixels should decide the grid, because filenames can be
wrong or missing.** A wrong token must never override what the image clearly shows.

So the logic was flipped:

- **Content decides.** Whenever the pixels yield a grid, that is the answer.
- **The filename is a last resort only.** It's consulted *only* when content comes up empty — never
  to override a content detection.

On the test set this was safe because the content detector never *disagrees* with a correct token —
it either agrees or stays silent. A file mischievously named `boom_4x4.png` but containing a true
`8×8` sheet now correctly detects `8×8` straight from the pixels.

---

## The last mile: single-row and single-column strips

Some sheets are a single row or column — a `4×1` strip of flames, a `3×1` strip of plants. These
have a period on *one* axis (the frames) and none on the other (each frame spans the full height).
The detector originally required *both* axes to have a period, so strips slipped through.

Relaxing that — "one axis has a period, the other doesn't → it's a strip" — immediately recovered
`Araucaria 3×1`. But it also introduced a **false positive**: the bat-wing texture got read as a
`1×3` vertical strip, because the wing's membrane has roughly three horizontal bands that look
periodic.

### The strip adjacency guard

The fix leaned on what makes an *animation* an animation: **consecutive frames resemble each
other.** We measured the similarity of neighboring cells:

| File | Read as | Adjacent-frame similarity | Verdict |
|------|---------|---------------------------|---------|
| Bat_Wings (false) | 1×3 | **0.25** — bands don't match | reject |
| Araucaria (real) | 3×1 | **0.59** — plant frames match | keep |

A segmented object's "frames" are just different parts of one thing; a real strip's frames are
variations on a theme. So strips get one extra requirement — adjacent similarity above ~0.45 — that
2-D grids (which are corroborated by *two* axes of evidence) don't need. That one guard cleanly kept
the real strip and rejected the bat wing.

Two sheets still can't be seen from content at all: `flares 4×1` (the flames blend together with no
gutters) and a tiny `2×3` sheet (too small and weak). Those fall back to the filename token — the
one place a token is still used, and only when content is completely silent.

---

## Where it landed

The final detector, validated against all 23 real files:

- **Content determines the grid** — the pixels decide, a wrong filename can't corrupt it.
- **14 of 16** flipbooks are detected from content alone, including full-bleed fire/smoke (via the
  alpha channel) and a single-row strip.
- **2 of 16** genuinely can't be seen from content and fall back to the filename token as a last
  resort.
- **0 false positives** across the 7 non-flipbook textures, and **0** cases where content and a
  correct token disagree.

### The pipeline, end to end

1. Shrink the image to a small grayscale analysis copy.
2. Build a per-column and per-row "content activity" profile.
3. Run **YIN period detection** on each axis to find the cell size (handles fractional cells and
   amplitude drift).
4. If only one axis has a period, treat it as a **strip**.
5. Reject **flat profiles** (uniform textures), **near-perfect tilings** (far-similarity ≈ 1),
   **over-split** grids (mostly-empty cells), and **strips whose frames don't resemble each other**.
6. If the brightness channel finds nothing, retry on the **alpha channel**.
7. If content is still silent, fall back to a `NxM` **filename token**.

---

## How fast is it, and when does it run?

A detector that's accurate but slow would hurt the whole point of the viewer, which is optimized for
*time-to-first-pixel*. So two practical questions matter: **when** does detection happen relative to
displaying the image, and **how long** does it take?

### When it runs: after the image is already on screen

Fire decodes images on a pool of background worker threads, never on the UI thread. Detection lives
on that same background job — but it is deliberately kept **off the path to first pixel**. The
sequence for opening one image is:

1. The window opens **immediately** with a placeholder — the UI thread is never blocked.
2. A background worker **decodes** the image and **posts it straight to the UI** for display.
3. Only *then*, on that same worker, does it **run flipbook detection** on the decoded pixels.
4. It posts the detection result back to the UI as a **separate, second message**.
5. The UI shows the image first; the "flipbook detected" hint pops up a beat later.

The image and the hint are two independent messages, and detection sits between them. So the answer
to "display first, or analyze first?" is: **the image displays first, and analysis follows.** Any
detection time is completely hidden — the image is already up, and the hint simply appears whenever
the analysis finishes.

This wasn't the original design. At first, detection was bundled *with* decode into one message, so
it sat on the critical path and delayed the image. That was fine at a few milliseconds for normal
images, but before the scan was made constant-time (see below), an 8K/16K sheet's ~half-second
analysis would have added a very visible stall before the picture appeared. The fix was to split it:
the decoded pixels are shared (cheaply, by reference) between the display path and the detection
pass, so the image can go to the screen while the worker analyzes the same pixels in the background.
A stale-guard makes sure that if you navigate away before detection finishes, the late-arriving hint
for the old image is simply discarded. With the scan now constant-time as well, the two changes are
belt-and-suspenders: detection is both fast *and* off the critical path.

Because Fire uses several worker threads, detection of *different* images (say, when you arrow
through a folder) can also overlap across threads.

### How long it takes

Measured on the real test folder (release build, times are the median of repeated runs):

| Image size | Detection time |
|------------|----------------|
| 64–256 px  | 0.02 – 0.6 ms |
| 512 px     | 1.5 – 3 ms |
| 1024 px    | 3 – 6 ms |
| 2048×2048  | ~3.3 ms, or ~6.5 ms if it also runs the alpha pass |

Across all 23 files, detection averaged **~3.3 ms**. And because it runs *after* the image is
displayed, none of that time is felt as lag — it only governs how soon the hint appears.

The key to those numbers is a small optimization that came out of profiling. The dominant cost was
never the algorithm (YIN and the guards run on a tiny ≤512 px copy and are essentially free); it was
*building* that copy — reading every source pixel to shrink the image down. So a naïve version's
cost scaled with the full pixel count, which is fine at 2K but turns into ~0.5 second at 16K.

The fix: when shrinking a large image, don't average *every* pixel in each block — average a small,
fixed grid of samples from it (a 2×2 subsample). The block's average is essentially the same, but
the number of reads is now capped regardless of source size. That makes detection **roughly
constant-time**: a 16K sheet costs about the same as a 2K one, and 2K itself roughly halved (~9 ms →
~3.3 ms). Cell gutters are far wider than a sampling block, so they're never missed — verified by
re-running the whole test corpus and confirming detection was byte-for-byte unchanged.

Two other things shape the numbers:

- **The alpha fallback can roughly double the cost.** If the brightness pass finds a grid, that's
  one scan; if it comes up empty and the alpha pass runs, that's a second. `FireFar` (needs alpha)
  and the non-flipbook `Trail_Shred` (scans both and still finds nothing) sit at the higher ~6.5 ms.
- **Small sheets are effectively free** — well under a millisecond.

### Why the ordering matters

For normal images detection is only a few milliseconds, so displaying the image first versus
bundling detection with it makes no perceptible difference. The reason the split matters is the
extreme case: an 8K or 16K sheet has **64 times** the pixels of a 2048×2048 one, which turns a ~10 ms
scan into hundreds of milliseconds. If detection sat on the critical path, opening a giant sheet
would visibly stall before the picture appeared — exactly the kind of hitch a viewer built around
time-to-first-pixel exists to avoid. By putting the image on screen first and letting the hint catch
up, the app stays instant no matter how large the sheet is; the only thing that grows with image
size is how long you wait for a small "flipbook detected" chip, which nobody is watching for.

## Lessons worth keeping

- **Test against real data, early, with a harness.** Every good decision here came from a table of
  real results. Synthetic test images (moving dots, clean grids) had quietly hidden every one of
  the failure modes that real game art exposed.

- **Look at the actual pixels.** The single biggest leap came not from a cleverer formula but from
  generating thumbnails and *seeing* that the signal was "periodic gutters." You can't design a
  detector for a thing you haven't looked at.

- **Borrow from other fields.** The core breakthrough — YIN — is a pitch-detection algorithm from
  audio. "Find the period of a signal whose amplitude drifts" is the same problem whether the signal
  is a musical note or a row of pixels.

- **Reliability is mostly about *not* firing.** Half the work was guards: reject tilings, reject
  single objects, reject over-splits, reject dissimilar-band "strips." A detector that finds every
  real sheet is easy; one that also stays quiet on everything else is the hard and valuable part.

- **Trust the data source that's actually reliable.** We went from "trust the filename" to "trust
  the pixels, use the filename only as a last resort" — because in the real world, the pixels don't
  lie and filenames sometimes do.

---
---

# Appendix B — Deleting 2,000 Lines of GDI and Getting *Faster*

*How we replaced a hand-painted Win32 UI with Dear ImGui in a performance-obsessed image viewer —
the invariants we were terrified of breaking, the bugs we found, and the benchmark that surprised us.*

---

## The app, and the one number that matters

**Fire** is a native Windows image viewer with a single design goal: *time-to-first-pixel*. You
double-click a PNG in Explorer and the picture should be **on screen**. Not a splash screen, not a
process warming up — the image.

There is no resident background process. The thing Explorer launches *is* the whole app: it decodes
off-thread, uploads once to a D3D11 texture, and presents through a DXGI flip-model swapchain. Every
architectural decision gets weighed against that one number, and the number is around **140 ms** —
most of which is the decoder doing honest work.

So when we decided to throw away the entire UI layer and replace it with a third-party immediate-mode
library, the question wasn't "will it look nicer." It was: **what does this cost us at startup, and
what does it cost us at idle?**

The answer turned out to be *negative*. It made the app faster. This is the story of how, and why.

---

## What we were replacing

Fire's UI was hand-painted GDI, all of it. Not "we used a few common controls" — we mean every pixel
of the toolbar, the status bar, the flipbook transport, the tooltips, and a full settings dialog were
`FillRect` and `DrawTextW` calls, with hand-rolled hit-testing and hover state behind them.

| Module | Lines | What it was |
|---|---|---|
| `settings/mod.rs` | 1,879 | A custom-painted modal dialog: tabs, checkboxes, dropdowns, a key-capture field, a tree editor |
| `chrome.rs` | 1,211 | Toolbar + status bar painting, hit-test, hover, theming |
| `transport.rs` | 979 | The flipbook band: a slider, numeric fields, a checkbox — with their own drag and edit state machines |
| `hint_chip.rs` | 381 | One floating chip with two buttons |
| `tooltip.rs` | 267 | Tooltips |

That's roughly **4,700 lines of UI**, and essentially all of it was reimplementing widgets that have
been solved since 1995. The `transport.rs` doc comment now says the quiet part out loud:

> …a hand-rolled layout, hit-test, hover/drag state machine, and a typed-number-field editor, all
> painted with GDI — is gone: sliders, numeric fields and checkboxes are solved widgets, and
> reimplementing them is exactly the kind of work that produced the bugs this migration exists to kill.

### The three things that actually hurt

Line count is the boring complaint. Three structural problems were worse.

**1. We were calling undocumented Windows APIs.** Right-click menus used `TrackPopupMenu`, which is
*system-drawn*. There is no documented way to make one dark. The only way is three `uxtheme.dll`
functions that ship with no names — you resolve them by **ordinal** (133, 135, 136), `transmute` the
function pointers, and hope. This is exactly the kind of thing that breaks on a Windows update and
gets your app blamed.

**2. A modal dialog meant a nested message pump.** The settings dialog ran its own `GetMessageW`
loop. That loop re-enters the window procedure — which means the dialog could not hold a `&mut App`
across its lifetime without aliasing it. We'd structured around this with `PostMessage` and boxed
payloads, but the hazard was permanent, load-bearing, and one refactor away from a soundness bug.

**3. Two windows, because GDI can't paint on a flip-model swapchain.** This is the real one. You
*cannot* mix GDI painting with a flip-model swapchain, so the app had a parent window (GDI chrome)
and a child window (the D3D viewport). Two HWNDs, two window procedures, `WS_CLIPCHILDREN`, and a
retained layout that had to be invalidated in the right order. Remember this one — it's why the
performance result comes out the way it does.

---

## The two invariants we refused to break

Dear ImGui's natural mode is a **game loop**: rebuild the entire UI, every frame, forever, at
whatever your monitor refreshes at. Fire's entire personality is the opposite of that.

So the migration had two hard rules, and both of them are performance rules.

### Invariant 1: an idle window must cost ~0

Fire draws a frame **only when something happens**. There is no free-running timer, no unconditional
repaint. The mechanism is a countdown: `App::request_frames(n)` asks for the *one or two* extra
frames ImGui genuinely needs to settle a hover or a click, and the count **terminates**. At zero,
`WM_PAINT` stops requesting itself.

The measured idle cost was **0.00% of a core**, and it had to stay there.

### Invariant 2: upload once, redraw is one draw

The decoded image becomes a D3D11 texture with a hardware mip chain exactly *once*, when it's
adopted. Pan, zoom, exposure, channel isolation, tonemap, and flipbook cell selection are all
**constant-buffer values** — a single 128-byte struct. Every frame is one fullscreen-triangle draw.

No per-pixel CPU work. No per-frame texture re-uploads. An immediate-mode UI must not sneak either
one back in.

---

## Doing it

We used [`dear-imgui-rs`](https://crates.io/crates/dear-imgui-rs) with the Win32 and DX11 backend
shims, which compile **ocornut's own** `imgui_impl_win32.cpp` and `imgui_impl_dx11.cpp`. We own zero
backend code, which was a deliberate call: the backends are the part most likely to be subtly wrong,
and upstream's are battle-tested by thousands of applications.

The migration ran in five phases, each one shippable:

0. Get ImGui compiling and drawing a triangle over the viewport.
1. **The single-window collapse** — delete the child window. One HWND, one swapchain, and the image
   drawn into a *sub-rect* of it, with the chrome over the rest.
2. Toolbar, status bar, transport, hint chip, tooltips.
3. The settings dialog.
4. The popup menus (and with them, the uxtheme ordinals).
5. Polish, layout, colours — and the benchmark.

Phase 1 is where the performance story is hiding, so hold that thought.

---

## The bugs, briefly

Every one of these was found by *driving the actual app* — launching it, capturing the screen with a
DPI-aware `BitBlt`, synthesizing clicks, and sampling pixels — rather than by reading the code and
believing it.

- **ImGui does not close a modal on Escape.** We assumed it did. It doesn't. You bind that yourself.
- **`io.WantCaptureKeyboard` is `true` for the entire time *any* modal is open.** It literally cannot
  mean "a text box has focus." The flag you want is `io.WantTextInput`.
- **An open ImGui popup is not modal** — so `WantCaptureKeyboard` stays *false*, and our Escape
  handler fell straight through to the viewer and closed **the window** out from under the open menu.
- **`InputInt` ships as a stepper.** Those `−`/`+` buttons ate the entire width of the transport's
  numeric fields and left the value nowhere to render. `step(0)`, `step_fast(0)`.
- **ImGui fills the *unselected* tabs** and leaves the selected one blending into the page — which
  reads exactly like "this tab is disabled and those are buttons." We inverted it.
- **Tofu.** `✕` (U+2715) is not in Segoe UI, and ImGui does not fall back to another font. `×`
  (U+00D7) *is* in the font, but it's a **multiplication sign** — drawn tiny, it reads as a speck of
  dust on the screen. The close button is a plain capital `X`.
- **A slider that painted its frame underneath the fields next to it.** The cause was reading
  `cursor_pos_x()` *before* calling `same_line()`, so it returned the start of a fresh row and the
  slider was sized as if the group in front of it didn't exist. Found by sampling pixels, not by
  staring.

And one that isn't an ImGui bug at all, but is the best bug of the migration.

---

## The best bug: `SV_Position` is a lie (sort of)

Late in the project, a report: *"when a new image opens it is positioned vertically off centre, a bit
high, so the top is clipped behind the toolbar."*

The obvious suspect is the fit/centring math in `render/view.rs`. That code is pure, unit-tested, and
— it turns out — completely innocent.

Here's what actually happened. The pixel shader receives `SV_Position`, and `SV_Position` is in
**render-target space**, not viewport space. D3D applies the viewport transform *before* the fragment
stage. So even though `RSSetViewports` correctly scopes the image draw to its sub-rect below the
toolbar, the shader still gets handed **absolute client coordinates** — while it was centring the
image on `surf_size * 0.5`, the viewport's centre measured from the *client's* origin.

Every image opened exactly `toolbar_h` pixels too high, and the viewport dutifully clipped the
overhang. We confirmed it by measuring a screenshot: image centre at y≈492 against a region centre of
550 — off by 58 px, and `toolbar_h` at 144 DPI is **57**.

The fix is one subtraction:

```hlsl
float2 sp = pos.xy - surf_origin;   // back into the viewport's own frame
```

But here's the part worth keeping. **This bug did not exist before Phase 1.** The old child window
had its own render target that *started* at the image region, so `SV_Position` was viewport-relative
*by construction* and the shader's assumption was free. Collapsing to one window quietly invalidated
an assumption nothing in the code had ever had to state.

That's now a documented invariant, because the symptom points at the wrong file.

---

## Performance: the part we actually cared about

### The idle scare

With the new settings window open, idle CPU measured **0.20%** of a core, against **0.00%** with it
closed. That looks exactly like a free-running repaint — precisely the failure mode we'd spent the
whole migration guarding against.

We instrumented `WM_PAINT` in a debug build and counted. **Zero paints across six idle seconds.**

The 0.20% was the measurement floor: the sample interval was 15.6 ms, and the Windows scheduler
quantum is **15.625 ms**. We were measuring the ruler, not the thing.

The one timer in the app that repaints with no input behind it is the **caret blink** — and it's
armed *only* while `io.WantTextInput` is true (i.e. while a settings text field is actually being
edited) and killed the instant focus leaves.

Idle is still 0.00%.

### Measuring time-to-first-pixel honestly

This is the number the app exists for, so the measurement deserved care.

**What counts as "first pixel"?** We defined it as *process creation → the first `Present` that
carries image pixels*. That covers everything Explorer actually pays for: the loader, CRT init,
config read, window creation, D3D device creation, decode, upload, and the frame itself.

Three details mattered:

1. **`Instant::now()` at the top of `main()` is wrong.** It cannot see the loader and CRT time —
   real milliseconds the user experiences. We used `GetProcessTimes`, which reports the true process
   creation time, against `GetSystemTimePreciseAsFileTime` at the Present.

2. **Release builds have no console** (`windows_subsystem = "windows"`), so the timing goes to a
   file, not stderr. Easy to lose an afternoon to that one.

3. **Both branches have exactly one `Present(` call**, in `render/gpu.rs`, and both keep a
   `current_image`. So the *identical* instrumentation drops into the same conceptual point on each,
   gated on an image being present — no arguing about whether the two builds were measured the same
   way.

We then built both branches in throwaway git worktrees and **interleaved** the runs, flipping the A/B
order every iteration. That last bit isn't ceremony: the measured spread is only ~1.5 ms, so "which
build happened to run second" would otherwise have been a *larger* effect than the thing we were
trying to measure.

### The result

Twenty runs per cell, on a 38 KB PNG and an 8.9 MB one:

| Build | Image | Median | Mean | SD | vs. GDI |
|---|---|---|---|---|---|
| GDI | 38 KB | 143.8 ms | 143.8 | 1.5 | — |
| **ImGui** | 38 KB | **141.1 ms** | 141.3 | 1.6 | **−2.7 ms** |
| GDI | 8.9 MB | 170.4 ms | 170.2 | 1.8 | — |
| **ImGui** | 8.9 MB | **164.4 ms** | 165.0 | 2.0 | **−6.0 ms** |

Not "no regression." **Faster.** On both images, by several standard errors — these gaps are real,
not drift. (The GDI numbers also reproduced an older baseline almost exactly: 143.8 vs 143.7, which
is a good sign the method is sound.)

### Why an immediate-mode UI made startup *faster*

The ImGui executable is **1.4 MB bigger** (10.35 → 11.77 MB) and it *still wins*. That's the clue:
image loading isn't the bottleneck. **Decode dominates**, and the UI layer simply is not on the
critical path.

The win is Phase 1 — the single-window collapse. The old build, before it could show you a pixel:

- created a **second HWND** for the viewport, and
- painted the chrome with GDI on the UI thread during startup — creating fonts, blitting icons
  through device contexts, double-buffering the lot.

All of that is gone. ImGui's context creation and font atlas upload cost *less* than the window and
the GDI paint they replaced. The icons are now a single texture, and the chrome is a handful of
triangles on a GPU that was already initialized and idle.

The irony is neat: we adopted a library whose reputation is "redraws constantly, made for games," and
the result was a program that draws **less**.

**One caveat, stated rather than buried.** On the old build, the chrome was painted separately from
the viewport's `Present`, so its timestamp is "image pixels are up," possibly *before* the chrome
finished drawing. On the new build, `Present` happens after the entire UI is built. If that biases
anything, it **flatters the old build** — the real gap is at least this large, not smaller.

---

## What's left over

| | Before | After |
|---|---|---|
| UI code | ~4,700 lines | ~2,570 lines |
| Windows | 2 (+ 2 window procedures) | 1 |
| Undocumented APIs | 3 uxtheme ordinals | **0** |
| Nested message pumps | 1 (settings dialog) | 0 |
| GDI painting | all of it | **none** |
| Idle CPU | 0.00% | 0.00% |
| Time-to-first-pixel | 143.8 / 170.4 ms | **141.1 / 164.4 ms** |

Across the whole change: **26 files, +3,641 / −5,750** — a net deletion of about 2,100 lines, with
clippy clean and 61 tests passing.

The settings window is now 672 lines instead of 1,879, contains **no pixel constants** at all (it
opens at a fraction of the viewport, the footer is pinned by a negative-height child region, and
every control's width is derived from the longest label *measured in the live font*), and — because
ImGui modals pump no messages — the `&mut App` aliasing hazard didn't get *managed*. It **stopped
existing**.

Exactly one deferred Win32 modal remains: the "Browse…" file picker, which genuinely does run a
nested loop.

---

## What we'd tell you

**The invariant you're protecting is not the one the library threatens.** We spent the migration
braced for ImGui's redraw-forever culture to wreck our idle cost. It didn't — a frame counter that
terminates was enough. Meanwhile the *real* win came from a constraint we'd stopped noticing: GDI
can't paint on a flip-model swapchain, so we'd been carrying a second window for years. Removing the
UI framework let us remove the window, and removing the window is what bought the milliseconds.

**Measure the ruler before you believe the measurement.** 0.20% idle looked like a leak and was a
scheduler quantum. A 58-pixel offset looked like a centring bug and was a coordinate space.

**Don't reimplement a slider.** The 979-line transport band is 35 lines of pure data now. Every one
of the deleted lines was a place a bug could live, and several of them did.

And the thing we didn't expect: deleting a UI framework you wrote yourself, and replacing it with one
designed for 60 fps games, can make a program that draws *only when you ask it to* measurably faster
at the one job it has.
