# Teaching an Image Viewer to Recognize Sprite Sheets

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
to "display first, or analyze first?" is: **the image displays first, and analysis follows.** The
detection time — which for a huge 8K/16K sheet means scanning hundreds of millions of pixels, easily
hundreds of milliseconds — is completely hidden: the image is already up, and the hint simply
appears whenever the scan finishes.

This wasn't the original design. At first, detection was bundled *with* decode into one message, so
it sat on the critical path and delayed the image. That was fine at a few milliseconds for normal
images, but for 8K/16K sheets it would have added a visible stall before the picture appeared. The
fix was to split it: the decoded pixels are shared (cheaply, by reference) between the display path
and the detection pass, so the image can go to the screen while the worker keeps analyzing the same
pixels in the background. A stale-guard makes sure that if you navigate away before detection
finishes, the late-arriving hint for the old image is simply discarded.

Because Fire uses several worker threads, detection of *different* images (say, when you arrow
through a folder) can also overlap across threads.

### How long it takes

Measured on the real test folder (release build, times are the median of repeated runs):

| Image size | Detection time |
|------------|----------------|
| 64–256 px  | 0.02 – 0.6 ms |
| 512 px     | 1.5 – 3.3 ms |
| 1024 px    | 3 – 6 ms |
| 2048×2048  | ~9 ms, or ~17 ms if it also runs the alpha pass |

Across all 23 files, detection averaged **~6.3 ms**, versus ~10.5 ms to decode — comparable to the
decode itself. But since it runs *after* the image is displayed, none of that time is felt as lag:
it only governs how long after the image the hint appears (14 ms on a real 2048×2048 sheet).

A few things explain the numbers:

- **Cost scales with pixel count, not grid complexity.** The dominant work is scanning every source
  pixel once to shrink the image into a small (≤512 px) grayscale analysis copy. The clever part —
  YIN period detection and the guards — runs on that tiny copy, so it's essentially constant
  regardless of whether the source is 512 px or 2048 px.

- **The alpha fallback can double the cost.** If the brightness pass finds a grid, that's one scan
  of the image. If it comes up empty and the alpha pass kicks in, that's a second scan. You can see
  this clearly in the 2048×2048 sheets: the ones detected on the first pass take ~9 ms, while
  `FireFar` (which needs the alpha channel) and the non-flipbook `Trail_Shred` (which scans both
  channels and still finds nothing) take ~17 ms.

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
