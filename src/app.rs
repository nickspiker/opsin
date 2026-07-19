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

use crate::panel::{Observer, PanelTools, HIST_OVERSAMPLE};

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
    /// Linear SIGNED Rec.2020 of the current image (white = 65535, out-of-range preserved) — kept so exposure re-encodes without re-decoding, so the EV multiply can recover clipped-at-display speculars and sub-black noise, and as the chart's per-frame chromaticity source.
    lin: Vec<i32>,
    /// The raw sensor view — the histogram's per-frame source.
    raw: RawView,
    /// Histogram x-axis: false = linear counts, true = log2 stops. Y-axis likewise: linear count vs log2 count. Independent pills; every combination is a labelled remap, never a silent curve.
    hist_xlog: bool,
    hist_ylog: bool,
    /// Clip indicator on/off — lumis's raw inversion in [`crate::convert::to_linear`]: blown highlights render dark, crushed shadows render blown, channel-wise.
    clip_show: bool,
    /// The retained decode, so the clip toggle can re-render the linear pipe without touching disk. `None` only in the empty drop-target state.
    dec: Option<crate::convert::Decoded>,
    /// The histogram's axis pills + the clip toggle, overlaid top-right of the histogram rect. Labels read the CURRENT mode.
    btn_xscale: fluor::widgets::Button,
    btn_yscale: fluor::widgets::Button,
    btn_clip: fluor::widgets::Button,
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

/// Clip pill fill while the indicator is live — a warning red so the false-colour preview can't be mistaken for the image.
const CLIP_ON_FILL: u32 = argb(0x8B, 0x30, 0x30, 0xFF);

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

/// One decoded image, ready to install into the viewer. Produced by [`load_image`], consumed by `open` (construction) and `install` (navigation). Keeps the linear RGB so exposure changes re-encode without re-decoding the source, and the raw sensor view so the histogram bins actual counts per frame.
struct Loaded {
    pixels: Vec<u32>,
    lin: Vec<i32>,
    w: usize,
    h: usize,
    raw: RawView,
    /// The decode itself, retained so the clip toggle re-renders without touching disk. `None` only for the empty drop-target state.
    dec: Option<crate::convert::Decoded>,
    title: String,
}

/// The raw sensor plane retained for the view-live histogram: unpacked counts, CFA channel routing, black/white levels, and the orientation bridge from display coords back to sensor tiles. Everything the per-frame binning needs, nothing borrowed from the decode.
struct RawView {
    /// Sensor counts — mosaic `[h, w]`, or planar `[k, h, w]` planes.
    counts: Vec<u16>,
    /// Sensor plane width (row stride into `counts`).
    sensor_w: usize,
    /// CFA tile dims + channel index per cell; `cfa` empty ⇒ planar.
    tile_w: usize,
    tile_h: usize,
    cfa: Vec<u8>,
    /// Plane stride (w·h) for planar sources; 0 for mosaic.
    planar_n: usize,
    /// Per-display-channel black/white in raw counts (scalar levels broadcast).
    black: [f32; 3],
    white: [f32; 3],
    /// Sensor bit depth — the stops span of the log view.
    bits: usize,
    /// EXIF orientation code the display applied; [`crate::convert::orientation_src`] inverts it per pixel.
    orient: u16,
    /// PRE-orientation display dims (the debayer-bin output the orientation permuted).
    pre_w: usize,
    pre_h: usize,
    /// Per-channel sample census of one CFA tile (Bayer: G = 2) — spread deposits weight by 1/census so green's double sampling stops inflating it (lumis's equal-energy channel weighting, generalized to any tile).
    census: [f32; 3],
}

impl RawView {
    fn empty() -> Self {
        Self { counts: Vec::new(), sensor_w: 0, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: 0, black: [0.; 3], white: [1.; 3], bits: 1, orient: 1, pre_w: 0, pre_h: 0, census: [1.; 3] }
    }

