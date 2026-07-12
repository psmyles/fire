# Deleting 2,000 Lines of GDI and Getting *Faster*

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
