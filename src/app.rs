//! The opsin viewer — a fluor app following Photon's skeleton. v0 scope: open one VSF-Image, render it colour-honestly (raw camera space, gamma-2 display encode), pan with drag, zoom with wheel, DefaultChrome for window controls. The Browser/Inspect/Convert states from [`crate::state`] grow from here.

use fluor::coord::Coord;
use fluor::event::{CursorIcon, ElementState, Event as FEvent, Key, MouseButton, MouseScrollDelta, NamedKey};
use fluor::geom::Viewport;
use fluor::host::app::{Context, EventResponse, FluorApp};
use fluor::host::chrome::{self, HIT_NONE, HitId, ResizeEdge};
use fluor::host::chrome_widget::DefaultChrome;
use fluor::host::widget::Container;
use fluor::canvas::Canvas;
use fluor::paint::{self, Clip};
use fluor::pixel::{Blend, BlendMode};

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::panel::{Observer, PanelTools, CHART_RES, HIST_BINS};

/// Empty-canvas backdrop behind/around the image: opaque near-black in α+darkness packing (α=0xFF, darkness ≈ high = dark visible).
const BACKDROP: u32 = 0xFF_F2_F2_F2;
/// Panel background — a shade above the backdrop so the tool area reads as a surface.
const PANEL_BG: u32 = 0xFF_E6_E6_E6;
/// Const-context version of `paint::pack_argb` — same visible-RGB → α+darkness packing.
const fn argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((a as u32) << 24) | (((255 - r) as u32) << 16) | (((255 - g) as u32) << 8) | ((255 - b) as u32)
}
/// Divider + section hairlines: flat grey, same 1px weight as Photon's button strokes.
const HAIRLINE: u32 = argb(0x60, 0x60, 0x60, 0xFF);
/// Panel label text (EV readout, button labels).
const TEXT_GREY: u32 = argb(0xE0, 0xE0, 0xE0, 0xFF);
/// Histogram channel colours (translucent so overlaps read additively under the front-to-back blend).
const HIST_R: u32 = argb(0xE0, 0x40, 0x40, 0x90);
const HIST_G: u32 = argb(0x40, 0xE0, 0x40, 0x90);
const HIST_B: u32 = argb(0x50, 0x50, 0xF0, 0x90);
/// Chromaticity chart curves.
const LOCUS: u32 = argb(0xB0, 0xB0, 0xB0, 0xFF);
const PLANCK: u32 = argb(0xE0, 0xA0, 0x40, 0xFF);

/// Base tone (visible RGB) for the top-bar noise texture — the controls-strip grey (`WINDOW_CONTROLS_BG` ≈ 0x1E1E1E visible) so the textured fill and the flat control fill sit at the same value.
const BAR_TEXTURE_BASE: u32 = 0x00_1E_1E_1E;

pub struct OpsinApp {
    title: String,
    chrome: DefaultChrome,
    /// Display-ready α+darkness pixels, img_w × img_h, gamma-2 encoded from the linear rendering. Raw camera space on purpose — no observer applied until the Inspect/observer machinery lands.
    pixels: Vec<u32>,
    img_w: usize,
    img_h: usize,
    /// View transform in RELATIVE units (AGENT.md dimensional units — no stored pixels anywhere in the image pipe). `zoom_rel` is image scale per unit of the image area's harmonic-mean span (2wh/(w+h)); the drawn zoom is `zoom_rel * area_span`, derived fresh from the live viewport every frame. So the composition scales continuously and smoothly with the window — resize any edge, drag the divider, maximize: the image tracks like every other UI element, fitted or zoomed alike, no modes.
    zoom_rel: f32,
    /// The image-space point (as fractions of image dims) anchored at the centre of the image area. Pan and navigator move these; resizes preserve them — which IS composition preservation.
    cx_frac: f32,
    cy_frac: f32,
    /// Cursor position at the last drag event while panning; None = not panning.
    drag: Option<(Coord, Coord)>,
    /// One-shot fit queued for the next render — the window opens at a guessed size before the surface reports real dims, so load-time fitting must be lazy (render is the first callback guaranteed real dims). NOT a mode: the span-relative transform preserves the fitted composition thru resizes by construction.
    needs_fit: bool,
    /// Viewport dims mirrored from init/on_resize so hit_test_map (which has no Context) can report them.
    view_w: usize,
    view_h: usize,
    /// Right tool panel width as a fraction of window width; the divider drags it.
    panel_frac: f32,
    divider_drag: bool,
    tools: PanelTools,
    /// Supported images in the opened folder, sorted; ←/→ step through them.
    dir_list: Vec<PathBuf>,
    /// Index into `dir_list` of the image currently shown.
    dir_idx: usize,
    /// Linear SIGNED Rec.2020 of the current image (white = 65535, out-of-range preserved) — kept so exposure re-encodes without re-decoding, and so the EV multiply can recover clipped-at-display speculars and sub-black noise.
    lin: Vec<i32>,
    /// Exposure in stops (gain = 2^ev in linear). ±[EV_RANGE].
    ev: f32,
    /// The panel's exposure slider (fluor widget, value 0..1 ↔ −EV_RANGE..+EV_RANGE).
    ev_slider: fluor::widgets::Slider,
    /// 1:1 / Fit — fluor pill Buttons, same widget family as the slider and chrome (squircle, AA edge, hover tint thru the host overlay pipe). Geometry is set every frame from panel_rects; hit silhouettes stamp into the chrome hit map at render, so dispatch rides the same Container walk as the chrome controls.
    btn_one: fluor::widgets::Button,
    btn_fit: fluor::widgets::Button,
    /// True while dragging the exposure slider handle.
    ev_drag: bool,
    /// True while dragging inside the navigator — every cursor move re-centers the main view live.
    nav_drag: bool,
    /// Total allocated HitIds (chrome + pills + slider) — the host's overlay tables are indexed by id, so their length is this + 1.
    hit_count: HitId,
    // --- [] debug chord (photon's scheme): both brackets held arms the chord, the next letter fires a debug toggle. Press/release Instants instead of booleans so X11's synthetic Release on the next keypress is absorbed by the grace window.
    chord_lb_press: Option<Instant>,
    chord_lb_release: Option<Instant>,
    chord_rb_press: Option<Instant>,
    chord_rb_release: Option<Instant>,
    /// Hitmask debug overlay active ([]h) — render's last act replaces every pixel with its hit id's palette colour.
    show_hitmask: bool,
    /// 256 random opaque colours (α+darkness), regenerated on each []h enable so distinct ids always pop.
    debug_hit_colours: Vec<u32>,
}

/// Grace for X11's synthetic key-Release while a key is actually held (photon's chord constant).
const CHORD_RELEASE_GRACE: Duration = Duration::from_millis(40);

/// Exposure slider half-range in stops.
const EV_RANGE: f32 = (1 << 2) as f32;

/// Panel section rects (x0, y0, w, h) — named fields, no position-coded indexing.
struct PanelRects {
    nav: (usize, usize, usize, usize),
    btns: (usize, usize, usize, usize),
    hist: (usize, usize, usize, usize),
    ev: (usize, usize, usize, usize),
    chart: (usize, usize, usize, usize),
}