    fn from_image(img: &vsf::spectral_image::SpectralImage) -> Self {
        let level = |l: &[f32], i: usize| if l.len() == 1 { l[0] } else { l.get(i).copied().unwrap_or_else(|| l.first().copied().unwrap_or(0.)) };
        let black = [level(&img.black, 0), level(&img.black, 1), level(&img.black, 2)];
        let white = [level(&img.white, 0), level(&img.white, 1), level(&img.white, 2)];
        let bits = (img.bit_depth() as usize).clamp(1, 16);
        let orient = crate::convert::orientation_code(img);
        match &img.layout {
            vsf::spectral_image::PlaneLayout::Mosaic { cfa } => {
                let (tile_h, tile_w) = (cfa.shape[0], cfa.shape[1]);
                let mut census = [0f32; 3];
                for &c in &cfa.data {
                    census[(c as usize).min(2)] += 1.;
                }
                for c in &mut census {
                    *c = c.max(1.);
                }
                Self {
                    counts: img.samples.unpack_u16(),
                    sensor_w: img.width,
                    tile_w,
                    tile_h,
                    cfa: cfa.data.clone(),
                    planar_n: 0,
                    black,
                    white,
                    bits,
                    orient,
                    pre_w: img.width / tile_w,
                    pre_h: img.height / tile_h,
                    census,
                }
            }
            vsf::spectral_image::PlaneLayout::Planar => Self {
                counts: img.samples.unpack_u16(),
                sensor_w: img.width,
                tile_w: 1,
                tile_h: 1,
                cfa: Vec::new(),
                planar_n: img.width * img.height,
                black,
                white,
                bits,
                orient,
                pre_w: img.width,
                pre_h: img.height,
                census: [1.; 3],
            },
        }
    }

    /// Tally every raw sample under display pixel (dx, dy) into the per-ADC-code table: orientation bridge → sensor tile → CFA channel routing (channels past 3 fold onto blue pending the spectral resolve). No axis math here — codes are exact, and [`Self::spread`] owns the mapping.
    #[inline]
    fn collect_codes(&self, dx: usize, dy: usize, codes: &mut [[u32; 3]]) {
        let (sx, sy) = crate::convert::orientation_src(self.orient, self.pre_w, self.pre_h, dx, dy);
        if self.planar_n > 0 {
            let idx = sy * self.sensor_w + sx;
            for ch in 0..3 {
                codes[self.counts[ch * self.planar_n + idx] as usize][ch] += 1;
            }
        } else {
            let base = sy * self.tile_h * self.sensor_w + sx * self.tile_w;
            for ty in 0..self.tile_h {
                for tx in 0..self.tile_w {
                    let ch = (self.cfa[ty * self.tile_w + tx] as usize).min(2);
                    codes[self.counts[base + ty * self.sensor_w + tx] as usize][ch] += 1;
                }
            }
        }
    }

