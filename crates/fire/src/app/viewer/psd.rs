//! PSD layer stepping, the viewer's half: the per-file session, the keys, and moving textures
//! between the decode workers, the CPU cache, the GPU stash and the screen.
//!
//! The policy — where the cursor is, what to decode ahead, what to keep — is `crate::layers`,
//! which is pure and unit-tested. This module only applies it:
//!
//! * A session starts when a PSD's layer index arrives ([`Viewer::psd_layers_ready`]), after its
//!   composite is on screen, and arms the dwell timer. Nothing decodes until the user has stayed
//!   on the file for [`layers::DWELL_MS`] or pressed a layer key.
//! * A step ([`Viewer::step_layer`]) shows the target straight away when it is staged on the GPU
//!   (a binding swap), queues an upload for the next frame when it is decoded in RAM (so a held
//!   key uploads only the layer each frame actually lands on), and otherwise keeps the current
//!   pixels up while the target decodes on every core.
//! * Decoded layers land in a byte-bounded cache; idle frames upload the neighbours most likely
//!   to be next into the stash ([`Viewer::layers_tick`]).
//! * Any new decode — another file, a reload, a close — ends the session first, putting the
//!   composite back and handing everything big to the reaper thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fire_decode::{DecodedImage, PixelFormat, PsdLayers};

use super::Viewer;
use crate::app::timers::TimerKind;
use crate::decode_pool::{LayerFail, LayerIndex, LayerJob, LayerOutcome};
use crate::layers::{self, ByteCache, JobCtl, LayerLabel, LayerState, Nav, SessionCtl};
use crate::render::gpu::Placement;

/// Session ids: a layer result is applied only to the session that asked for it.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// One decoded layer, as kept in RAM.
struct CachedLayer {
    image: Arc<DecodedImage>,
    mips: Vec<Vec<u8>>,
    placement: Placement,
}

/// Layer stepping for the PSD on screen.
pub(super) struct LayerSession {
    ctl: Arc<SessionCtl>,
    /// The window generation the session belongs to.
    generation: u64,
    path: PathBuf,
    doc: Arc<PsdLayers>,
    nav: Nav,
    /// Background decoding has begun (the dwell elapsed, or the user stepped).
    warm: bool,
    cache: ByteCache<CachedLayer>,
    /// Estimated RAM/VRAM of each decoded position, for the stash budget.
    sizes: HashMap<u32, u64>,
    inflight: HashMap<u32, Arc<JobCtl>>,
    failed: HashMap<u32, String>,
    /// The position whose pixels are on screen.
    showing: u32,
    /// A decoded layer to upload on the next frame (see `flush_layer_upload`).
    pending_upload: Option<u32>,
    last_step: Instant,
}

impl LayerSession {
    fn len(&self) -> u32 {
        self.nav.len
    }

    /// The layer at position `pos` (1-based).
    fn info(&self, pos: u32) -> Option<&fire_decode::LayerInfo> {
        self.doc.layers().get(pos.checked_sub(1)? as usize)
    }

    fn state(&self) -> LayerState {
        let pos = self.nav.pos;
        if let Some(e) = self.failed.get(&pos) {
            LayerState::Failed(e.clone())
        } else if self.showing == pos {
            LayerState::Shown
        } else {
            LayerState::Loading
        }
    }
}

impl Viewer {
    /// A PSD's layer stack arrived (after its composite). Start a session for it, unless the
    /// window has moved on since.
    pub fn psd_layers_ready(&mut self, index: LayerIndex) {
        if index.generation != self.surface.generation()
            || self.shown_path.as_deref() != Some(index.path.as_path())
        {
            self.pool.dispose(Box::new(index.doc));
            return;
        }
        self.end_layer_session();
        let len = index.doc.layers().len() as u32;
        self.layers = Some(LayerSession {
            ctl: Arc::new(SessionCtl::new(
                NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            )),
            generation: index.generation,
            path: index.path,
            doc: index.doc,
            nav: Nav::new(len),
            warm: false,
            cache: ByteCache::new(layers::CPU_CACHE_BYTES),
            sizes: HashMap::new(),
            inflight: HashMap::new(),
            failed: HashMap::new(),
            showing: 0,
            pending_upload: None,
            last_step: Instant::now(),
        });
        self.arm(TimerKind::Layers, layers::DWELL_MS);
        self.redraw(); // the "N layers" hint
    }

