//! Throwaway CPU-rendering viability probe (branch: cpu-rendering-test).
//!
//! Decodes an image with the *real* fire-decode path, then renders it on the CPU into a
//! softbuffer framebuffer presented to a winit window — no GPU device, no D3D runtime, no
//! driver UMD. The point is to measure, on a large (8K) texture:
//!   * time-to-first-pixel  (decode + first shade)
//!   * decoded-source RAM   (the bitmap we must hold to sample from)
//!   * per-frame shade time  at fit (minify) vs zoomed-in (magnify) vs pan
//!   * process working set   (printed each frame; also watch Task Manager)
//!
//! The shading mirrors the daemon's WGSL pipeline for the 8-bit/LDR path: linear-light
//! checkerboard composite, solo-channel isolation, sRGB encode. HDR exposure/tonemap is
//! omitted (the test image is an 8-bit PNG); it would cost the same per pixel on CPU.

use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use fire_decode::{DecodeOptions, DecodedImage, PixelFormat};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

mod view;
use view::{Channel, ViewState, Viewport};

/// Checkerboard cell size in surface px (matches the shader's CHECKER_SIZE).
const CHECKER_SIZE: f32 = 12.0;
/// Multiplicative zoom per wheel notch / keyboard step (matches the daemon).
const ZOOM_STEP: f32 = 1.15;

type Ctx = softbuffer::Context<Rc<Window>>;
type Surf = softbuffer::Surface<Rc<Window>, Rc<Window>>;

fn main() {
    // Captured as early as possible in main() so we can report cold-start phases. The
    // pre-main loader cost (exe + DLL page-ins) is captured separately by wrapping a
    // `--bench` run in PowerShell Measure-Command.
    let t_proc = Instant::now();

    let args: Vec<String> = std::env::args().collect();
    let bench = args.iter().any(|a| a == "--bench");
    let path = match args.iter().skip(1).find(|a| !a.starts_with("--")) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: cpu-test [--bench] <image-path>");
            return;
        }
    };

    // Decode through the production path. max_dim huge so the full 8K is kept (the GPU
    // viewer also keeps it: 8192 < the 16384 device limit), giving the worst-case RAM.
    let opts = DecodeOptions { max_dim: 1 << 17, honor_icc: true };
    let t = Instant::now();
    let img = match fire_decode::decode_path(Path::new(&path), &opts) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("decode failed: {e}");
            return;
        }
    };
    let decode_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "decoded {}x{} {:?}  in {:.1} ms   |   source bitmap in RAM = {:.1} MB",
        img.width,
        img.height,
        img.format,
        decode_ms,
        img.pixels.len() as f64 / 1.0e6
    );
    println!("working set after decode (pre-window): {:.1} MB", working_set_mb());
    if img.format != PixelFormat::Rgba8Unorm {
        eprintln!("note: prototype shades only the 8-bit path; {:?} will look wrong", img.format);
    }

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(img, t_proc, decode_ms, bench);
    event_loop.run_app(&mut app).expect("run");
}

struct App {
    image: DecodedImage,
    /// sRGB byte -> linear float (256 entries).
    lut_lin: [f32; 256],
    /// linear [0,1] -> sRGB byte (4097 entries, indexed by `lin * 4096`).
    lut_srgb: Vec<u8>,
    window: Option<Rc<Window>>,
    _context: Option<Ctx>,
    surface: Option<Surf>,
    view: ViewState,
    viewport: Viewport,
    channel: Channel,
    cursor: (f32, f32),
    dragging: bool,
    frames: u64,
    /// Process-entry timer + the measured decode cost, for cold-start reporting.
    t_proc: Instant,
    decode_ms: f64,
    /// `--bench`: exit right after the first present so an external timer captures
    /// the full launch→first-pixel wall clock.
    bench: bool,
}