/// One decoded image, ready to install into the viewer. Produced by [`load_image`], consumed by `open` (construction) and `load_current` (navigation). Keeps the linear RGB so exposure changes re-encode without re-decoding the source.
struct Loaded {
    pixels: Vec<u32>,
    lin: Vec<i32>,
    w: usize,
    h: usize,
    tools: PanelTools,
    title: String,
}

/// Linear signed Rec.2020 → gamma-2 u8 visible → darkness-packed u32. Exposure is a Q16 integer multiply — the gain constant is the only float, precomputed once (a scalar commutes with the cmx, shifts no hue). The SINGLE display clamp in the whole pipe follows the multiply: negative light and beyond-white cannot display, and the bare integer cast would wrap (a −1 shadow pixel would speckle full-white), so the clamp is the u16 container boundary, applied at the last possible moment — everything before it is signed and recoverable. Then the EV-independent sqrt LUT (64Ki sqrts once) maps to display bytes.
fn encode_pixels(lin: &[i32], ev: f32) -> Vec<u32> {
    const GAIN_SHIFT: u32 = 1 << 4;
    let gain = (2f64.powf(ev as f64) * (1u64 << GAIN_SHIFT) as f64).round() as i64;
    let mut lut = [0u32; 65536];
    for (v, out) in lut.iter_mut().enumerate() {
        *out = 255 - ((v as f32 / 65535.).sqrt() * 255.) as u32;
    }
    let mut pixels = Vec::with_capacity(lin.len() / 3);
    for px in lin.chunks_exact(3) {
        let ch = |v: i32| lut[((v as i64 * gain) >> GAIN_SHIFT).clamp(0, 65535) as usize];
        pixels.push(0xFF000000 | (ch(px[0]) << 16) | (ch(px[1]) << 8) | ch(px[2]));
    }
    pixels
}

/// Decode `path` (any supported format) into display pixels + panel tools + a title. Encoded at EV 0; the caller re-encodes if it's carrying exposure over.
fn load_image(path: &Path) -> Result<Loaded, String> {
    let dec = crate::convert::load_any(path)?;
    let (w, h, lin) = crate::convert::to_linear(&dec)?;
    let pixels = encode_pixels(&lin, 0.);
    let tools = PanelTools::build(&pixels, &lin, w, h, &Observer::stock());
    let title = format!(
        "opsin — {} ({}×{}, {} ch, {}-bit)",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        dec.img.width,
        dec.img.height,
        dec.img.channel_count(),
        dec.img.bit_depth()
    );
    Ok(Loaded { pixels, lin, w, h, tools, title })
}

/// Sorted list of supported images in `dir`.
fn folder_images(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| crate::convert::is_supported(p))
        .collect();
    v.sort();
    v
}