    /// Equal-energy spread: each ADC code deposits its census-weighted count uniformly over the bin interval its quantization step `[v, v+1)` covers through the active axis — exact density on both axes, comb-free, deterministic (lumis's `bin_span` idea taken to its conclusion: the interval IS the span, deposited rather than divided, so no duty-cycle/log-order weirdness survives). Below-black collapses into bin 0 and at/above-white into the last bin — the clip spikes.
    fn spread(&self, codes: &[[u32; 3]], x_log: bool, bins: usize) -> Vec<[f32; 3]> {
        let mut dens = vec![[0f32; 3]; bins];
        for ch in 0..3 {
            let black = self.black[ch];
            let range = (self.white[ch] - black).max(1.);
            let frac = |x: f32| -> f32 {
                if x_log {
                    if x <= 0. { 0. } else { (1. + (x / range).log2() / self.bits as f32).clamp(0., 1.) }
                } else {
                    (x / range).clamp(0., 1.)
                }
            };
            let weight = 1. / self.census[ch];
            // Effective quantization step: files re-scaled after capture (e.g. a 10-bit frame stretched
            // into 16-bit codes by older lumis saves) only populate every Nth code, and depositing over
            // [v, v+1) would re-comb them. The MODE of the gaps between occupied codes is the honest
            // estimator: a native file's mode is 1 (identical behaviour, bit for bit), a stretched file's
            // is its stretch factor. Irregular stretches (65536/1023 alternates 64/65) leave sub-bin
            // residue only. Scene-content gaps can't skew a mode the way a mean or a max would.
            let step = {
                let mut gap_hist = [0u32; 257];
                let mut prev: Option<usize> = None;
                for (v, code) in codes.iter().enumerate() {
                    if code[ch] > 0 {
                        if let Some(p) = prev {
                            gap_hist[(v - p).min(256)] += 1;
                        }
                        prev = Some(v);
                    }
                }
                gap_hist.iter().enumerate().skip(1).max_by_key(|e| *e.1).map(|(g, _)| g).unwrap_or(1).max(1) as f32
            };
            for (v, code) in codes.iter().enumerate() {
                let c = code[ch];
                if c == 0 {
                    continue;
                }
                let b0 = frac(v as f32 - black) * bins as f32;
                let b1 = (frac(v as f32 + step - black) * bins as f32).max(b0);
                let total = c as f32 * weight;
                let lo = (b0 as usize).min(bins - 1);
                if b1 - b0 <= f32::EPSILON {
                    // Degenerate interval — the clip collapses (≤ black, ≥ white) land whole in one bin.
                    dens[lo][ch] += total;
                    continue;
                }
                let hi = (b1.ceil() as usize).clamp(lo + 1, bins);
                let inv = total / (b1 - b0);
                for (b, d) in dens[lo..hi].iter_mut().enumerate() {
                    let bf = (lo + b) as f32;
                    d[ch] += (b1.min(bf + 1.) - b0.max(bf)).max(0.) * inv;
                }
            }
        }
        dens
    }
}

/// Linear signed Rec.2020 → gamma-2 u8 visible → darkness-packed u32. Exposure is a Q16 integer multiply — the gain constant is the only float, precomputed once (a scalar commutes with the cmx, shifts no hue). The SINGLE display clamp in the whole pipe follows the multiply: negative light and beyond-white cannot display, and the bare integer cast would wrap (a −1 shadow pixel would speckle full-white), so the clamp is the u16 container boundary, applied at the last possible moment — everything before it is signed and recoverable. Then the EV-independent sqrt LUT (64Ki sqrts, built ONCE per process — it never varies with EV) maps to display bytes. Each output pixel depends only on its own three samples, so the pass splits across the rayon pool — this runs on every exposure-slider tick and must stay interactive at full resolution.
fn encode_pixels(lin: &[i32], ev: f32) -> Vec<u32> {
    use rayon::prelude::*;
    const GAIN_SHIFT: u32 = 1 << 4;
    static LUT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let lut = LUT.get_or_init(|| (0..65536u32).map(|v| 255 - ((v as f32 / 65535.).sqrt() * 255.) as u32).collect());
    let gain = (2f64.powf(ev as f64) * (1u64 << GAIN_SHIFT) as f64).round() as i64;
    lin.par_chunks_exact(3)
        .map(|px| {
            let ch = |v: i32| lut[((v as i64 * gain) >> GAIN_SHIFT).clamp(0, 65535) as usize];
            0xFF000000 | (ch(px[0]) << 16) | (ch(px[1]) << 8) | ch(px[2])
        })
        .collect()
}