    /// End the layer session, if any: stop its jobs, put the composite back on screen, and free
    /// everything it held (off the UI thread).
    pub(super) fn end_layer_session(&mut self) {
        let Some(mut s) = self.layers.take() else {
            return;
        };
        s.ctl.cancelled.store(true, Ordering::Relaxed);
        for c in s.inflight.values() {
            c.cancelled.store(true, Ordering::Relaxed);
        }
        if s.showing != 0 {
            self.surface.show_staged(0, s.showing);
        }
        self.surface.clear_stage();
        self.kill(TimerKind::Layers);
        let cached = s.cache.drain();
        self.pool.dispose(Box::new((s.doc, cached)));
        self.redraw();
    }

    /// Up/Down: step through the layers (`delta` -1 toward the composite, +1 down the stack).
    pub(super) fn step_layer(&mut self, delta: i32) {
        if self.flipbook_state().is_some() || self.loading {
            return;
        }
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        if !s.nav.step(delta) {
            return;
        }
        s.ctl.cursor.store(s.nav.pos, Ordering::Relaxed);
        s.ctl.dir.store(s.nav.dir, Ordering::Relaxed);
        s.warm = true;
        s.last_step = Instant::now();
        s.pending_upload = None;
        let pos = s.nav.pos;
        if self.surface.show_staged(pos, s.showing) {
            s.showing = pos;
            crate::render::gpu::report_timing(&format!("psd layer {pos} shown (staged)"));
        } else if s.cache.contains(pos) {
            s.pending_upload = Some(pos);
        }
        self.trim_stash();
        self.schedule_layers();
        self.arm(TimerKind::Layers, layers::STAGE_IDLE_MS);
        self.redraw();
    }

    /// Back to the composite, without ending the session (entering flipbook mode does this).
    pub(super) fn show_composite(&mut self) {
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        if s.nav.pos == 0 {
            return;
        }
        let delta = -(s.nav.pos as i32);
        self.step_layer(delta);
    }

    /// Upload the decoded layer a step landed on, now that a frame is being drawn. Done here
    /// rather than in the key handler so that a held key, which delivers several steps per
    /// frame, uploads only the one each frame shows.
    pub(super) fn flush_layer_upload(&mut self) {
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        let Some(pos) = s.pending_upload.take() else {
            return;
        };
        if pos != s.nav.pos || s.showing == pos {
            return;
        }
        let park = s.showing;
        let Some(c) = s.cache.get(pos) else {
            return;
        };
        let t = Instant::now();
        match self
            .surface
            .show_layer(Arc::clone(&c.image), &c.mips, Some(c.placement), park)
        {
            Ok(()) => {
                s.showing = pos;
                crate::render::gpu::report_timing(&format!(
                    "psd layer {pos} upload — {:.2} ms",
                    t.elapsed().as_secs_f64() * 1e3
                ));
            }
            Err(e) => {
                eprintln!("fire: layer {pos} upload failed: {e}");
                s.failed.insert(pos, format!("GPU upload ({e})"));
            }
        }
        self.trim_stash();
    }