/// The folder image list + the index of `path` within it, for arrow navigation. A directory yields its images at index 0; a file yields its folder's images positioned on itself (inserted at front if the scan didn't catch it).
fn dir_list_for(path: &Path) -> (Vec<PathBuf>, usize) {
    if path.is_dir() {
        return (folder_images(path), 0);
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut list = folder_images(dir);
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match list.iter().position(|p| std::fs::canonicalize(p).map(|c| c == canon).unwrap_or(false)) {
        Some(i) => (list, i),
        None => {
            list.insert(0, path.to_path_buf());
            (list, 0)
        }
    }
}

impl OpsinApp {
    /// Start with no image — an empty drop target. Drag any supported file onto the window (or it arrives via `show_path`); the panel's locus/Planck chart renders from the observer alone.
    pub fn empty() -> Self {
        let loaded = Loaded {
            pixels: Vec::new(),
            lin: Vec::new(),
            w: 0,
            h: 0,
            tools: PanelTools::build(&[], &[], 0, 0, &Observer::stock()),
            title: "opsin — drop an image".to_string(),
        };
        Self::from_loaded(loaded, Vec::new(), 0)
    }

    /// Open a file (shown, folder siblings navigable) or a directory (first supported image shown).
    pub fn open(path: &Path) -> Result<Self, String> {
        let (dir_list, dir_idx) = dir_list_for(path);
        if dir_list.is_empty() {
            return Err(format!("{}: no supported images", path.display()));
        }

        let loaded = load_image(&dir_list[dir_idx])?;
        Ok(Self::from_loaded(loaded, dir_list, dir_idx))
    }

    fn from_loaded(loaded: Loaded, dir_list: Vec<PathBuf>, dir_idx: usize) -> Self {

        // App orb — three cone-fundamental lobes blooming warm/cool from centre (the observer the app is named for). Bundled as a 256×256 VSF; regenerate with `cargo run --bin make_orb` after editing the art. Decode failure is non-fatal — the chrome just runs orb-less.
        let orb = fluor::host::icon::Icon::from_vsf_bytes(include_bytes!("../assets/opsin_orb.vsf")).ok();

        let viewport = Viewport::new(1280, 800);
        let mut hit_counter: HitId = HIT_NONE;
        let chrome = DefaultChrome::new(viewport, loaded.title.clone(), orb, None, &mut hit_counter);
        // Geometry is placeholder — set_rect runs every frame from panel_rects.
        let ev_slider = fluor::widgets::Slider::new(&mut hit_counter, 0., 0., 1., 1., 0.5);
        let btn_one = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "1:1");
        let btn_fit = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Fit");

        Self {
            title: loaded.title,
            chrome,
            pixels: loaded.pixels,
            img_w: loaded.w,
            img_h: loaded.h,
            zoom_rel: 0.,
            cx_frac: 0.5,
            cy_frac: 0.5,
            drag: None,
            needs_fit: true,
            view_w: 1280,
            view_h: 800,
            panel_frac: 7. / (1 << 5) as f32,
            divider_drag: false,
            tools: loaded.tools,
            dir_list,
            dir_idx,
            lin: loaded.lin,
            ev: 0.,
            ev_slider,
            btn_one,
            btn_fit,
            ev_drag: false,
            nav_drag: false,
            hit_count: hit_counter,
            chord_lb_press: None,
            chord_lb_release: None,
            chord_rb_press: None,
            chord_rb_release: None,
            show_hitmask: false,
            debug_hit_colours: Vec::new(),
        }
    }

    /// True iff both `[` and `]` are currently held (photon's rule): pressed more recently than released, or released within the grace window (X11 fires a synthetic Release for a held key the instant another key goes down).
    fn brackets_held(&self, now: Instant) -> bool {
        fn key_held(press: Option<Instant>, release: Option<Instant>, now: Instant) -> bool {
            match (press, release) {
                (Some(p), Some(r)) => p > r || now.duration_since(r) < CHORD_RELEASE_GRACE,
                (Some(_), None) => true,
                _ => false,
            }
        }
        key_held(self.chord_lb_press, self.chord_lb_release, now)
            && key_held(self.chord_rb_press, self.chord_rb_release, now)
    }

    /// `[]`-chord debug toggles — photon's letters, minus its vault/session ones: h hitmask, a alpha cycle, p skip-premult, f fps strip, w damage outline, d screen decay, b opaque-scan tint, c skip-chrome, l skip-controls, r force-redraw. Returns true if the letter fired.
    fn handle_chord_action(&mut self, ac: char, ctx: &mut Context) -> bool {
        use std::sync::atomic::Ordering;
        let mut acted = true;
        match ac {
            'h' => {
                self.show_hitmask = !self.show_hitmask;
                paint::DEBUG_SHOW_HITMASK.store(self.show_hitmask, Ordering::Relaxed);
                eprintln!("[]h hitmask = {}", self.show_hitmask);
                if self.show_hitmask {
                    // xorshift32 seeded from the clock's subsecond nanos (pure entropy, not a timestamp — no epoch involved) → 256 random opaque RGBs stored in α+darkness. Fresh palette every toggle so distinct IDs always pop visually.
                    let seed = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(1))
                        | 1;
                    let mut s = seed;
                    self.debug_hit_colours.clear();
                    self.debug_hit_colours.reserve(256);
                    for _ in 0..256 {
                        let mut next = || {
                            s ^= s << 13;
                            s ^= s >> 17;
                            s ^= s << 5;
                            (s >> 16) & 0xFF
                        };
                        let (r, g, b) = (next(), next(), next());
                        let visible = (r << 16) | (g << 8) | b;
                        self.debug_hit_colours.push(0xFF000000 | (visible ^ 0x00FFFFFF));
                    }
                }
            }
            'a' => {
                // Cycle: off (0) → grayscale (1) → force-opaque (2) → off.
                let cur = paint::DEBUG_SHOW_ALPHA.load(Ordering::Relaxed);
                let next = (cur + 1) % 3;
                paint::DEBUG_SHOW_ALPHA.store(next, Ordering::Relaxed);
                let label = match next {
                    0 => "off",
                    1 => "grayscale",
                    _ => "force-opaque",
                };
                eprintln!("[]a show-alpha = {next} ({label})");
            }
            'p' => {
                let cur = paint::DEBUG_SKIP_PREMULT.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_PREMULT.store(!cur, Ordering::Relaxed);
                eprintln!("[]p skip-premult = {}", !cur);
            }
            'f' => {
                let cur = paint::DEBUG_SHOW_FPS.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_FPS.store(!cur, Ordering::Relaxed);
                eprintln!("[]f fps-strip = {}", !cur);
            }
            'w' => {
                let cur = paint::DEBUG_SHOW_DAMAGE.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_DAMAGE.store(!cur, Ordering::Relaxed);
                eprintln!("[]w damage-outline = {}", !cur);
            }
            'd' => {
                let cur = paint::DEBUG_SHOW_FADE.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_FADE.store(!cur, Ordering::Relaxed);
                eprintln!("[]d screen-decay = {}", !cur);
            }
            'b' => {
                let cur = paint::DEBUG_SHOW_OPAQUE_SCAN.load(Ordering::Relaxed);
                paint::DEBUG_SHOW_OPAQUE_SCAN.store(!cur, Ordering::Relaxed);
                eprintln!("[]b opaque-scan tint = {}", !cur);
            }
            'c' => {
                let cur = paint::DEBUG_SKIP_CHROME.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_CHROME.store(!cur, Ordering::Relaxed);
                self.chrome.invalidate_chrome();
                eprintln!("[]c skip-chrome = {}", !cur);
            }
            'l' => {
                let cur = paint::DEBUG_SKIP_CONTROLS.load(Ordering::Relaxed);
                paint::DEBUG_SKIP_CONTROLS.store(!cur, Ordering::Relaxed);
                self.chrome.invalidate_chrome();
                eprintln!("[]l skip-controls = {}", !cur);
            }
            'r' => {
                self.chrome.invalidate_bg();
                self.chrome.invalidate_chrome();
                eprintln!("[]r force-redraw");
            }
            _ => acted = false,
        }
        if acted {
            ctx.window.request_redraw();
        }
        acted
    }

    /// Center the main view on the navigator-space point under the cursor (clamped to the thumb, so dragging past the edge pins to the edge). The navigator IS the fraction space — the cursor's thumb fractions become the anchored composition directly.
    fn nav_center(&mut self, viewport: Viewport, cx: f32, cy: f32) {
        let (nx, ny, nw, nh) = self.panel_rects(viewport).nav;
        if nw == 0 || nh == 0 || self.zoom_rel <= 0. {
            return;
        }
        // Clamp justified: external drag input — the cursor can leave the thumb entirely; pinning to the edge is the intended behavior, not bug-hiding.
        self.cx_frac = ((cx - nx as f32) / nw as f32).clamp(0., 1.);
        self.cy_frac = ((cy - ny as f32) / nh as f32).clamp(0., 1.);
    }

    /// Apply the slider's 0..1 value as stops and re-encode the display pixels. The panel thumbnail tracks (cheap); histogram/scatter stay on the un-exposed linear data — they describe the capture, not the view.
    fn apply_ev(&mut self, value01: f32, ctx: &mut Context) {
        let ev = (value01 * 2. - 1.) * EV_RANGE;
        if (ev - self.ev).abs() < 1e-4 || self.lin.is_empty() {
            return;
        }
        self.ev = ev;
        self.pixels = encode_pixels(&self.lin, ev);
        self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
        ctx.window.request_redraw();
    }

    /// Swap the decoded image into the view: title, pixels, dims, panel tools, redraw. Same dimensions (and no rotation model yet, so dims are the whole test) ⇒ the view state carries over — pan, zoom, and exposure stay put so stepping through a burst or LED sequence compares like with like. Different dimensions ⇒ refit and reset exposure.
    fn install(&mut self, loaded: Loaded, ctx: &mut Context) {
        let same_geometry = loaded.w == self.img_w && loaded.h == self.img_h && self.img_w > 0;
        self.chrome.set_title(&loaded.title);
        self.title = loaded.title;
        self.img_w = loaded.w;
        self.img_h = loaded.h;
        self.tools = loaded.tools;
        self.lin = loaded.lin;
        if same_geometry && self.ev.abs() > 1e-4 {
            // Carry the exposure into the new frame (loaded.pixels were encoded at EV 0).
            self.pixels = encode_pixels(&self.lin, self.ev);
            self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
        } else {
            self.pixels = loaded.pixels;
        }
        if !same_geometry {
            self.ev = 0.;
            self.ev_slider.set_value(0.5);
            self.fit(ctx.viewport);
        }
        ctx.window.request_redraw();
    }

    /// Step `delta` (±1) through the folder, skipping images that fail to decode. Reloads the view and refits.
    fn navigate(&mut self, delta: isize, ctx: &mut Context) {
        let n = self.dir_list.len();
        if n <= 1 {
            return;
        }
        let mut idx = self.dir_idx;
        for _ in 0..n {
            idx = ((idx as isize + delta).rem_euclid(n as isize)) as usize;
            match load_image(&self.dir_list[idx]) {
                Ok(loaded) => {
                    self.dir_idx = idx;
                    self.install(loaded, ctx);
                    return;
                }
                Err(e) => eprintln!("opsin: {}: {e}", self.dir_list[idx].display()),
            }
        }
    }

    /// Open a path dropped onto the window: rebuild the folder list around it so arrow-nav works from there, then show it. Unsupported / undecodable drops are logged and ignored (current image stays).
    fn show_path(&mut self, path: &Path, ctx: &mut Context) {
        match load_image(path) {
            Ok(loaded) => {
                let (list, idx) = dir_list_for(path);
                self.dir_list = list;
                self.dir_idx = idx;
                self.install(loaded, ctx);
            }
            Err(e) => eprintln!("opsin: {}: {e}", path.display()),
        }
    }

    /// Convert the current image to a VSF-Image beside the source (`<stem>.vsf`). Returns the written path for the caller to surface.
    fn convert_current_to_vsf(&self) -> Result<PathBuf, String> {
        let Some(src) = self.dir_list.get(self.dir_idx) else {
            return Err("no image loaded".to_string());
        };
        if src.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("vsf")).unwrap_or(false) {
            return Err("already a VSF image".to_string());
        }
        let out = src.with_extension("vsf");
        let mut dec = crate::convert::load_any(src)?;
        // Record the live exposure as a Technical view op (a scalar shifts no hue) — the translateration log, replayed on reopen. EV 0 leaves the section omitted.
        if self.ev.abs() > 1e-4 {
            dec.img.view = Some(vsf::spectral_image::ViewTransform {
                space: "vsf_rgb_linear".to_string(),
                ops: vec![vsf::spectral_image::ViewOp {
                    name: "exposure".to_string(),
                    class: vsf::spectral_image::IdtClass::Technical,
                    params: vec![self.ev],
                }],
            });
        }
        crate::convert::write_vsf(&dec.img, &out)?;
        Ok(out)
    }

    /// Screen x of the panel divider (left edge of the panel).
    fn divider_x(&self, viewport: Viewport) -> f32 {
        viewport.width_px as f32 * (1. - self.panel_frac)
    }

    /// Divider grab band half-width — half the resize band, RU-scaled like every other affordance (no pixel constants).
    fn divider_grab(viewport: Viewport) -> f32 {
        Self::strip_h(viewport) / 8.0
    }

    /// Image area geometry — the region left of the divider, below the top bar: (width, height, bar) in screen px, live from the viewport.
    fn image_area(&self, viewport: Viewport) -> (f32, f32, f32) {
        let bar = Self::strip_h(viewport);
        (self.divider_x(viewport), viewport.height_px as f32 - bar, bar)
    }

    /// Harmonic-mean span of the image area, 2wh/(w+h) — the universal scaling base (smooth in both dims, biased toward the smaller one).
    fn area_span(aw: f32, ah: f32) -> f32 {
        2. * aw * ah / (aw + ah)
    }

    /// The view transform in screen pixels — (zoom, ox, oy) — derived fresh from the live viewport at every use. Nothing pixel-valued is ever stored: zoom = zoom_rel × area span, and (ox, oy) place the anchored image fraction at the area centre. This derivation is what makes every window op (edge resize, divider drag, maximize) scale the composition continuously — C¹⁺ in the window dims because the span is.
    fn view_px(&self, viewport: Viewport) -> (f32, f32, f32) {
        let (aw, ah, bar) = self.image_area(viewport);
        let zoom = self.zoom_rel * Self::area_span(aw, ah);
        let ox = aw * 0.5 - self.cx_frac * self.img_w as f32 * zoom;
        let oy = bar + ah * 0.5 - self.cy_frac * self.img_h as f32 * zoom;
        (zoom, ox, oy)
    }

    /// Fit the image inside the image area with a small margin and center it — a one-shot that SETS the relative composition. Because the transform is span-relative, the fitted composition then rides every resize on its own; the contain-min here runs only at this moment (load, F, Fit button), never per-frame, so it puts no kink in the resize response. No-op in the empty drop-target state.
    fn fit(&mut self, viewport: Viewport) {
        if self.img_w == 0 || self.img_h == 0 {
            return;
        }
        let (aw, ah, _) = self.image_area(viewport);
        let zoom = (aw / self.img_w as f32).min(ah / self.img_h as f32) * (1. - 1. / (1 << 6) as f32);
        self.zoom_rel = zoom / Self::area_span(aw, ah);
        self.cx_frac = 0.5;
        self.cy_frac = 0.5;
    }

    /// 1:1 — one image pixel per screen pixel, EXACTLY: stores `zoom_rel = 1/span` bitwise so the readout's `==` test can certify pixel-exactness (and lose it the moment a resize changes the span). Zooms about the image-area centre for free — the anchored fractions ARE the centre point, so changing only the scale is a centre zoom by construction.
    fn one_to_one(&mut self, viewport: Viewport) {
        let (aw, ah, _) = self.image_area(viewport);
        self.zoom_rel = 1. / Self::area_span(aw, ah);
    }

    /// Is this screen point on the drawn image itself (as opposed to backdrop)?
    fn on_image(&self, x: f32, y: f32, viewport: Viewport) -> bool {
        let (zoom, ox, oy) = self.view_px(viewport);
        x >= ox && y >= oy && x < ox + self.img_w as f32 * zoom && y < oy + self.img_h as f32 * zoom
    }

    /// Rescale around a screen-space anchor so the image point under the cursor stays put — computed in derived pixel space, stored back as relative state. Zoom is unbounded — the blit cost is capped by screen area at any zoom, and the wheel factor is strictly positive so zoom can't reach 0 (the old clamp was defensive theater).
    fn zoom_around(&mut self, factor: f32, ax: f32, ay: f32, viewport: Viewport) {
        let (zoom, ox, oy) = self.view_px(viewport);
        let (aw, ah, bar) = self.image_area(viewport);
        let zoom2 = zoom * factor;
        let ox2 = ax - (ax - ox) * factor;
        let oy2 = ay - (ay - oy) * factor;
        self.zoom_rel = zoom2 / Self::area_span(aw, ah);
        self.cx_frac = (aw * 0.5 - ox2) / (self.img_w as f32 * zoom2);
        self.cy_frac = (bar + ah * 0.5 - oy2) / (self.img_h as f32 * zoom2);
    }

    /// Full-width top bar height — matches `DefaultChrome`'s controls strip exactly (shared source of truth) so opsin's bottom hairline lines up with the chrome strip's.
    fn strip_h(viewport: Viewport) -> f32 {
        chrome::strip_height(viewport)
    }

    // Resize classification comes from `chrome::get_resize_edge`, which since the RU-coherence fix uses a band of a quarter strip height (`ceil(effective_span/32)/4`) on all four edges — the bar's top quarter resizes, the rest is the move handle, and the band scales with RU zoom everywhere.

    /// Panel section rects (x0, y0, w, h) in pixels — named, not position-coded. Stacked top-down inside the panel with uniform padding; each section keeps its natural aspect (navigator = image aspect, histogram = 2:1, buttons/slider = thin bands, chart = square) and the stack just runs off the bottom on short windows.
    fn panel_rects(&self, viewport: Viewport) -> PanelRects {
        let vw = viewport.width_px as usize;
        let vh = viewport.height_px as usize;
        let dx = self.divider_x(viewport) as usize;
        let pad = (viewport.effective_span() / (1 << 7) as f32).ceil() as usize;
        let band = (viewport.effective_span() / (1 << 6) as f32).ceil() as usize;
        let x0 = (dx + 1 + pad).min(vw);
        let w = vw.saturating_sub(x0 + pad);
        let mut y = (Self::strip_h(viewport) as usize + pad).min(vh);

        let nav_h = if self.img_w > 0 { (w * self.img_h / self.img_w).min(vh / 3) } else { 0 };
        let nav = (x0, y, w, nav_h);
        y += nav_h + pad;

        let btn_h = if self.img_w > 0 { band } else { 0 };
        let btns = (x0, y.min(vh), w, btn_h);
        y += btn_h + pad;

        let hist_h = (w / 2).min(vh / 4);
        let hist = (x0, y.min(vh), w, hist_h);
        y += hist_h + pad;

        let ev = (x0, y.min(vh), w, band);
        y += band + pad;

        let chart_h = w.min(vh.saturating_sub(y + pad));
        let chart = (x0, y.min(vh), w, chart_h);
        PanelRects { nav, btns, hist, ev, chart }
    }

    /// If (cx, cy) lands in the navigator's drawn thumbnail, return the image coords it points at.
    fn nav_hit(&self, viewport: Viewport, cx: f32, cy: f32) -> Option<(f32, f32)> {
        let (nx, ny, nw, nh) = self.panel_rects(viewport).nav;
        if nw == 0 || nh == 0 {
            return None;
        }
        let fx = (cx - nx as f32) / nw as f32;
        let fy = (cy - ny as f32) / nh as f32;
        if !(0.0..1.).contains(&fx) || !(0.0..1.).contains(&fy) {
            return None;
        }
        Some((fx * self.img_w as f32, fy * self.img_h as f32))
    }
}