/// Decode `path` (any supported format) into display pixels + the raw sensor view + a title, rendering with the caller's live clip-indicator state. Encoded at EV 0; the caller re-encodes if it's carrying exposure over.
fn load_image(path: &Path, clip_show: bool) -> Result<Loaded, String> {
    let dec = crate::convert::load_any(path)?;
    let (w, h, lin) = crate::convert::to_linear(&dec, clip_show)?;
    let pixels = encode_pixels(&lin, 0.);
    let raw = RawView::from_image(&dec.img);
    let title = format!(
        "opsin — {} ({}×{}, {} ch, {}-bit)",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        dec.img.width,
        dec.img.height,
        dec.img.channel_count(),
        dec.img.bit_depth()
    );
    Ok(Loaded { pixels, lin, w, h, raw, dec: Some(dec), title })
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
            raw: RawView::empty(),
            dec: None,
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

        let loaded = load_image(&dir_list[dir_idx], false)?;
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
        let btn_xscale = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "X Lin");
        let btn_yscale = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Y Lin");
        let btn_clip = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Clip");

        let tools = PanelTools::new(&loaded.pixels, loaded.w, loaded.h);

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
            tools,
            dir_list,
            dir_idx,
            lin: loaded.lin,
            raw: loaded.raw,
            hist_xlog: false,
            hist_ylog: false,
            clip_show: false,
            dec: loaded.dec,
            btn_xscale,
            btn_yscale,
            btn_clip,
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
        let Some((fx, fy, fw, fh)) = self.nav_fit(viewport) else {
            return;
        };
        if self.zoom_rel <= 0. {
            return;
        }
        // Clamp justified: external drag input — the cursor can leave the thumb entirely; pinning to the edge is the intended behavior, not bug-hiding.
        self.cx_frac = ((cx - fx as f32) / fw as f32).clamp(0., 1.);
        self.cy_frac = ((cy - fy as f32) / fh as f32).clamp(0., 1.);
    }

    /// The aspect-fitted thumb placement inside the navigator rect (letterboxed, centered) — blit, view-rect overlay, and cursor mapping ALL share this rect, so the navigator never stretches and clicks land exactly where they look. `None` when the panel or the image has no extent.
    fn nav_fit(&self, viewport: Viewport) -> Option<(usize, usize, usize, usize)> {
        let (nx, ny, nw, nh) = self.panel_rects(viewport).nav;
        let (tw, th) = (self.tools.thumb_w, self.tools.thumb_h);
        if nw == 0 || nh == 0 || tw == 0 || th == 0 {
            return None;
        }
        let fw = nw.min(nh * tw / th).max(1);
        let fh = (fw * th / tw).clamp(1, nh);
        Some((nx + (nw - fw) / 2, ny + (nh - fh) / 2, fw, fh))
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

    /// Swap the decoded image into the view: title, pixels, dims, panel tools, redraw. Same dimensions ⇒ the view state carries over — pan, zoom, and exposure stay put so stepping through a burst or LED sequence compares like with like. Different dimensions ⇒ refit and reset exposure. Dims arrive from `to_linear` with EXIF orientation already applied, so they remain the whole test — a 90°-tagged frame in a landscape burst lands portrait and correctly refits.
    fn install(&mut self, loaded: Loaded, ctx: &mut Context) {
        let same_geometry = loaded.w == self.img_w && loaded.h == self.img_h && self.img_w > 0;
        self.chrome.set_title(&loaded.title);
        self.title = loaded.title;
        self.img_w = loaded.w;
        self.img_h = loaded.h;
        self.tools = PanelTools::new(&loaded.pixels, loaded.w, loaded.h);
        self.lin = loaded.lin;
        self.raw = loaded.raw;
        self.dec = loaded.dec;
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
            match load_image(&self.dir_list[idx], self.clip_show) {
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
        match load_image(path, self.clip_show) {
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
        // Record the live exposure as a Technical view op (a scalar shifts no hue) — APPENDED to the translateration log, so the ingest-recorded orientation op rides ahead of it. EV 0 adds nothing (and an op-less log was never created).
        if self.ev.abs() > 1e-4 {
            let op = vsf::spectral_image::ViewOp {
                name: "exposure".to_string(),
                class: vsf::spectral_image::IdtClass::Technical,
                params: vec![self.ev],
            };
            match &mut dec.img.view {
                Some(v) => v.ops.push(op),
                None => {
                    dec.img.view = Some(vsf::spectral_image::ViewTransform {
                        space: "vsf_rgb_linear".to_string(),
                        ops: vec![op],
                    })
                }
            }
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
        let (nx, ny, nw, nh) = self.nav_fit(viewport)?;
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

impl Container for OpsinApp {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn fluor::host::widget::Widget)) {
        self.chrome.visit(f);
        f(&mut self.btn_one);
        f(&mut self.btn_fit);
        f(&mut self.btn_xscale);
        f(&mut self.btn_yscale);
        f(&mut self.btn_clip);
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
                for b in [&mut self.btn_one, &mut self.btn_fit, &mut self.btn_xscale, &mut self.btn_yscale, &mut self.btn_clip] {
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
                    if self.btn_xscale.take_click() {
                        // X: linear counts ↔ log2 stops — an explicit, labelled remap; the pill always reads the CURRENT mode.
                        self.hist_xlog = !self.hist_xlog;
                        self.btn_xscale.set_label(if self.hist_xlog { "X Log" } else { "X Lin" });
                        ctx.window.request_redraw();
                    }
                    if self.btn_yscale.take_click() {
                        // Y: linear count ↔ log2 count.
                        self.hist_ylog = !self.hist_ylog;
                        self.btn_yscale.set_label(if self.hist_ylog { "Y Log" } else { "Y Lin" });
                        ctx.window.request_redraw();
                    }
                    if self.btn_clip.take_click() {
                        // Clip indicator: re-render the linear pipe from the retained decode with lumis's raw inversion — blown highlights dark, crushed shadows blown, channel-wise. Display-only; the stored plane and the raw histogram source are untouched.
                        self.clip_show = !self.clip_show;
                        self.btn_clip.set_fill(self.clip_show.then_some(CLIP_ON_FILL));
                        if let Some(dec) = &self.dec {
                            if let Ok((_, _, lin)) = crate::convert::to_linear(dec, self.clip_show) {
                                self.lin = lin;
                                self.pixels = encode_pixels(&self.lin, self.ev);
                                self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
                            }
                        }
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
        let PanelRects { nav: _, btns, hist, ev: ev_rect, chart } = self.panel_rects(ctx.viewport);
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
                ctx.text.draw_text_center(&mut canvas, &magnification, (bx + bw / 2) as f32, (by + bh / 2) as f32, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
            }
            // Exposure slider + EV label. Label takes the band's left end, the slider the rest; the fluor Slider paints the lumis-style white/black track + circular handle.
            let (ex, ey, ew, eh) = ev_rect;
            if ew > 0 && eh > 0 && !self.lin.is_empty() {
                let label_w = eh * 4;
                let font = eh as f32 * (7. / (1 << 3) as f32);
                // Truncated toward zero, never rounded — same integer decomposition as the magnification readout, so the label shows the stored value's truth: three 1/3-stop nudges display "+0.99" because the float accumulation genuinely is a hair under a stop.
                let centi = (self.ev * 100.).trunc() as i32;
                let label = format!("{}{}.{:02}", if centi < 0 { '-' } else { '+' }, (centi / 100).abs(), (centi % 100).abs());
                ctx.text.draw_text_left(&mut canvas, &label, ex as f32, (ey + eh / 2) as f32, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
                let sw = ew.saturating_sub(label_w);
                if sw > eh {
                    self.ev_slider.set_rect((ex + label_w + sw / 2) as f32, (ey + eh / 2) as f32, sw as f32, eh as f32);
                    let id = self.ev_slider.hit_id();
                    self.ev_slider.render_content_into(&mut canvas, None, id);
                }
            }

            // Histogram pills in their OWN band carved from the top of the hist rect — above the plot, never on it. Right-aligned row: [X ..][Y ..][Clip]. Geometry derives from the hist rect (RU-coherent, no pixel constants).
            let (hx, hy, hw, hh) = hist;
            let pill_h = (hh as f32 / 5.).max(8.);
            let pill_pad = hh as f32 / (1 << 4) as f32;
            let pill_band = (pill_h + pill_pad * 2.) as usize;
            if hw > 0 && hh > pill_band && !self.raw.counts.is_empty() {
                let pill_w = pill_h * 3.;
                let cy_pill = hy as f32 + pill_pad + pill_h / 2.;
                let mut right = hx as f32 + hw as f32 - pill_pad;
                for b in [&mut self.btn_clip, &mut self.btn_yscale, &mut self.btn_xscale] {
                    b.set_rect(right - pill_w / 2., cy_pill, pill_w, pill_h);
                    b.set_font_size(pill_h * (3. / 4.));
                    let id = b.hit_id();
                    b.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                    right -= pill_w + pill_pad;
                }
            }
            // The plot body takes the rest of the rect, below the pill band.
            let (hy, hh) = (hy + pill_band.min(hh), hh.saturating_sub(pill_band));

            // Histogram body — RAW counts of WHAT'S IN VIEW, per frame, equal-energy: every visible display pixel bridges back to its sensor tile and tallies its samples into the per-ADC-code table (exact integers, no axis math per sample); the spread then deposits each code over the bin interval its quantization step covers through the active axis. Comb-free by construction; XOR stop hairlines render in panel::render_hist.
            if hw > 0 && hh > 0 {
                let bins = hw * HIST_OVERSAMPLE;
                let codes: Vec<[u32; 3]> = if !self.raw.counts.is_empty() && self.img_w > 0 && zoom > 0. {
                    use rayon::prelude::*;
                    let raw = &self.raw;
                    let (img_w, img_h) = (self.img_w, self.img_h);
                    let x_end = divider_px.min(buf_w);
                    let y_start = bar_h.min(buf_h);
                    (y_start..buf_h)
                        .into_par_iter()
                        .with_min_len(((buf_h - y_start) / 8).max(1))
                        .fold(
                            || vec![[0u32; 3]; 1 << 16],
                            |mut c, sy| {
                                let fy = (sy as f32 - img_oy) / zoom;
                                if fy >= 0. && (fy as usize) < img_h {
                                    for sx in 0..x_end {
                                        let fx = (sx as f32 - img_ox) / zoom;
                                        if fx >= 0. && (fx as usize) < img_w {
                                            raw.collect_codes(fx as usize, fy as usize, &mut c);
                                        }
                                    }
                                }
                                c
                            },
                        )
                        .reduce(
                            || vec![[0u32; 3]; 1 << 16],
                            |mut a, b| {
                                for (x, y) in a.iter_mut().zip(&b) {
                                    for ch in 0..3 {
                                        x[ch] += y[ch];
                                    }
                                }
                                a
                            },
                        )
                } else {
                    vec![[0u32; 3]; 1 << 16]
                };
                let dens = self.raw.spread(&codes, self.hist_xlog, bins);
                // Stop hairlines at oversampled-bin precision: every whole stop from saturation down to the sensor's bit floor — equally spaced in log, halving positions in linear.
                let stop_bins: Vec<usize> = if self.raw.counts.is_empty() {
                    Vec::new()
                } else if self.hist_xlog {
                    (0..=self.raw.bits as i32).map(|s| (((1. - s as f32 / self.raw.bits as f32) * bins as f32) as usize).min(bins - 1)).collect()
                } else {
                    (0..=self.raw.bits as i32).map(|s| ((bins as f32 / 2f32.powi(s)) as usize).min(bins - 1)).collect()
                };
                let hist_px = crate::panel::render_hist(&dens, hw, hh, &stop_bins, self.hist_ylog);
                for row in 0..hh.min(buf_h.saturating_sub(hy)) {
                    for col in 0..hw.min(buf_w.saturating_sub(hx)) {
                        put(target, buf_w, hx + col, hy + row, hist_px[row * hw + col]);
                    }
                }
            }
        }
        // Navigator thumbnail — nearest blit into the ASPECT-FITTED sub-rect (letterboxed, centered): the navigator never stretches the image. `nav_fit` is the shared truth for blit, view-rect overlay, and cursor mapping.
        let fitted = self.nav_fit(ctx.viewport);
        if let Some((fx, fy, fw, fh)) = fitted {
            for ty in 0..fh.min(buf_h.saturating_sub(fy)) {
                let sy = ty * self.tools.thumb_h / fh;
                for tx in 0..fw.min(buf_w.saturating_sub(fx)) {
                    let sx = tx * self.tools.thumb_w / fw;
                    put(target, buf_w, fx + tx, fy + ty, self.tools.thumb[sy * self.tools.thumb_w + sx]);
                }
            }
        }
        // Navigator view rect — the TRUE viewport rect in thumb space, unclamped (it slides off the thumb edge when panned past the image instead of shrinking and sticking), drawn AFTER the thumbnail as a wrapping-add-of-128 marker on each gamma-encoded RGB byte (`b ^ 0x80` — self-contrasting on any content).
        if let (Some((fx, fy, fw, fh)), true) = (fitted, zoom > 0. && self.img_w > 0) {
            let img_area_w = self.divider_x(ctx.viewport);
            let sx = fw as f32 / (self.img_w as f32 * zoom);
            let sy = fh as f32 / (self.img_h as f32 * zoom);
            let rx0 = fx as isize + ((0. - img_ox) * sx) as isize;
            let ry0 = fy as isize + ((bar_h as f32 - img_oy) * sy) as isize;
            let rx1 = fx as isize + ((img_area_w - img_ox) * sx) as isize;
            let ry1 = fy as isize + ((buf_h as f32 - img_oy) * sy) as isize;
            let (cx0, cy0) = (fx as isize, fy as isize);
            let (cx1, cy1) = ((fx + fw) as isize, (fy + fh) as isize);
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
        // Chromaticity chart — oriel's Maxwell triangle, recomputed THIS FRAME at exactly this rect's size from exactly the pixels visible in the image area: walk the image-area screen pixels, invert the view transform, splat each visible sample's chromaticity into a chart-pixel density grid, render. No base resolution, no resampling — chart pixels ARE screen pixels, and the cloud tracks pan/zoom live. The √3/2 height is the equilateral triangle's geometry, not a pixel ratio.
        let (cx, cy, cw, chh) = chart;
        if cw > 0 && chh > 0 {
            let dw = cw.min((chh as f32 * 2. / 3f32.sqrt()) as usize).max(1);
            let dh = ((dw as f32 * 3f32.sqrt() / 2.) as usize).clamp(1, chh);
            let ox = cx + (cw - dw) / 2;
            let oy = cy + (chh - dh) / 2;
            let density = if self.img_w > 0 && zoom > 0. && !self.lin.is_empty() {
                use rayon::prelude::*;
                let lin: &[i32] = &self.lin;
                let (img_w, img_h) = (self.img_w, self.img_h);
                let x_end = divider_px.min(buf_w);
                let y_start = bar_h.min(buf_h);
                // Parallel fold over screen rows, split into few bands so the per-band grid merges stay far below the sample splats.
                (y_start..buf_h)
                    .into_par_iter()
                    .with_min_len(((buf_h - y_start) / 8).max(1))
                    .fold(
                        || vec![0u32; dw * dh],
                        |mut grid, sy| {
                            let fy = (sy as f32 - img_oy) / zoom;
                            if fy >= 0. && (fy as usize) < img_h {
                                let row = (fy as usize) * img_w;
                                for sx in 0..x_end {
                                    let fx = (sx as f32 - img_ox) / zoom;
                                    if fx >= 0. && (fx as usize) < img_w {
                                        let i = (row + fx as usize) * 3;
                                        let r = lin[i].max(0) as f32;
                                        let g = lin[i + 1].max(0) as f32;
                                        let b = lin[i + 2].max(0) as f32;
                                        if let Some((px, py)) = crate::panel::project(r, g, b, dw, dh) {
                                            if px >= 0. && px < dw as f32 && py >= 0. && py < dh as f32 {
                                                grid[py as usize * dw + px as usize] += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            grid
                        },
                    )
                    .reduce(
                        || vec![0u32; dw * dh],
                        |mut a, b| {
                            for (x, y) in a.iter_mut().zip(&b) {
                                *x += y;
                            }
                            a
                        },
                    )
            } else {
                vec![0u32; dw * dh]
            };
            let chart_px = crate::panel::render_chart(&density, dw, dh, &Observer::stock());
            for py in 0..dh.min(buf_h.saturating_sub(oy)) {
                for px in 0..dw.min(buf_w.saturating_sub(ox)) {
                    let v = chart_px[py * dw + px];
                    if v >> 24 != 0 {
                        put(target, buf_w, ox + px, oy + py, v);
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
        if [self.btn_one.hit_id(), self.btn_fit.hit_id(), self.btn_xscale.hit_id(), self.btn_yscale.hit_id(), self.btn_clip.hit_id()].contains(&hit) {
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