    /// A layer job finished (or gave up).
    pub fn layer_decoded(&mut self, o: LayerOutcome) {
        let Some(s) = self.layers.as_mut() else {
            self.pool
                .dispose(Box::new(o.result.ok().map(|d| (d.image, d.mips))));
            return;
        };
        if o.session != s.ctl.id || o.generation != s.generation {
            self.pool
                .dispose(Box::new(o.result.ok().map(|d| (d.image, d.mips))));
            return;
        }
        s.inflight.remove(&o.pos);
        match o.result {
            Ok(d) => {
                let bytes = layers::est_bytes(d.image.width, d.image.height, d.image.format);
                s.sizes.insert(o.pos, bytes);
                s.failed.remove(&o.pos);
                s.cache.insert(
                    o.pos,
                    CachedLayer {
                        image: d.image,
                        mips: d.mips,
                        placement: Placement {
                            origin: d.origin,
                            canvas: d.canvas,
                        },
                    },
                    bytes,
                );
                let protect = [s.nav.pos, s.showing];
                let evicted = s.cache.trim(s.nav.pos, &protect);
                if !evicted.is_empty() {
                    self.pool.dispose(Box::new(evicted));
                }
                let s = self.layers.as_mut().expect("still in a session");
                if o.pos == s.nav.pos && s.showing != o.pos {
                    s.pending_upload = Some(o.pos);
                    self.redraw();
                } else {
                    // A neighbour: upload it when the user next pauses.
                    let idle = s.last_step.elapsed();
                    let wait = Duration::from_millis(layers::STAGE_IDLE_MS).saturating_sub(idle);
                    self.arm(TimerKind::Layers, wait.as_millis() as u64);
                }
            }
            Err(LayerFail::Superseded) => {}
            Err(LayerFail::Failed(e)) => {
                eprintln!("fire: {}: layer {} failed: {e}", s.path.display(), o.pos);
                s.failed.insert(o.pos, e);
                if o.pos == s.nav.pos {
                    self.show_failed_layer();
                }
                self.redraw();
            }
        }
        self.schedule_layers();
    }

    /// The layer the cursor is on failed to decode: show it as empty (transparent over the
    /// backdrop) rather than leave another layer's pixels under its name.
    fn show_failed_layer(&mut self) {
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        let Some(canvas) = self.surface.content_dims() else {
            return;
        };
        let format = self
            .surface
            .current_image()
            .map_or(PixelFormat::Rgba8Unorm, |i| i.format);
        let bpp = crate::render::mips::bytes_per_texel(format);
        let empty = Arc::new(DecodedImage {
            pixels: vec![0; bpp],
            width: 1,
            height: 1,
            format,
            bit_depth: 8,
            channels: 4,
            alpha_opaque: false,
            icc: None,
            source_format: "PSD",
            downscaled_from: None,
            source_mips: None,
            layout: None,
            animation: None,
        });
        let placement = Placement {
            origin: (0, 0),
            canvas,
        };
        // A 1×1 texture placed on the canvas; the shader reads everything as transparent past it.
        if self
            .surface
            .show_layer(empty, &[], Some(placement), s.showing)
            .is_ok()
        {
            s.showing = s.nav.pos;
        }
        self.trim_stash();
    }

    /// Start, promote, demote and cancel layer jobs to match where the cursor is.
    fn schedule_layers(&mut self) {
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        let target = s.nav.pos;
        // Work the cursor has left behind stops at its next checkpoint (and replies Superseded);
        // whatever is not the target runs at background speed.
        for (&p, c) in &s.inflight {
            if p != target {
                c.foreground.store(false, Ordering::Relaxed);
            }
            if !layers::in_window(p, target, s.nav.dir) {
                c.cancelled.store(true, Ordering::Relaxed);
            }
        }
        if !s.warm && target == 0 {
            return;
        }
        let running = |s: &LayerSession, large: bool| {
            s.inflight
                .iter()
                .filter(|(p, c)| {
                    !c.cancelled.load(Ordering::Relaxed)
                        && !c.foreground.load(Ordering::Relaxed)
                        && (!large
                            || s.doc.layer_bytes(**p as usize - 1) >= layers::LARGE_JOB_BYTES)
                })
                .count()
        };
        for p in s.nav.wanted() {
            if s.cache.contains(p) || s.failed.contains_key(&p) || self.surface.is_staged(p) {
                continue;
            }
            if s.showing == p {
                continue;
            }
            let fg = p == target;
            // Already in flight: wanted again, so un-cancel it (if it already gave up, its
            // Superseded reply comes back and the next pass resubmits), and promote the target.
            // Never replaced while in flight — its reply is matched by position.
            if let Some(c) = s.inflight.get(&p) {
                c.cancelled.store(false, Ordering::Relaxed);
                if fg {
                    c.foreground.store(true, Ordering::Relaxed);
                }
                continue;
            }
            if !fg {
                if running(s, false) >= layers::MAX_BACKGROUND_JOBS {
                    continue;
                }
                let large = s.doc.layer_bytes(p as usize - 1) >= layers::LARGE_JOB_BYTES;
                if large && running(s, true) >= 1 {
                    continue;
                }
            }
            let ctl = Arc::new(JobCtl::default());
            ctl.foreground.store(fg, Ordering::Relaxed);
            s.inflight.insert(p, Arc::clone(&ctl));
            self.pool.submit_layer(LayerJob {
                window: self.window.id(),
                generation: s.generation,
                session: Arc::clone(&s.ctl),
                ctl,
                pos: p,
                doc: Arc::clone(&s.doc),
                opts: fire_decode::DecodeOptions {
                    max_dim: super::MAX_CPU_DIM,
                    honor_icc: true,
                },
            });
        }
    }