/// Under-compose one pixel, bounds-unchecked by construction (callers clamp rects to the buffer).
#[inline]
fn put(target: &mut [u32], buf_w: usize, x: usize, y: usize, colour: u32) {
    let i = y * buf_w + x;
    target[i] = target[i].under(colour, BlendMode::Normal);
}

/// Plot a polyline as densely-interpolated 1px dots — fine for the panel's small charts, no AA pretensions yet.
fn plot_polyline(target: &mut [u32], buf_w: usize, pts: &[(f32, f32)], rect: (usize, usize, usize, usize), colour: u32) {
    let (rx, ry, rw, rh) = rect;
    if rw == 0 || rh == 0 {
        return;
    }
    let to_px = |p: (f32, f32)| (rx as f32 + p.0 * (rw - 1) as f32, ry as f32 + p.1 * (rh - 1) as f32);
    for pair in pts.windows(2) {
        let (x0, y0) = to_px(pair[0]);
        let (x1, y1) = to_px(pair[1]);
        let steps = ((x1 - x0).abs().max((y1 - y0).abs()).ceil() as usize).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = (x0 + (x1 - x0) * t) as usize;
            let y = (y0 + (y1 - y0) * t) as usize;
            if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
                put(target, buf_w, x, y, colour);
            }
        }
    }
}

