//! The winit 0.30 `ApplicationHandler`. Owns the warm GPU context and the pooled
//! window, handles incoming open requests (decode → upload → show → raise), and the
//! window's own events (redraw/resize/close-hides).

use std::sync::Arc;

use texview_decode::{decode_path, DecodeOptions};
use texview_ipc::OpenRequest;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{WindowAttributes, WindowId};

use crate::gpu::{GpuContext, WindowState};
use crate::{foreground, UserEvent};

pub struct App {
    gpu: GpuContext,
    window: Option<WindowState>,
}

impl App {
    pub fn new() -> Self {
        // Warm the expensive GPU objects at startup, before any window or open (§12).
        Self { gpu: GpuContext::new(), window: None }
    }

    fn open(&mut self, req: &OpenRequest) {
        let Some(ws) = self.window.as_mut() else {
            return;
        };

        // Phase 1 decodes synchronously on the main thread (the async worker pool is
        // Phase 2). The window is shown regardless of decode outcome so the foreground
        // handoff is always exercised. honor_icc: lcms2 transforms non-sRGB profiles into
        // the sRGB working space (best-effort; see texview_decode::icc).
        let opts = DecodeOptions { max_dim: self.gpu.max_texture_dim(), honor_icc: true };
        match decode_path(&req.path, &opts) {
            Ok(img) => {
                let (w, h) = clamp_window_size(img.width, img.height);
                let _ = ws.window.request_inner_size(PhysicalSize::new(w, h));
                ws.set_image(&self.gpu, &img);
                let name = req
                    .path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("image");
                ws.window.set_title(&format!("{name} — texview"));
                println!(
                    "texview-daemon: opened {:?} ({}x{}, {})",
                    req.path, img.width, img.height, img.source_format
                );
            }
            Err(e) => {
                eprintln!("texview-daemon: failed to open {:?}: {e}", req.path);
            }
        }

        ws.window.set_visible(true);
        ws.window.set_minimized(false);
        if req.flags.activate {
            foreground::raise(&ws.window);
        }
        ws.window.request_redraw();
    }
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
            .with_title("texview")
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
            UserEvent::Open(req) => self.open(&req),
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
