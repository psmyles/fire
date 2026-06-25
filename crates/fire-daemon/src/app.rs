//! The winit 0.30 `ApplicationHandler`. Owns the warm GPU context and the pooled
//! window, handles incoming open requests (decode → upload → show → raise), and the
//! window's own events (redraw/resize/close-hides).

use std::sync::Arc;

use fire_decode::DecodeOptions;
use fire_ipc::OpenRequest;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Icon, WindowAttributes, WindowId};

use crate::decode_pool::{DecodeJob, DecodeOutcome, DecodePool};
use crate::gpu::{GpuContext, WindowState};
use crate::{foreground, UserEvent};

pub struct App {
    gpu: GpuContext,
    window: Option<WindowState>,
    pool: DecodePool,
}

impl App {
    pub fn new(pool: DecodePool) -> Self {
        // Warm the expensive GPU objects at startup, before any window or open (§12).
        Self { gpu: GpuContext::new(), window: None, pool }
    }

    /// Handle an open request: raise the pooled window immediately with a placeholder,
    /// then enqueue the decode off-thread. The texture swaps in when `DecodeDone`
    /// arrives — the window never blocks on decode (§5, Phase 2).
    fn open(&mut self, req: OpenRequest) {
        let Some(ws) = self.window.as_mut() else {
            return;
        };

        let name = file_name(&req);

        // Show the window now, before decode. Clear any previous image so a reused
        // window shows the placeholder (a solid clear; the "loading" label is Phase 4)
        // rather than the wrong file's pixels while the new one decodes.
        ws.clear_image();
        ws.window.set_title(&format!("{name} — Fire (loading…)"));
        ws.window.set_visible(true);
        ws.window.set_minimized(false);
        if req.flags.activate {
            // Raise on the click, not on decode-done — the foreground grant is one-shot
            // and must be spent promptly (§4.1).
            foreground::raise(&ws.window);
        }
        ws.window.request_redraw();

        // Tag the job with a fresh generation; a later open supersedes this decode.
        // honor_icc: lcms2 transforms non-sRGB profiles into the sRGB working space
        // (best-effort; see fire_decode::icc).
        let generation = ws.next_generation();
        let opts = DecodeOptions { max_dim: self.gpu.max_texture_dim(), honor_icc: true };
        self.pool.submit(DecodeJob {
            window_id: ws.window.id(),
            generation,
            path: req.path,
            opts,
        });
    }

    /// Handle a finished decode. Upload only if it is still the window's latest request
    /// (stale-drop): a slow PSD that finishes after a newer PNG was opened is discarded.
    fn decode_done(&mut self, outcome: DecodeOutcome) {
        let Some(ws) = self.window.as_mut() else {
            return;
        };
        if outcome.window_id != ws.window.id() || outcome.generation != ws.generation() {
            return; // superseded by a newer open, or a different window — drop it
        }

        let name = outcome
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image");
        match outcome.result {
            Ok(img) => {
                let (w, h, fmt) = (img.width, img.height, img.source_format);
                let (cw, ch) = clamp_window_size(w, h);
                let _ = ws.window.request_inner_size(PhysicalSize::new(cw, ch));
                ws.set_image(&self.gpu, img);
                ws.window.set_title(&format!("{name} — Fire"));
                ws.window.request_redraw();
                println!("fire-daemon: opened {:?} ({w}x{h}, {fmt})", outcome.path);
            }
            Err(e) => {
                ws.window.set_title(&format!("{name} — Fire (failed)"));
                eprintln!("fire-daemon: failed to open {:?}: {e}", outcome.path);
            }
        }
    }
}

/// The Fire window icon (taskbar + title bar), decoded for free from a raw 256×256 RGBA
/// blob baked into the binary — no PNG decoder at startup. `magick`-generated from
/// `assets/icon.png`; the on-disk exe icon is embedded separately via `build.rs`.
fn load_window_icon() -> Option<Icon> {
    const ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon_256.rgba");
    const SIDE: u32 = 256;
    // from_rgba only fails on a length mismatch; the blob is generated at exactly
    // 256×256×4, so this is infallible in practice (degrade to no icon if not).
    Icon::from_rgba(ICON_RGBA.to_vec(), SIDE, SIDE).ok()
}

/// File name of an open request's path, for the window title (falls back to "image").
fn file_name(req: &OpenRequest) -> String {
    req.path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string()
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Pre-create the pooled window HIDDEN and fully warm its surface + pipeline now,
        // at startup (resumed runs immediately). The first open just uploads a texture
        // and shows it (§12).
        let attrs = WindowAttributes::default()
            .with_title("Fire")
            .with_window_icon(load_window_icon())
            .with_visible(false)
            .with_inner_size(PhysicalSize::new(1280, 800));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );
        self.window = Some(WindowState::new(&self.gpu, window));
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Open(req) => self.open(req),
            UserEvent::DecodeDone(outcome) => self.decode_done(outcome),
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let Some(ws) = self.window.as_mut() else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => ws.render(&self.gpu),
            WindowEvent::Resized(size) => {
                ws.resize(&self.gpu, size.width, size.height);
                ws.window.request_redraw();
            }
            WindowEvent::CloseRequested => {
                // Resident: hide instead of exiting so the next open is instant.
                ws.window.set_visible(false);
            }
            _ => {}
        }
    }
}

/// Keep the initial window within a reasonable on-screen size while preserving aspect.
/// (Real fit/center math arrives with the view transform in Phase 3.)
fn clamp_window_size(w: u32, h: u32) -> (u32, u32) {
    const MAX_W: f32 = 1600.0;
    const MAX_H: f32 = 1000.0;
    let w = w.max(1) as f32;
    let h = h.max(1) as f32;
    let scale = (MAX_W / w).min(MAX_H / h).min(1.0);
    (((w * scale) as u32).max(1), ((h * scale) as u32).max(1))
}