impl Container for OpsinApp {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn fluor::host::widget::Widget)) {
        self.chrome.visit(f);
        f(&mut self.btn_one);
        f(&mut self.btn_fit);
    }
}

impl FluorApp for OpsinApp {
    type UserEvent = ();

    fn title(&self) -> &str {
        &self.title
    }

    /// OS window icon (taskbar / alt-tab). The host applies this at window creation — Windows/X11 only; Wayland/macOS source the icon from packaging. Same orb the chrome draws.
    fn window_icon(&self) -> Option<&fluor::host::icon::Icon> {
        self.chrome.app_icon.as_ref()
    }

    fn init(&mut self, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.view_w = ctx.viewport.width_px as usize;
        self.view_h = ctx.viewport.height_px as usize;
        // No fit here — init runs against the guessed pre-surface viewport; fit mode applies at first render with real dims.
    }

    fn on_resize(&mut self, w: u32, h: u32, ctx: &mut Context) {
        self.chrome.resize(ctx.viewport);
        self.view_w = w as usize;
        self.view_h = h as usize;
        self.chrome.set_full_edge(ctx.is_maximized);
        // Nothing view-related to do: the transform is span-relative and derived from the live viewport at render, so the composition rides the resize by construction.
    }

    fn on_event(&mut self, event: &FEvent, ctx: &mut Context) -> EventResponse {
        match event {
            FEvent::CursorMoved { .. } => {
                // Use ctx.cursor_x/y (window-relative) — the event's own x/y are raw screen coords, offset by the window origin in the fullscreen-compositor model, so they'd desync pan/divider/hover from everything else (chrome hit-test, image blit) which all work in window space.
                let (cx, cy) = (ctx.cursor_x, ctx.cursor_y);
                if self.ev_drag {
                    self.ev_slider.set_value_from_x(cx);
                    let v = self.ev_slider.value();
                    self.apply_ev(v, ctx);
                    return EventResponse::Handled;
                }
                if self.nav_drag {
                    self.nav_center(ctx.viewport, cx, cy);
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                if self.divider_drag {
                    // Clamp justified: external user input (a drag can leave the window entirely); the stops keep both the image area and the panel usable. 1/8 .. 1/2 of window width.
                    self.panel_frac = (1. - cx / ctx.viewport.width_px as f32).clamp(1. / (1 << 3) as f32, 1. / (1 << 1) as f32);
                    // The divider moves the pill buttons without dirtying the chrome layer, which would leave stale hit stamps at pre-drag positions (photon's scroll lesson) — invalidate so the map wipes and re-stamps against this frame's geometry.
                    self.chrome.invalidate_chrome();
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                if let Some((lx, ly)) = self.drag {
                    // Pan: shift the anchored fraction by the cursor delta in image-fraction space. Guarded by construction — drag only starts on_image, so img dims and zoom are nonzero.
                    let (zoom, _, _) = self.view_px(ctx.viewport);
                    self.cx_frac -= (cx - lx) / (self.img_w as f32 * zoom);
                    self.cy_frac -= (cy - ly) / (self.img_h as f32 * zoom);
                    self.drag = Some((cx, cy));
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                let hit = self.chrome.hit_at(cx, cy);
                let mut dirty = self.chrome.set_hover(hit);
                // Pill button hover — driven by the same stamped hit map as the chrome controls.
                for b in [&mut self.btn_one, &mut self.btn_fit] {
                    let over = hit == b.hit_id();
                    if b.is_hovered() != over {
                        b.set_hovered(over);
                        dirty = true;
                    }
                }
                if dirty {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::CursorLeft => {
                if self.chrome.set_hover(HIT_NONE) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left } => {
                let hit = self.chrome.hit_at(ctx.cursor_x, ctx.cursor_y);
                if hit != HIT_NONE {
                    // Chrome button (close/min/max/orb) — dispatch thru the Container walk, same as panes.
                    let (x, y, mods) = (ctx.cursor_x, ctx.cursor_y, ctx.modifiers);
                    let mut response = EventResponse::Pass;
                    self.visit(&mut |w| {
                        if w.id() == hit {
                            if let Some(c) = w.click() {
                                response = c.on_click(x, y, mods);
                            }
                        }
                    });
                    // The pill buttons fire thru the same walk (they only bump a counter) — poll and act here.
                    if self.btn_one.take_click() {
                        // 1:1 — a MOMENT: exactly one image pixel per screen pixel right now; the composition then scales relatively like everything else.
                        self.one_to_one(ctx.viewport);
                        ctx.window.request_redraw();
                    }
                    if self.btn_fit.take_click() {
                        // Fit — a MOMENT too: sets the composition to whole-image-centered; the span-relative transform carries it thru resizes on its own.
                        self.fit(ctx.viewport);
                        ctx.window.request_redraw();
                    }
                    return response;
                }
                let edge = chrome::get_resize_edge(ctx.viewport, ctx.cursor_x, ctx.cursor_y);
                if edge != ResizeEdge::None {
                    return EventResponse::StartResize(edge);
                }
                if ctx.cursor_y < Self::strip_h(ctx.viewport) {
                    return EventResponse::StartWindowDrag;
                }
                if (ctx.cursor_x - self.divider_x(ctx.viewport)).abs() <= Self::divider_grab(ctx.viewport) {
                    self.divider_drag = true;
                    return EventResponse::Handled;
                }
                if ctx.cursor_x > self.divider_x(ctx.viewport) {
                    let rects = self.panel_rects(ctx.viewport);
                    let hit_rect = |r: (usize, usize, usize, usize)| {
                        r.2 > 0
                            && ctx.cursor_x >= r.0 as f32
                            && ctx.cursor_x < (r.0 + r.2) as f32
                            && ctx.cursor_y >= r.1 as f32
                            && ctx.cursor_y < (r.1 + r.3) as f32
                    };
                    // 1:1 / Fit presses never reach here — the pills stamp the chrome hit map, so they dispatch thru the hit != HIT_NONE arm above. The band's remaining space (magnification readout, corners) is panel dead space and falls thru to the window drag below.
                    // Exposure slider — press anywhere on the band jumps the handle there and starts the drag.
                    if hit_rect(rects.ev) && !self.lin.is_empty() {
                        self.ev_drag = true;
                        self.ev_slider.set_value_from_x(ctx.cursor_x);
                        let v = self.ev_slider.value();
                        self.apply_ev(v, ctx);
                        return EventResponse::Handled;
                    }
                    // Navigator press → live drag: center immediately and keep re-centering on every cursor move until release. Panel dead space (gaps, section padding) moves the window — the hand is already there when arranging the workspace.
                    if self.nav_hit(ctx.viewport, ctx.cursor_x, ctx.cursor_y).is_some() {
                        self.nav_drag = true;
                        self.nav_center(ctx.viewport, ctx.cursor_x, ctx.cursor_y);
                        ctx.window.request_redraw();
                        return EventResponse::Handled;
                    }
                    return EventResponse::StartWindowDrag;
                }
                if self.on_image(ctx.cursor_x, ctx.cursor_y, ctx.viewport) {
                    self.drag = Some((ctx.cursor_x, ctx.cursor_y));
                    return EventResponse::Handled;
                }
                // Backdrop (letterbox margin) — move the window, panes convention.
                EventResponse::StartWindowDrag
            }
            FEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left } => {
                self.drag = None;
                self.divider_drag = false;
                self.ev_drag = false;
                self.nav_drag = false;
                EventResponse::Pass
            }
            FEvent::MouseWheel { delta } => {
                // Trackpad pixel deltas: a step's worth of travel is span/(1<<6) — ≈21 px on a 1920×1080 window, the legacy photon "20 px" feel, but derived from the display instead of hardcoded (no fixed pixels). Bare span, not effective_span: feed sensitivity must not compound with the zoom being adjusted.
                let steps = match delta {
                    MouseScrollDelta::Lines(_, y) => *y,
                    MouseScrollDelta::Pixels(_, y) => y / (ctx.viewport.span / (1 << 6) as f32),
                };
                // The ecosystem zoom curve, inherited from fluor — asymmetric BY DESIGN (in ×32/31, out ×32/33; incommensurate, so notch combos are dense and any zoom is reachable). No local curve, no override: one wheel language everywhere.
                self.zoom_around(fluor::geom::zoom_step_factor(steps), ctx.cursor_x, ctx.cursor_y, ctx.viewport);
                ctx.window.request_redraw();
                EventResponse::Handled
            }
            FEvent::Focused(focused) => {
                // Forward to the chrome so the title colour + orb ring/darken track focus (the tint logic lives in DefaultChrome, driven by its `focused` flag — it just needs to be told).
                if self.chrome.set_focused(*focused) {
                    ctx.window.request_redraw();
                }
                EventResponse::Pass
            }
            FEvent::DroppedFile(path) => {
                self.show_path(Path::new(path), ctx);
                EventResponse::Handled
            }
            FEvent::KeyboardInput { event } => {
                // Bracket chord first, on BOTH press and release (photon's scheme) — the debug action must fire before normal key routing so a chord letter doesn't also trigger its app binding.
                if let Key::Character(c) = &event.logical_key {
                    let cs = c.as_str();
                    let now = Instant::now();
                    let mut action_char: Option<char> = None;
                    match (cs, event.state) {
                        ("[", ElementState::Pressed) => self.chord_lb_press = Some(now),
                        ("[", ElementState::Released) => self.chord_lb_release = Some(now),
                        ("]", ElementState::Pressed) => self.chord_rb_press = Some(now),
                        ("]", ElementState::Released) => self.chord_rb_release = Some(now),
                        (_, ElementState::Pressed) if !event.repeat => {
                            if self.brackets_held(now) {
                                action_char = c.to_ascii_lowercase().chars().next();
                            }
                        }
                        _ => {}
                    }
                    if let Some(ac) = action_char {
                        if self.handle_chord_action(ac, ctx) {
                            return EventResponse::Handled;
                        }
                    }
                }
                if event.state != ElementState::Pressed {
                    return EventResponse::Pass;
                }
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) => EventResponse::Close,
                    Key::Named(NamedKey::ArrowLeft) => {
                        self.navigate(-1, ctx);
                        EventResponse::Handled
                    }
                    Key::Named(NamedKey::ArrowRight) => {
                        self.navigate(1, ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                        match self.convert_current_to_vsf() {
                            Ok(out) => println!("opsin: wrote {}", out.display()),
                            Err(e) => eprintln!("opsin: convert to VSF failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("f") => {
                        self.fit(ctx.viewport);
                        ctx.window.request_redraw();
                        EventResponse::Handled
                    }
                    Key::Character(c) if c == "1" => {
                        // 1:1 pixels, about the image-area centre — same exact moment as the button.
                        self.one_to_one(ctx.viewport);
                        ctx.window.request_redraw();
                        EventResponse::Handled
                    }
                    // Exposure: +/− nudge a third of a stop, 0 resets.
                    Key::Character(c) if c == "+" || c == "=" || c == "-" => {
                        let delta = if c == "-" { -1. / 3. } else { 1. / 3. };
                        let v = (((self.ev + delta).clamp(-EV_RANGE, EV_RANGE)) + EV_RANGE) / (2. * EV_RANGE);
                        self.ev_slider.set_value(v);
                        self.apply_ev(v, ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if c == "0" => {
                        self.ev_slider.set_value(0.5);
                        self.apply_ev(0.5, ctx);
                        EventResponse::Handled
                    }
                    _ => EventResponse::Pass,
                }
            }
            _ => EventResponse::Pass,
        }
    }

    fn render(&mut self, target: &mut [u32], ctx: &mut Context) {
        // Queued load-time fit — render is the first callback guaranteed a real (post-surface) viewport.
        if self.needs_fit {
            self.needs_fit = false;
            self.fit(ctx.viewport);
        }
        // The whole image transform for this frame, derived from the live viewport — the ONLY place pixel values exist, and they exist for exactly one frame.
        let (zoom, img_ox, img_oy) = self.view_px(ctx.viewport);
        let buf_w = ctx.viewport.width_px as usize;
        let buf_h = ctx.viewport.height_px as usize;
        let clip = Some(Clip::new(ctx.damage_clip.x0, ctx.damage_clip.y0, ctx.damage_clip.x1, ctx.damage_clip.y1));

        // Front-to-back under-blend: perimeter hairline first (must own the window edge), then chrome controls, then panel content, then the image composes under those, then the backdrop under everything.
        self.chrome.rasterize_perimeter(target, buf_w, buf_h, ctx.clip_mask);
        self.chrome.rasterize_chrome(ctx.damage, ctx.text, ctx.clip_mask);
        self.chrome.flatten_into(target, buf_w, buf_h, clip);

        // ── Top bar ── full-width, Photon's horizontal-streak noise texture (composes UNDER the already-flattened chrome, so orb/title/controls stay on top). Base toned to the controls-strip grey so the textured area and the flat control fill read as one bar. The bar is the canonical window-move handle (and future menu home).
        let bar_h = Self::strip_h(ctx.viewport) as usize;
        {
            let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
            let bar_clip = Clip::new(0, 0, buf_w, bar_h.min(buf_h));
            paint::background_noise(&mut canvas, 0, true, 0, Some(bar_clip), Some(BAR_TEXTURE_BASE));
            paint::fill_rect(&mut canvas, 0, bar_h as isize, buf_w as isize, 0, HAIRLINE, clip, None);
        }

        // ── Right tool panel ──
        let divider_px = (self.divider_x(ctx.viewport) as usize).min(buf_w.saturating_sub(1));
        let PanelRects { nav, btns, hist, ev: ev_rect, chart } = self.panel_rects(ctx.viewport);
        {
            let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
            // Divider — 1px vertical hairline from the bar down (fill_rect's 0-width hairline convention).
            paint::fill_rect(&mut canvas, divider_px as isize, bar_h as isize, 0, (buf_h - bar_h) as isize, HAIRLINE, clip, None);
            // Section separators under nav-buttons and hist.
            for &(sx, sy, sw, sh) in &[btns, hist] {
                paint::fill_rect(&mut canvas, sx as isize, (sy + sh) as isize + 4, sw as isize, 0, HAIRLINE, clip, None);
            }
            // 1:1 / magnification / Fit — the band splits in thirds: two fluor pill Buttons (same widget family as the slider and chrome) flanking the LIVE magnification readout (screen px per image px: 1x = pixel-exact certificate, 2.00x = zoomed in past it). The readout derives from the same per-frame span-relative transform as the blit, so it tracks every window op — press 1:1, resize, and it honestly drifts; that's the relative model reporting itself.
            let (bx, by, bw, bh) = btns;
            if bw > 2 && bh > 0 && self.img_w > 0 {
                let third = bw as f32 / 3.;
                let font = bh as f32 / 2.;
                let bcy = by as f32 + bh as f32 / 2.;
                self.btn_one.set_rect(bx as f32 + third / 2., bcy, third, bh as f32);
                self.btn_one.set_font_size(font);
                self.btn_fit.set_rect(bx as f32 + bw as f32 - third / 2., bcy, third, bh as f32);
                self.btn_fit.set_font_size(font);
                let id = self.btn_one.hit_id();
                self.btn_one.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                let id = self.btn_fit.hit_id();
                self.btn_fit.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                // "1x" is a CERTIFICATE, not a rounding: bitwise == against the value one_to_one() stored, so it holds iff the span (window + divider geometry) is unchanged since — the moment a resize makes the image resample, equality breaks and the decimals return. Resize back to the identical geometry and exactness honestly comes back.
                let (aw, ah, _) = self.image_area(ctx.viewport);
                let magnification = if self.zoom_rel == 1. / Self::area_span(aw, ah) {
                    "1x".to_string()
                } else {
                    // TRUNCATED to two decimals, never rounded — the readout may understate but never overstate: 0.9999999 reads "0.99x" (it is NOT yet 1; only the == certificate may say "1x"). Integer decomposition so the formatter can't re-round.
                    let centi = (zoom * 100.).trunc() as u64;
                    format!("{}.{:02}x", centi / 100, centi % 100)
                };
                ctx.text.draw_text_center_u32(&mut canvas, &magnification, (bx + bw / 2) as f32, (by + bh / 2) as f32, font, 400, TEXT_GREY, "Open Sans", clip, None, None);
            }
            // Exposure slider + EV label. Label takes the band's left end, the slider the rest; the fluor Slider paints the lumis-style white/black track + circular handle.
            let (ex, ey, ew, eh) = ev_rect;
            if ew > 0 && eh > 0 && !self.lin.is_empty() {
                let label_w = eh * 4;
                let font = eh as f32 * (7. / (1 << 3) as f32);
                // Truncated toward zero, never rounded — same integer decomposition as the magnification readout, so the label shows the stored value's truth: three 1/3-stop nudges display "+0.99" because the float accumulation genuinely is a hair under a stop.
                let centi = (self.ev * 100.).trunc() as i32;
                let label = format!("{}{}.{:02}", if centi < 0 { '-' } else { '+' }, (centi / 100).abs(), (centi % 100).abs());
                ctx.text.draw_text_left_u32(&mut canvas, &label, ex as f32, (ey + eh / 2) as f32, font, 400, TEXT_GREY, "Open Sans", clip, None, None);
                let sw = ew.saturating_sub(label_w);
                if sw > eh {
                    self.ev_slider.set_rect((ex + label_w + sw / 2) as f32, (ey + eh / 2) as f32, sw as f32, eh as f32);
                    let id = self.ev_slider.hit_id();
                    self.ev_slider.render_content_into(&mut canvas, None, id);
                }
            }

            // Histogram bars — per-column, three translucent channel segments composing additively. Bins are oversampled (HIST_BINS); each column takes the PEAK over its bin range so narrow spikes survive the downsample (oriel's oversample idea).
            let (hx, hy, hw, hh) = hist;
            if hw > 0 && hh > 0 {
                for col in 0..hw {
                    let lo = col * HIST_BINS / hw;
                    let hi = (((col + 1) * HIST_BINS / hw).max(lo + 1)).min(HIST_BINS);
                    for (ch, colour) in [HIST_R, HIST_G, HIST_B].iter().enumerate() {
                        let v = self.tools.hist[lo..hi].iter().map(|b| b[ch]).fold(0f32, f32::max);
                        let bar = (v * hh as f32) as usize;
                        if bar > 0 {
                            paint::fill_rect(&mut canvas, (hx + col) as isize, (hy + hh - bar) as isize, 0, bar as isize, *colour, clip, None);
                        }
                    }
                }
            }
        }
        // Navigator thumbnail — nearest blit into the nav rect (direct writes).
        let (nx, ny, nw, nh) = nav;
        if nw > 0 && nh > 0 {
            for ty in 0..nh.min(buf_h.saturating_sub(ny)) {
                let sy = ty * self.tools.thumb_h / nh;
                for tx in 0..nw.min(buf_w.saturating_sub(nx)) {
                    let sx = tx * self.tools.thumb_w / nw;
                    put(target, buf_w, nx + tx, ny + ty, self.tools.thumb[sy * self.tools.thumb_w + sx]);
                }
            }
        }
        // Navigator view rect — the TRUE viewport rect in thumb space, unclamped (it slides off the thumb edge when panned past the image instead of shrinking and sticking), drawn AFTER the thumbnail as a wrapping-add-of-128 marker on each gamma-encoded RGB byte (`b ^ 0x80` — self-contrasting on any content).
        if nw > 0 && nh > 0 && zoom > 0. && self.img_w > 0 {
            let img_area_w = self.divider_x(ctx.viewport);
            let sx = nw as f32 / (self.img_w as f32 * zoom);
            let sy = nh as f32 / (self.img_h as f32 * zoom);
            let rx0 = nx as isize + ((0. - img_ox) * sx) as isize;
            let ry0 = ny as isize + ((bar_h as f32 - img_oy) * sy) as isize;
            let rx1 = nx as isize + ((img_area_w - img_ox) * sx) as isize;
            let ry1 = ny as isize + ((buf_h as f32 - img_oy) * sy) as isize;
            let (cx0, cy0) = (nx as isize, ny as isize);
            let (cx1, cy1) = ((nx + nw) as isize, (ny + nh) as isize);
            let mut mark = |x: isize, y: isize| {
                if x >= cx0 && x < cx1 && y >= cy0 && y < cy1 {
                    let i = y as usize * buf_w + x as usize;
                    target[i] ^= 0x0080_8080;
                }
            };
            for x in rx0.max(cx0)..rx1.min(cx1) {
                mark(x, ry0);
                mark(x, ry1);
            }
            for y in (ry0 + 1).max(cy0)..ry1.min(cy1) {
                mark(rx0, y);
                mark(rx1, y);
            }
        }
        // Chromaticity chart — locus + Planckian arc lines topmost, the tinted lobe (chromaticity colour × density brightness, precomputed per observer+image) composing under them.
        let (cx, cy, cw, chh) = chart;
        if cw > 0 && chh > 0 {
            plot_polyline(target, buf_w, &self.tools.locus, chart, LOCUS);
            if self.tools.locus.len() >= 2 {
                let closure = [self.tools.locus[self.tools.locus.len() - 1], self.tools.locus[0]];
                plot_polyline(target, buf_w, &closure, chart, LOCUS);
            }
            plot_polyline(target, buf_w, &self.tools.blackbody, chart, PLANCK);
            for py in 0..chh.min(buf_h.saturating_sub(cy)) {
                let gy = py * CHART_RES / chh;
                for px in 0..cw.min(buf_w.saturating_sub(cx)) {
                    let gx = px * CHART_RES / cw;
                    let v = self.tools.chart[gy * CHART_RES + gx];
                    if v != 0 {
                        put(target, buf_w, cx + px, cy + py, v);
                    }
                }
            }
        }
        // Panel background under all tool content.
        {
            let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
            paint::fill_rect(&mut canvas, (divider_px + 1) as isize, 0, (buf_w - divider_px - 1) as isize, buf_h as isize, PANEL_BG, clip, None);
        }

        // Nearest-neighbour blit of the image rect ∩ image area (left of the divider, below the bar). Per-row source index precomputed once; per-pixel work is one under() compose.
        let x0 = img_ox.max(0.) as usize;
        let y0 = (img_oy.max(0.) as usize).max(bar_h);
        let x1 = ((img_ox + self.img_w as f32 * zoom) as usize).min(divider_px);
        let y1 = ((img_oy + self.img_h as f32 * zoom) as usize).min(buf_h);
        for sy in y0..y1 {
            let iy = ((sy as f32 - img_oy) / zoom) as usize;
            if iy >= self.img_h {
                continue;
            }
            let src_row = iy * self.img_w;
            let dst_row = sy * buf_w;
            for sx in x0..x1 {
                let ix = ((sx as f32 - img_ox) / zoom) as usize;
                if ix >= self.img_w {
                    continue;
                }
                target[dst_row + sx] = target[dst_row + sx].under(self.pixels[src_row + ix], BlendMode::Normal);
            }
        }

        // Backdrop under everything else.
        for px in target.iter_mut().take(buf_w * buf_h) {
            *px = px.under(BACKDROP, BlendMode::Normal);
        }

        // Hit-mask overlay ([]h) — every pixel replaced by its hit id's palette colour, drawn LAST so it shows exactly what hit_at returns. `.get` justified: the map can hold stale stamps at ids past the 256-colour palette; unknown ids render transparent instead of panicking a debug view.
        if self.show_hitmask && !self.debug_hit_colours.is_empty() {
            let map = &self.chrome.hit_test_map;
            let n = map.len().min(target.len());
            for i in 0..n {
                target[i] = self.debug_hit_colours.get(map[i] as usize).copied().unwrap_or(0);
            }
        }
    }

    fn hit_test_map(&self) -> Option<(&[fluor::paint::HitId], usize, usize)> {
        Some((&self.chrome.hit_test_map, self.view_w, self.view_h))
    }

    /// The table the host actually PAINTS hover from — set_hovered alone is state; this pipe is what puts the tint on screen. One Container walk: chrome buttons + the pills each contribute their tint at their id's slot.
    fn overlay_deltas(&mut self) -> Vec<u32> {
        let count = self.hit_count as usize + 1;
        fluor::host::widget::build_overlay_deltas(self, count)
    }

    /// Parallel bbox table so the host bounds each tint scan to the widget's rect instead of the whole window.
    fn overlay_bboxes(&mut self, viewport_w: usize, viewport_h: usize) -> Vec<Option<fluor::canvas::PixelRect>> {
        let count = self.hit_count as usize + 1;
        fluor::host::widget::build_overlay_bboxes(self, count, viewport_w, viewport_h)
    }

    fn cursor_for(&self, x: Coord, y: Coord, ctx: &Context) -> CursorIcon {
        // The OS horizontal arrows on the divider band (and while dragging it, wherever the cursor is).
        if self.divider_drag || (x - self.divider_x(ctx.viewport)).abs() <= Self::divider_grab(ctx.viewport) {
            return CursorIcon::EwResize;
        }
        // Chrome buttons → hand, like panes. The panel pills stamp the same map, so they get the hand thru the same lookup.
        let hit = self.chrome.hit_at(x, y);
        if self.chrome.owns_hit(hit) && hit != self.chrome.app_icon_btn.id() {
            return CursorIcon::Pointer;
        }
        if hit == self.btn_one.hit_id() || hit == self.btn_fit.hit_id() {
            return CursorIcon::Pointer;
        }
        // Resize arrows only where a press would actually resize — the bar body below the sliver stays Default (it moves the window).
        match chrome::get_resize_edge(ctx.viewport, x, y) {
            ResizeEdge::Top | ResizeEdge::Bottom => CursorIcon::NsResize,
            ResizeEdge::Left | ResizeEdge::Right => CursorIcon::EwResize,
            ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorIcon::NwseResize,
            ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorIcon::NeswResize,
            ResizeEdge::None => {
                if self.drag.is_some() { CursorIcon::Pointer } else { CursorIcon::Default }
            }
        }
    }
}