impl App {
    fn new(image: DecodedImage, t_proc: Instant, decode_ms: f64, bench: bool) -> Self {
        let mut lut_lin = [0.0f32; 256];
        for (i, v) in lut_lin.iter_mut().enumerate() {
            *v = srgb_to_linear(i as f32 / 255.0);
        }
        let mut lut_srgb = vec![0u8; 4097];
        for (i, v) in lut_srgb.iter_mut().enumerate() {
            *v = (linear_to_srgb(i as f32 / 4096.0).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        Self {
            image,
            lut_lin,
            lut_srgb,
            window: None,
            _context: None,
            surface: None,
            view: ViewState::default(),
            viewport: Viewport::new(1, 1),
            channel: Channel::Rgb,
            cursor: (0.0, 0.0),
            dragging: false,
            frames: 0,
            t_proc,
            decode_ms,
            bench,
        }
    }

    fn dims(&self) -> (u32, u32) {
        (self.image.width, self.image.height)
    }

    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn render(&mut self) {
        let w = self.viewport.width as u32;
        let h = self.viewport.height as u32;
        if w == 0 || h == 0 {
            return;
        }
        let surface = match self.surface.as_mut() {
            Some(s) => s,
            None => return,
        };
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .unwrap();
        let mut buf = surface.buffer_mut().unwrap();

        let t0 = Instant::now();
        // Disjoint field borrows: `buf` holds &mut self.surface; the rest are other fields.
        shade(
            &mut buf,
            w,
            h,
            &self.image,
            &self.view,
            &self.viewport,
            self.channel,
            &self.lut_lin,
            &self.lut_srgb,
        );
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        buf.present().unwrap();

        let first = self.frames == 0;
        self.frames += 1;
        let kind = if self.view.zoom < 1.0 { "minify" } else { "magnify" };
        let tag = if first { "  <- time-to-first-pixel (render only)" } else { "" };
        println!(
            "render {:6.2} ms  [{kind}]  zoom {:.3}  ws {:.1} MB{tag}",
            dt,
            self.view.zoom,
            working_set_mb()
        );

        if first {
            let to_pixel = self.t_proc.elapsed().as_secs_f64() * 1000.0;
            println!(
                "[coldstart] main->first-pixel {:.1} ms  (decode {:.1} ms + startup/window/first-shade {:.1} ms)",
                to_pixel,
                self.decode_ms,
                to_pixel - self.decode_ms
            );
            if self.bench {
                // Let the external Measure-Command capture loader + everything; exit now.
                std::process::exit(0);
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let (cw, ch) = clamp_window(self.image.width, self.image.height);
        let attrs = Window::default_attributes()
            .with_title("Fire — CPU render test (no GPU)")
            .with_inner_size(PhysicalSize::new(cw, ch));
        let window = Rc::new(el.create_window(attrs).expect("create window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, window.clone()).expect("softbuffer surface");
        let sz = window.inner_size();
        self.viewport = Viewport::new(sz.width, sz.height);
        self.view.fit_to_window(self.dims(), &self.viewport);
        self.window = Some(window.clone());
        self._context = Some(context);
        self.surface = Some(surface);
        println!(
            "[coldstart] main->window-created {:.1} ms",
            self.t_proc.elapsed().as_secs_f64() * 1000.0
        );
        println!(
            "controls: wheel=zoom-to-cursor  drag=pan  F=fit  1=1:1  R/G/B/A=solo  C=rgb  +/-=zoom  M=mem  Esc=quit"
        );
        window.request_redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.render(),
            WindowEvent::Resized(size) => {
                self.viewport = Viewport::new(size.width, size.height);
                if self.view.fit {
                    self.view.fit_to_window(self.dims(), &self.viewport);
                } else {
                    self.view.clamp_pan(self.dims(), &self.viewport);
                }
                self.redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 / 60.0,
                };
                if dy != 0.0 {
                    self.view
                        .zoom_to_cursor(ZOOM_STEP.powf(dy), self.cursor, self.dims(), &self.viewport);
                    self.redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let pos = (position.x as f32, position.y as f32);
                let delta = (pos.0 - self.cursor.0, pos.1 - self.cursor.1);
                self.cursor = pos;
                if self.dragging {
                    self.view.pan_by(delta, self.dims(), &self.viewport);
                    self.redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.dragging = state == ElementState::Pressed;
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let mut redraw = true;
                match &event.logical_key {
                    Key::Character(s) => match s.as_str() {
                        "f" | "F" => self.view.fit_to_window(self.dims(), &self.viewport),
                        "1" => self.view.one_to_one(),
                        "r" | "R" => self.channel = toggle(self.channel, Channel::R),
                        "g" | "G" => self.channel = toggle(self.channel, Channel::G),
                        "b" | "B" => self.channel = toggle(self.channel, Channel::B),
                        "a" | "A" => self.channel = toggle(self.channel, Channel::A),
                        "c" | "C" => self.channel = Channel::Rgb,
                        "=" | "+" => self.view.zoom_centered(ZOOM_STEP, self.dims(), &self.viewport),
                        "-" | "_" => self.view.zoom_centered(1.0 / ZOOM_STEP, self.dims(), &self.viewport),
                        "m" | "M" => {
                            println!("working set: {:.1} MB", working_set_mb());
                            redraw = false;
                        }
                        _ => redraw = false,
                    },
                    Key::Named(NamedKey::Escape) => el.exit(),
                    _ => redraw = false,
                }
                if redraw {
                    self.redraw();
                }
            }
            _ => {}
        }
    }
}

fn toggle(cur: Channel, c: Channel) -> Channel {
    if cur == c {
        Channel::Rgb
    } else {
        c
    }
}

/// Initial on-screen window size: fit the image within a sane cap, preserving aspect.
fn clamp_window(w: u32, h: u32) -> (u32, u32) {
    const MAX_W: f32 = 1600.0;
    const MAX_H: f32 = 1000.0;
    let w = w.max(1) as f32;
    let h = h.max(1) as f32;
    let s = (MAX_W / w).min(MAX_H / h).min(1.0);
    (((w * s) as u32).max(1), ((h * s) as u32).max(1))
}

// --- CPU shading ------------------------------------------------------------

#[inline]
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[inline]
fn enc(lut_srgb: &[u8], lin: f32) -> u8 {
    let i = (lin.clamp(0.0, 1.0) * 4096.0 + 0.5) as usize;
    lut_srgb[i.min(4096)]
}

#[inline]
const fn pack(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Two neutral grays in linear (match the shader), composited then sRGB-encoded.
#[inline]
fn checker(x: u32, y: u32) -> f32 {
    let cx = (x as f32 / CHECKER_SIZE).floor();
    let cy = (y as f32 / CHECKER_SIZE).floor();
    if ((cx + cy) as i64 & 1) == 0 {
        0.45
    } else {
        0.21
    }
}

/// Fan the framebuffer out across cores by horizontal bands. Per-pixel: inverse-map the
/// surface pixel into image space, sample (nearest when magnifying, box-average over the
/// footprint when minifying), then shade. Cost is O(surface pixels) — independent of the
/// source resolution except for the minify footprint.
#[allow(clippy::too_many_arguments)]
fn shade(
    buf: &mut [u32],
    w: u32,
    h: u32,
    img: &DecodedImage,
    view: &ViewState,
    vp: &Viewport,
    channel: Channel,
    lut_lin: &[f32; 256],
    lut_srgb: &[u8],
) {
    let total = (w as usize) * (h as usize);
    let buf = &mut buf[..total];
    let view = *view;
    let vp = *vp;

    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let nthreads = ncpu.min(h as usize).max(1);
    let rows_per = (h as usize + nthreads - 1) / nthreads;
    let band = rows_per * w as usize;

    std::thread::scope(|sc| {
        let mut rest: &mut [u32] = buf;
        let mut y_start: u32 = 0;
        while !rest.is_empty() {
            let take = band.min(rest.len());
            let (chunk, tail) = rest.split_at_mut(take);
            rest = tail;
            let ys = y_start;
            y_start += (take / w as usize) as u32;
            sc.spawn(move || {
                shade_band(chunk, ys, w, img, view, vp, channel, lut_lin, lut_srgb);
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn shade_band(
    chunk: &mut [u32],
    y_start: u32,
    w: u32,
    img: &DecodedImage,
    view: ViewState,
    vp: Viewport,
    channel: Channel,
    lut_lin: &[f32; 256],
    lut_srgb: &[u8],
) {
    const BG: u32 = pack(30, 30, 34);

    let iw = img.width as f32;
    let ih = img.height as f32;
    let iwi = img.width as i32;
    let ihi = img.height as i32;
    let stride = img.width as usize * 4;
    let px = &img.pixels;

    let c = vp.center();
    let cx = c.0 + view.pan.0;
    let cy = c.1 + view.pan.1;
    let inv = 1.0 / view.zoom;

    let minify = view.zoom < 1.0;
    let foot = if minify { (inv.round() as i32).clamp(1, 6) } else { 1 };
    let half = foot / 2;
    let inv_taps = 1.0 / (foot * foot) as f32;

    let rows = chunk.len() / w as usize;
    for yy in 0..rows {
        let y = y_start + yy as u32;
        let sy = y as f32 + 0.5;
        let fy = ih * 0.5 + (sy - cy) * inv;
        let row = yy * w as usize;
        for x in 0..w {
            let sx = x as f32 + 0.5;
            let fx = iw * 0.5 + (sx - cx) * inv;

            let out = if fx < 0.0 || fy < 0.0 || fx >= iw || fy >= ih {
                BG
            } else if !minify {
                // Magnify / 1:1 — nearest neighbour (crisp texels), with an exact byte fast
                // path for the common opaque-RGB case.
                let o = (fy as usize) * stride + (fx as usize) * 4;
                let (r, g, b, a) = (px[o], px[o + 1], px[o + 2], px[o + 3]);
                match channel {
                    Channel::Rgb if a == 255 => pack(r, g, b),
                    Channel::R => pack(r, r, r),
                    Channel::G => pack(g, g, g),
                    Channel::B => pack(b, b, b),
                    Channel::A => pack(a, a, a),
                    Channel::Rgb => shade_linear(
                        lut_lin[r as usize],
                        lut_lin[g as usize],
                        lut_lin[b as usize],
                        a as f32 / 255.0,
                        channel,
                        x,
                        y,
                        lut_srgb,
                    ),
                }
            } else {
                // Minify — box-average the source footprint in linear light.
                let bx = fx as i32;
                let by = fy as i32;
                let (mut lr, mut lg, mut lb, mut la) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                for dy in 0..foot {
                    let yyy = (by - half + dy).clamp(0, ihi - 1) as usize;
                    for dx in 0..foot {
                        let xxx = (bx - half + dx).clamp(0, iwi - 1) as usize;
                        let o = yyy * stride + xxx * 4;
                        lr += lut_lin[px[o] as usize];
                        lg += lut_lin[px[o + 1] as usize];
                        lb += lut_lin[px[o + 2] as usize];
                        la += px[o + 3] as f32 / 255.0;
                    }
                }
                shade_linear(
                    lr * inv_taps,
                    lg * inv_taps,
                    lb * inv_taps,
                    la * inv_taps,
                    channel,
                    x,
                    y,
                    lut_srgb,
                )
            };
            chunk[row + x as usize] = out;
        }
    }
}

/// Shade a linear-light RGBA sample to a packed sRGB pixel: channel isolation, else
/// checkerboard composite over transparency, then encode.
#[inline]
#[allow(clippy::too_many_arguments)]
fn shade_linear(lr: f32, lg: f32, lb: f32, a: f32, channel: Channel, x: u32, y: u32, lut_srgb: &[u8]) -> u32 {
    match channel {
        Channel::R => {
            let v = enc(lut_srgb, lr);
            pack(v, v, v)
        }
        Channel::G => {
            let v = enc(lut_srgb, lg);
            pack(v, v, v)
        }
        Channel::B => {
            let v = enc(lut_srgb, lb);
            pack(v, v, v)
        }
        Channel::A => {
            let v = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            pack(v, v, v)
        }
        Channel::Rgb => {
            let (mut rr, mut gg, mut bb) = (lr, lg, lb);
            if a < 0.999 {
                let bg = checker(x, y);
                rr = bg * (1.0 - a) + lr * a;
                gg = bg * (1.0 - a) + lg * a;
                bb = bg * (1.0 - a) + lb * a;
            }
            pack(enc(lut_srgb, rr), enc(lut_srgb, gg), enc(lut_srgb, bb))
        }
    }
}

// --- process memory ---------------------------------------------------------

/// Current process working set in MB (Win32 `GetProcessMemoryInfo`).
fn working_set_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) != 0 {
            pmc.WorkingSetSize as f64 / 1.0e6
        } else {
            0.0
        }
    }
}