    /// The Layers timer: first the dwell (start pre-decoding), then idle-time staging.
    pub(super) fn layers_tick(&mut self) {
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        if !s.warm {
            s.warm = true;
            self.schedule_layers();
        }
        let Some(s) = self.layers.as_mut() else {
            return;
        };
        let idle = s.last_step.elapsed();
        let need = Duration::from_millis(layers::STAGE_IDLE_MS);
        if idle < need {
            self.arm(TimerKind::Layers, (need - idle).as_millis() as u64 + 1);
            return;
        }
        if self.stage_one() {
            self.arm(TimerKind::Layers, 1);
        }
    }

    /// Release staged textures the cursor no longer needs (the composite always stays).
    fn trim_stash(&mut self) {
        let Some(s) = self.layers.as_ref() else {
            return;
        };
        let keep = self.stash_keep(s);
        for k in self.surface.staged_keys() {
            if k != 0 && !keep.contains(&k) {
                self.surface.unstage(k);
            }
        }
    }

    fn stash_keep(&self, s: &LayerSession) -> Vec<u32> {
        let order = s.nav.keep_order(&self.surface.staged_keys());
        layers::stage_keep(
            &order,
            |p| {
                (p != s.showing && (s.cache.contains(p) || self.surface.is_staged(p)))
                    .then(|| s.sizes.get(&p).copied())
                    .flatten()
            },
            layers::GPU_STASH_BYTES,
        )
    }

    /// Upload one wanted neighbour into the stash. Returns whether there may be more to do.
    fn stage_one(&mut self) -> bool {
        self.trim_stash();
        let Some(s) = self.layers.as_ref() else {
            return false;
        };
        let keep = self.stash_keep(s);
        let next = keep
            .into_iter()
            .find(|&p| p != 0 && p != s.showing && !self.surface.is_staged(p));
        let Some(p) = next else {
            return false;
        };
        let s = self.layers.as_mut().expect("checked above");
        let Some(c) = s.cache.get(p) else {
            return false;
        };
        let t = Instant::now();
        match self
            .surface
            .stage(p, Arc::clone(&c.image), &c.mips, Some(c.placement))
        {
            Ok(()) => {
                crate::render::gpu::report_timing(&format!(
                    "psd layer {p} staged — {:.2} ms",
                    t.elapsed().as_secs_f64() * 1e3
                ));
                true
            }
            Err(e) => {
                // Out of video memory, most likely: keep only the composite and stop staging.
                eprintln!("fire: could not stage layer {p}: {e}");
                for k in self.surface.staged_keys() {
                    if k != 0 {
                        self.surface.unstage(k);
                    }
                }
                false
            }
        }
    }

    /// The status-bar text for layer mode: the detail after the file name, and the counter that
    /// leads the right side. `None` without a session.
    pub(super) fn layer_status(&self) -> Option<(Option<String>, String)> {
        let s = self.layers.as_ref()?;
        let detail = s
            .info(s.nav.pos)
            .map(|l| layers::status_detail(&LayerLabel::from(l)));
        Some((
            detail,
            layers::status_counter(s.nav.pos, s.len(), &s.state()),
        ))
    }

    /// Whether a layer (rather than the composite) is on screen.
    pub(super) fn showing_layer(&self) -> bool {
        self.layers.as_ref().is_some_and(|s| s.showing != 0)
    }
}
