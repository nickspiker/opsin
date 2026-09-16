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
    /// Exposure in stops (gain = 2^ev in linear), [EV_MIN]..=[EV_MAX].
    ev: f32,
    /// The panel's exposure slider (fluor widget, value 0..1 ↔ EV_MIN..EV_MAX; 0 EV sits at [EV_ZERO]).
    ev_slider: fluor::widgets::Slider,
    /// 1:1 / Fit — fluor pill Buttons, same widget family as the slider and chrome (squircle, AA edge, hover tint thru the host overlay pipe). Geometry is set every frame from panel_rects; hit silhouettes stamp into the chrome hit map at render, so dispatch rides the same Container walk as the chrome controls.
    btn_one: fluor::widgets::Button,
    btn_fit: fluor::widgets::Button,
    /// sRGB JPEG export pill — the visible face of the `E` key.
    btn_export: fluor::widgets::Button,
    /// Frame-info HUD toggle pill — the visible face of the `I` key; filled while the HUD is shown.
    btn_info: fluor::widgets::Button,
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
    /// Crop rect in DISPLAY pixels (after orientation), `[x0, x1) × [y0, y1)`; `Some` = crop mode on. Toggling on seeds the full frame and fits; a click or drag on the image moves the NEAREST corner to the cursor (no handles, no modes); toggling off clears and fits. Armed = the JPEG exports the rect and `V` records a `crop` view op. A view op: it culls, the plane never changes.
    crop: Option<(usize, usize, usize, usize)>,
    /// The corner being dragged (0 = x0y0, 1 = x1y0, 2 = x0y1, 3 = x1y1) while the button is down in crop mode.
    crop_drag: Option<usize>,
    btn_crop: fluor::widgets::Button,
    /// 90° rotates: compose onto the orientation view op, re-render from the retained decode. Display-only, recorded by `V`; the stored plane is never touched.
    btn_rot_ccw: fluor::widgets::Button,
    btn_rot_cw: fluor::widgets::Button,
    /// HDR highlight rolloff at the encode boundary (screen AND JPEG export — they must agree): `(3x − x³)/2` after the clamp, per channel, in linear. Brightens (slope 1.5 at black) and compresses the top into a soft shoulder instead of a hard stop; pull EV down ~3× and the reclaimed range is ~1.5 stops of highlight. `H` / the HDR pill. A CREATIVE op — recorded as `dr_curve` when converting to VSF, never silent.
    hdr: bool,
    btn_hdr: fluor::widgets::Button,
    /// Frame info HUD (metadata + live stats) over the image area's bottom-left; `I` toggles. Default on — opsin is an instrument, the readings are the point.
    show_info: bool,
    /// DNG header metadata for the HUD (EXIF exposure fields, profile name, baseline exposure); `None` for non-TIFF sources.
    meta: Option<crate::tiff::FrameMeta>,
    file_size: u64,
    file_name: String,
    /// Image pixel under the cursor at the last HUD-relevant redraw — the redraw gate for cursor motion (only a CHANGE of image pixel repaints, so high zoom doesn't repaint per screen pixel).
    info_px: Option<(usize, usize)>,
    /// Hitmask debug overlay active ([]h) — render's last act replaces every pixel with its hit id's palette colour.
    show_hitmask: bool,
    /// 256 random opaque colours (α+darkness), regenerated on each []h enable so distinct ids always pop.
    debug_hit_colours: Vec<u32>,
}

/// Grace for X11's synthetic key-Release while a key is actually held (photon's chord constant).
const CHORD_RELEASE_GRACE: Duration = Duration::from_millis(40);

/// Clip pill fill while the indicator is live — a warning red so the false-colour preview can't be mistaken for the image.
const CLIP_ON_FILL: u32 = argb(0x8B, 0x30, 0x30, 0xFF);

/// Info pill fill while the HUD is shown — a quiet blue-grey, distinct from the clip pill's warning red.
const INFO_ON_FILL: u32 = argb(0x30, 0x48, 0x70, 0xFF);

/// HUD backdrop — translucent near-black so the readings stay legible over any image content.
const HUD_BG: u32 = argb(0x10, 0x10, 0x10, 0xB8);

/// Exposure slider range in stops — asymmetric on purpose: −4 is as far as pulling down ever needs to go, but a sensor holds ~12 stops above its noise floor and the signed-linear pipe keeps every one of them, so pushing up runs all the way to +12 to blow a whole frame's highlights through the clip indicator. Gain at +12 is 2^12 · 2^16 (Q16) = 2^28 per i32 sample — nowhere near i64.
const EV_MIN: f32 = -((1 << 2) as f32);
const EV_MAX: f32 = (12) as f32;
/// Slider position (0..1) of 0 EV.
const EV_ZERO: f32 = -EV_MIN / (EV_MAX - EV_MIN);

/// Slider 0..1 → stops.
fn ev_of_slider(v: f32) -> f32 {
    EV_MIN + v * (EV_MAX - EV_MIN)
}
/// Stops → slider 0..1 (clamped to the range).
fn slider_of_ev(ev: f32) -> f32 {
    (ev.clamp(EV_MIN, EV_MAX) - EV_MIN) / (EV_MAX - EV_MIN)
}

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
    meta: Option<crate::tiff::FrameMeta>,
    file_size: u64,
    file_name: String,
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

    /// Every raw sample under display pixel (dx, dy) as (channel, count), tile order — the HUD's cursor readout. Same orientation bridge as the histogram, so the two can't disagree about which sensor tile a screen pixel shows.
    fn samples_at(&self, dx: usize, dy: usize) -> Vec<(usize, u16)> {
        // Display dims are the pre-orientation dims, swapped for the transposing codes (5..=8); check BEFORE the bridge — its subtractions assume in-range input.
        let (dw, dh) = if self.orient >= 5 { (self.pre_h, self.pre_w) } else { (self.pre_w, self.pre_h) };
        if self.counts.is_empty() || dx >= dw || dy >= dh {
            return Vec::new();
        }
        let (sx, sy) = crate::convert::orientation_src(self.orient, self.pre_w, self.pre_h, dx, dy);
        if self.planar_n > 0 {
            let idx = sy * self.sensor_w + sx;
            (0..3).map(|ch| (ch, self.counts[ch * self.planar_n + idx])).collect()
        } else {
            let base = sy * self.tile_h * self.sensor_w + sx * self.tile_w;
            let mut out = Vec::with_capacity(self.tile_w * self.tile_h);
            for ty in 0..self.tile_h {
                for tx in 0..self.tile_w {
                    out.push((self.cfa[ty * self.tile_w + tx] as usize, self.counts[base + ty * self.sensor_w + tx]));
                }
            }
            out
        }
    }

    /// Equal-energy spread: each ADC code deposits its census-weighted count uniformly over the bin interval its quantization step `[v, v+1)` covers through the active axis — exact density on both axes, comb-free, deterministic (lumis's `bin_span` idea taken to its conclusion: the interval IS the span, deposited rather than divided, so no duty-cycle/log-order weirdness survives). Below-black collapses into bin 0 and at/above-white into the last bin — the clip spikes.
    ///
    /// `gain` is the live EV multiplier (2^ev), applied to the black-subtracted counts before the axis map, so the histogram tracks the exposure slider exactly as the display does: in linear-x every peak moves 2× per stop, in log-x the whole distribution translates one stop per stop — a labelled remap of the same raw counts, still no curve. Data pushed past display white by the gain collapses into the last bin: the clip spike grows as you rack exposure, agreeing with the image's encode-boundary indicator.
    fn spread(&self, codes: &[[u32; 3]], x_log: bool, bins: usize, gain: f32) -> Vec<[f32; 3]> {
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
            // Effective quantization step: files re-scaled after capture (e.g. a 10-bit frame stretched into 16-bit codes by older lumis saves) only populate every Nth code, and depositing over
            // [v, v+1) would re-comb them. The MODE of the gaps between occupied codes is the honest estimator: a native file's mode is 1 (identical behaviour, bit for bit), a stretched file's is its stretch factor. Irregular stretches (65536/1023 alternates 64/65) leave sub-bin residue only. Scene-content gaps can't skew a mode the way a mean or a max would.
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
                let b0 = frac((v as f32 - black) * gain) * bins as f32;
                let b1 = (frac((v as f32 + step - black) * gain) * bins as f32).max(b0);
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
///
/// `clip_show` is lumis's `preview_sub` indicator relocated to opsin's one display clamp — HERE, after the magic-9 and the EV gain, so it marks what is clipping AT DISPLAY under the current exposure and moves live with the slider. Channel-wise, same inversion as lumis: a channel at/over display white renders DARK, a channel below zero renders BLOWN. Indicator-only — `lin` is never touched, so re-encodes and the JPEG export (which takes `lin` directly) stay clean of it by construction.
///
/// `hdr` applies the highlight rolloff [`crate::convert::hdr_rail`] after the clamp and before the transfer — per channel, in linear, exactly where oriel applies its `sin(πx/2)` twin. A second 64Ki LUT bakes curve+sqrt together, so the per-pixel cost is unchanged. The clip indicator is untouched by it: the inversion fires on the pre-curve over/under test, and `f(1) = 1` keeps "at display white" meaning the same thing.
fn encode_pixels(lin: &[i32], ev: f32, clip_show: bool, hdr: bool) -> Vec<u32> {
    use rayon::prelude::*;
    const GAIN_SHIFT: u32 = 1 << 4;
    static LUT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    static LUT_HDR: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let lut = if hdr {
        LUT_HDR.get_or_init(|| (0..65536u32).map(|v| 255 - ((crate::convert::hdr_rail(v as i64) as f32 / 65535.).sqrt() * 255.) as u32).collect())
    } else {
        LUT.get_or_init(|| (0..65536u32).map(|v| 255 - ((v as f32 / 65535.).sqrt() * 255.) as u32).collect())
    };
    let gain = (2f64.powf(ev as f64) * (1u64 << GAIN_SHIFT) as f64).round() as i64;
    lin.par_chunks_exact(3)
        .map(|px| {
            let ch = |v: i32| {
                let g = (v as i64 * gain) >> GAIN_SHIFT;
                let idx = if clip_show {
                    // preview_sub after the display transform: over-white → 0 (dark), under-black → max (blown).
                    if g >= 65535 {
                        0
                    } else if g < 0 {
                        65535
                    } else {
                        g
                    }
                } else {
                    g.clamp(0, 65535)
                };
                lut[idx as usize]
            };
            0xFF000000 | (ch(px[0]) << 16) | (ch(px[1]) << 8) | ch(px[2])
        })
        .collect()
}

/// Decode `path` (any supported format) into display pixels + the raw sensor view + a title. Encoded plain at EV 0; the caller re-encodes if it's carrying exposure or the clip indicator over.
fn load_image(path: &Path) -> Result<Loaded, String> {
    let dec = crate::convert::load_any(path)?;
    let (w, h, lin) = crate::convert::to_linear(&dec)?;
    let pixels = encode_pixels(&lin, 0., false, false);
    let raw = RawView::from_image(&dec.img);
    let title = format!(
        "opsin — {} ({}×{}, {} ch, {}-bit)",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        dec.img.width,
        dec.img.height,
        dec.img.channel_count(),
        dec.img.bit_depth()
    );
    // Header-only reads for the HUD; a non-TIFF source (JXL/JPEG/VSF) simply has no EXIF block here.
    let meta = crate::tiff::FrameMeta::read_path(path).ok();
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Loaded { pixels, lin, w, h, raw, dec: Some(dec), title, meta, file_size, file_name })
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
            meta: None,
            file_size: 0,
            file_name: String::new(),
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
        let ev_slider = fluor::widgets::Slider::new(&mut hit_counter, 0., 0., 1., 1., EV_ZERO);
        let btn_one = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "1:1");
        let btn_fit = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Fit");
        let btn_export = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "JPEG");
        let btn_hdr = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "HDR");
        let btn_crop = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Crop");
        let btn_rot_ccw = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "CCW");
        let btn_rot_cw = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "CW");
        let mut btn_info = fluor::widgets::Button::new(&mut hit_counter, 0., 0., 1., 1., 1., "Info");
        btn_info.set_fill(Some(INFO_ON_FILL));
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
            btn_export,
            btn_info,
            ev_drag: false,
            nav_drag: false,
            hit_count: hit_counter,
            chord_lb_press: None,
            chord_lb_release: None,
            chord_rb_press: None,
            chord_rb_release: None,
            hdr: false,
            btn_hdr,
            crop: None,
            crop_drag: None,
            btn_crop,
            btn_rot_ccw,
            btn_rot_cw,
            show_info: true,
            meta: loaded.meta,
            file_size: loaded.file_size,
            file_name: loaded.file_name,
            info_px: None,
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

    /// Apply the slider's 0..1 value as stops and re-encode the display pixels. The panel thumbnail tracks (cheap), the histogram tracks too (the same gain remaps its bins — see the render arm), and the chart alone stays put (chromaticity ratios shrug at a scalar).
    fn apply_ev(&mut self, value01: f32, ctx: &mut Context) {
        let ev = ev_of_slider(value01);
        if (ev - self.ev).abs() < 1e-4 || self.lin.is_empty() {
            return;
        }
        self.ev = ev;
        self.pixels = encode_pixels(&self.lin, ev, self.clip_show, self.hdr);
        self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
        ctx.window.request_redraw();
    }

    /// Swap the decoded image into the view: title, pixels, dims, panel tools, redraw. EVERY setting the operator holds survives a load — exposure, clip, histogram axes, the HUD, the panel split, the window — whether the frame arrives by arrow, drop, or socket handoff. Same dimensions ⇒ pan and zoom stay put too, so stepping through a burst or LED sequence compares like with like; different dimensions ⇒ refit (the one thing that can't sensibly carry: a composition framed on one sensor's dims). Dims arrive from `to_linear` with EXIF orientation already applied, so they remain the whole test — a 90°-tagged frame in a landscape burst lands portrait and correctly refits.
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
        self.meta = loaded.meta;
        self.file_size = loaded.file_size;
        self.file_name = loaded.file_name;
        if self.clip_show || self.hdr || self.ev.abs() > 1e-4 {
            // Carry the exposure and the clip indicator into the new frame (loaded.pixels were encoded plain at EV 0) — both are the operator's settings, not the frame's.
            self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
            self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
        } else {
            self.pixels = loaded.pixels;
        }
        if !same_geometry {
            // A crop is framed on one sensor's dims: it can't mean anything on another's, and an out-of-range rect would be a bad slice at export. Same dims (a burst) keep it.
            self.crop = None;
            self.crop_drag = None;
            self.btn_crop.set_fill(None);
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
        if crate::sniff::sniff_path(src) == Some(crate::sniff::Kind::Vsf) {
            return Err("already a VSF image".to_string());
        }
        let out = src.with_extension("vsf");
        let mut dec = crate::convert::load_any(src)?;
        // Record the live view ops — APPENDED to the translateration log, so the ingest-recorded orientation op rides ahead. `exposure` (Technical: a scalar shifts no hue) when EV ≠ 0; `dr_curve` (CREATIVE: a curve is a deliberate look) when HDR is on, params = the polynomial coefficients [c0, c1, c2, c3] of f(x) = Σ cᵢxⁱ applied per channel to the clamped linear display value — self-describing, so a reader can replay it without knowing opsin. Neither is ever baked into the plane; an op-less log is never created.
        // The display orientation as the viewer holds it NOW (ingest's EXIF code composed with any 90° turns): replace the ingest-recorded op's param, or insert one at the head so it rides ahead of everything else.
        if let Some(cur) = self.dec.as_ref() {
            let code = crate::convert::orientation_code(&cur.img);
            let op = vsf::spectral_image::ViewOp { name: "orientation".to_string(), class: vsf::spectral_image::IdtClass::Technical, params: vec![code as f32] };
            match &mut dec.img.view {
                Some(v) => match v.ops.iter_mut().find(|o| o.name == "orientation") {
                    Some(o) => o.params = vec![code as f32],
                    None if code != 1 => v.ops.insert(0, op),
                    None => {}
                },
                None if code != 1 => dec.img.view = Some(vsf::spectral_image::ViewTransform { space: "vsf_rgb_linear".to_string(), ops: vec![op] }),
                None => {}
            }
        }
        let mut ops = Vec::new();
        // `crop [x, y, w, h]` in display pixels AFTER orientation — ops replay in order, so the rect means what the screen showed. Technical: it culls, it shifts no hue.
        if let Some((x0, y0, x1, y1)) = self.crop {
            ops.push(vsf::spectral_image::ViewOp { name: "crop".to_string(), class: vsf::spectral_image::IdtClass::Technical, params: vec![x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32] });
        }
        if self.ev.abs() > 1e-4 {
            ops.push(vsf::spectral_image::ViewOp { name: "exposure".to_string(), class: vsf::spectral_image::IdtClass::Technical, params: vec![self.ev] });
        }
        if self.hdr {
            ops.push(vsf::spectral_image::ViewOp { name: "dr_curve".to_string(), class: vsf::spectral_image::IdtClass::Creative, params: crate::convert::HDR_CURVE_COEFS.to_vec() });
        }
        if !ops.is_empty() {
            match &mut dec.img.view {
                Some(v) => v.ops.extend(ops),
                None => dec.img.view = Some(vsf::spectral_image::ViewTransform { space: "vsf_rgb_linear".to_string(), ops }),
            }
        }
        crate::convert::write_vsf(&dec.img, &out)?;
        Ok(out)
    }

    /// Export the current view as an sRGB JPEG beside the source (`<stem>.jpg`) — the live exposure baked in, everything else exactly the rendering on screen. `self.lin` already carries the orientation, so the JPEG lands the way the viewer shows it. Returns the written path for the caller to surface.
    fn export_current_jpeg(&self) -> Result<PathBuf, String> {
        let Some(src) = self.dir_list.get(self.dir_idx) else {
            return Err("no image loaded".to_string());
        };
        if self.lin.is_empty() {
            return Err("no image loaded".to_string());
        }
        let out = src.with_extension("jpg");
        match self.crop {
            Some((x0, y0, x1, y1)) => {
                // Armed crop: export exactly the rect — row slices of the oriented linear buffer.
                let (cw, ch) = (x1 - x0, y1 - y0);
                let mut sub = Vec::with_capacity(cw * ch * 3);
                for y in y0..y1 {
                    sub.extend_from_slice(&self.lin[(y * self.img_w + x0) * 3..(y * self.img_w + x1) * 3]);
                }
                crate::convert::export_srgb_jpeg(&sub, cw, ch, self.ev, self.hdr, &out)?;
            }
            None => crate::convert::export_srgb_jpeg(&self.lin, self.img_w, self.img_h, self.ev, self.hdr, &out)?,
        }
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
        // With a crop armed, the crop IS the frame: fit and centre on it.
        let (rx0, ry0, rx1, ry1) = self.crop.unwrap_or((0, 0, self.img_w, self.img_h));
        let (rw, rh) = ((rx1 - rx0).max(1) as f32, (ry1 - ry0).max(1) as f32);
        let (aw, ah, _) = self.image_area(viewport);
        let zoom = (aw / rw).min(ah / rh) * (1. - 1. / (1 << 6) as f32);
        self.zoom_rel = zoom / Self::area_span(aw, ah);
        self.cx_frac = (rx0 as f32 + rw / 2.) / self.img_w as f32;
        self.cy_frac = (ry0 as f32 + rh / 2.) / self.img_h as f32;
    }

    /// Continuous image coordinate of a screen point (unclamped — the caller decides what off-image means).
    fn image_pt(&self, x: f32, y: f32, viewport: Viewport) -> (f32, f32) {
        let (zoom, ox, oy) = self.view_px(viewport);
        ((x - ox) / zoom, (y - oy) / zoom)
    }

    /// Move crop corner `k` to image point (px, py), clamped to the frame, then re-normalise so x0 < x1 and y0 < y1 (a corner dragged past its opposite just swaps roles) with at least one pixel of extent.
    fn move_crop_corner(&mut self, k: usize, px: f32, py: f32) {
        let Some((mut x0, mut y0, mut x1, mut y1)) = self.crop else { return };
        let nx = px.round().clamp(0., self.img_w as f32) as usize;
        let ny = py.round().clamp(0., self.img_h as f32) as usize;
        match k {
            0 => (x0, y0) = (nx, ny),
            1 => (x1, y0) = (nx, ny),
            2 => (x0, y1) = (nx, ny),
            _ => (x1, y1) = (nx, ny),
        }
        let (ax0, ax1) = (x0.min(x1), x0.max(x1).max(x0.min(x1) + 1).min(self.img_w));
        let (ay0, ay1) = (y0.min(y1), y0.max(y1).max(y0.min(y1) + 1).min(self.img_h));
        self.crop = Some((ax0.min(ax1 - 1), ay0.min(ay1 - 1), ax1, ay1));
    }

    /// Crop mode on/off — `C` and the Crop pill share this. On: the rect seeds as the full frame (nothing dimmed yet; pull corners in), off: cleared. Both refit, so the composition always frames what's armed.
    fn toggle_crop(&mut self, ctx: &mut Context) {
        if self.img_w == 0 {
            return;
        }
        self.crop = if self.crop.is_none() { Some((0, 0, self.img_w, self.img_h)) } else { None };
        self.crop_drag = None;
        self.btn_crop.set_fill(self.crop.is_some().then_some(INFO_ON_FILL));
        self.fit(ctx.viewport);
        ctx.window.request_redraw();
    }

    /// Rotate the DISPLAY 90° (cw or ccw): compose onto the orientation view op in the retained decode, re-render the linear buffer from it (the plane itself never moves), carry the crop rect through the same rotation, refit. `V` records the composed orientation; the JPEG lands the way the screen shows.
    fn rotate(&mut self, cw: bool, ctx: &mut Context) {
        let Some(dec) = self.dec.as_mut() else { return };
        let code = crate::convert::rotate_code(crate::convert::orientation_code(&dec.img), cw);
        let op = vsf::spectral_image::ViewOp { name: "orientation".to_string(), class: vsf::spectral_image::IdtClass::Technical, params: vec![code as f32] };
        match &mut dec.img.view {
            Some(v) => match v.ops.iter_mut().find(|o| o.name == "orientation") {
                Some(o) => o.params = vec![code as f32],
                None => v.ops.insert(0, op),
            },
            None => dec.img.view = Some(vsf::spectral_image::ViewTransform { space: "vsf_rgb_linear".to_string(), ops: vec![op] }),
        }
        let (w, h, lin) = match crate::convert::to_linear(dec) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("opsin: rotate: {e}");
                return;
            }
        };
        // The crop rides the rotation: CW maps (x, y) → (H − y, x) on the old W×H frame; CCW maps (x, y) → (y, W − x).
        let (ow, oh) = (self.img_w, self.img_h);
        self.crop = self.crop.map(|r| rotate_rect(r, ow, oh, cw));
        self.img_w = w;
        self.img_h = h;
        self.lin = lin;
        self.raw.orient = code;
        self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
        self.tools = PanelTools::new(&self.pixels, w, h);
        self.fit(ctx.viewport);
        ctx.window.request_redraw();
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

    /// HDR rolloff on/off — `H` and the HDR pill share this. A re-encode, same cost as an EV tick.
    fn toggle_hdr(&mut self, ctx: &mut Context) {
        self.hdr = !self.hdr;
        self.btn_hdr.set_fill(self.hdr.then_some(INFO_ON_FILL));
        if !self.lin.is_empty() {
            self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
            self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
        }
        ctx.window.request_redraw();
    }

    /// Frame-info HUD on/off — the `I` key and the Info pill share this; the pill's fill tracks the state.
    fn toggle_info(&mut self, ctx: &mut Context) {
        self.show_info = !self.show_info;
        self.btn_info.set_fill(self.show_info.then_some(INFO_ON_FILL));
        ctx.window.request_redraw();
    }

    /// The image pixel under screen point (x, y), if it's on the drawn image.
    fn image_px_at(&self, x: f32, y: f32, viewport: Viewport) -> Option<(usize, usize)> {
        if !self.on_image(x, y, viewport) {
            return None;
        }
        let (zoom, ox, oy) = self.view_px(viewport);
        Some((((x - ox) / zoom) as usize, ((y - oy) / zoom) as usize))
    }

    /// The HUD's lines: file → camera/sensor → exposure → levels → IDT (grade, class, illuminant, the nine numbers) → live view state → the raw samples and linear display value under the cursor. Everything here is a reading, not an interpretation: raw counts are the sensor's own ADC codes, `lin` is the signed Rec.2020 the display encodes from (white = 1).
    fn info_lines(&self) -> Vec<String> {
        let Some(dec) = &self.dec else { return Vec::new() };
        let img = &dec.img;
        let mut v = Vec::new();
        let size = |b: u64| if b >= 1 << 20 { format!("{:.1} MB", b as f64 / (1u64 << 20) as f64) } else { format!("{:.0} kB", b as f64 / 1024.) };
        let rat = |(n, d): (u32, u32)| if d == 0 { 0. } else { n as f64 / d as f64 };
        let meta = self.meta.as_ref();
        let datetime = meta.and_then(|m| m.datetime.clone()).unwrap_or_default();
        v.push(format!("{}  {}  {datetime}", self.file_name, size(self.file_size)));
        let layout = match &img.layout {
            vsf::spectral_image::PlaneLayout::Mosaic { cfa } => format!("CFA {}×{} {:?}", cfa.shape[1], cfa.shape[0], cfa.data),
            vsf::spectral_image::PlaneLayout::Planar => "planar".to_string(),
        };
        v.push(format!("{} {}  {}×{}  {} ch  {}-bit  {layout}", img.make, img.model, img.width, img.height, img.channel_count(), img.bit_depth()));
        if let Some(m) = meta {
            let mut parts = Vec::new();
            if let Some(f) = m.focal { parts.push(format!("{} mm", trim_f(rat(f), 3))); }
            if let Some(f) = m.f_number { parts.push(format!("f/{:.1}", rat(f))); }
            if let Some(t) = m.exposure_s {
                let t = rat(t);
                parts.push(if t > 0. && t < 1. { format!("1/{:.0} s ({:.4} s)", 1. / t, t) } else { format!("{t:.3} s") });
            }
            if let Some(i) = m.iso { parts.push(format!("ISO {i}")); }
            if let Some(b) = m.baseline_exposure { parts.push(format!("baseline {:+.2} EV", if b.1 == 0 { 0. } else { b.0 as f64 / b.1 as f64 })); }
            if !parts.is_empty() { v.push(parts.join("  ")); }
        }
        let orient = crate::convert::orientation_code(img);
        let level = |l: &[f32]| if l.iter().all(|v| v == &l[0]) { trim_f(l[0] as f64, 2) } else { format!("{l:?}") };
        v.push(format!("black {}  white {}  orientation {orient}", level(&img.black), level(&img.white)));
        match img.profile.as_ref().and_then(|p| p.entries.first().map(|e| (p, e))) {
            Some((p, e)) => {
                let ill = match e.illuminant { 17 | 2 => "A", 20 => "D55", 21 => "D65", 22 => "D75", 23 => "D50", 0 => "-", _ => "?" };
                let name = meta.and_then(|m| m.profile_name.clone()).map(|n| format!("  \"{n}\"")).unwrap_or_default();
                v.push(format!("IDT {}  {}  {}  {ill}{name}", e.source, e.grade.as_str(), e.class.as_str()));
                if let Some((m, _)) = p.dng_colormatrix[0] {
                    for r in 0..3 {
                        v.push(format!("   {:>9.5} {:>9.5} {:>9.5}", m[r * 3], m[r * 3 + 1], m[r * 3 + 2]));
                    }
                }
            }
            None => v.push(if matches!(&img.layout, vsf::spectral_image::PlaneLayout::Mosaic { .. }) { "IDT none — uncalibrated (identity ColorMatrix), rendering raw camera".to_string() } else { "IDT none".to_string() }),
        }
        v
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

        // Two rows: [1:1][mag][Fit][JPEG][Info] over [CCW][CW][Crop], a half-pad between.
        let btn_h = if self.img_w > 0 { band * 2 + pad / 2 } else { 0 };
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

/// Carry a display-space rect `[x0, x1) × [y0, y1)` on an `ow × oh` frame through a 90° turn of that frame. CW maps a point (x, y) → (oh − 1 − y, x), so the x-extent becomes [oh − y1, oh − y0) and the y-extent [x0, x1); CCW maps (x, y) → (y, ow − 1 − x).
fn rotate_rect((x0, y0, x1, y1): (usize, usize, usize, usize), ow: usize, oh: usize, cw: bool) -> (usize, usize, usize, usize) {
    if cw { (oh - y1, x0, oh - y0, x1) } else { (y0, ow - x1, y1, ow - x0) }
}

/// Fixed-precision float with trailing zeros (and a bare point) trimmed — "6.9", not "6.900".
fn trim_f(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
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
        f(&mut self.btn_export);
        f(&mut self.btn_info);
        f(&mut self.btn_hdr);
        f(&mut self.btn_crop);
        f(&mut self.btn_rot_ccw);
        f(&mut self.btn_rot_cw);
        f(&mut self.btn_xscale);
        f(&mut self.btn_yscale);
        f(&mut self.btn_clip);
    }
}

/// The single-instance socket path once bound (so main can unlink it on exit). A process-wide cell because the app is moved into the host's event loop and main never sees it again.
static SOCKET: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

impl OpsinApp {
    pub fn socket_cell() -> &'static std::sync::Mutex<Option<PathBuf>> {
        &SOCKET
    }
}

impl FluorApp for OpsinApp {
    /// A path handed over by a later `opsin <file>` launch (see `instance.rs`); `None` = bare `opsin`, just raise the window.
    type UserEvent = Option<PathBuf>;

    /// The host's wake-sender arrives once before init: become the single instance now — bind the socket and ship the sender to the listener thread, which forwards each handed-over path to `on_user_event`.
    fn set_event_proxy(&mut self, proxy: std::sync::Arc<dyn fluor::host::WakeSender<Self::UserEvent>>) {
        if let Some(sock) = crate::instance::listen(move |path| {
            let _ = proxy.send(path);
        }) {
            if let Ok(mut cell) = SOCKET.lock() {
                *cell = Some(sock);
            }
        }
    }

    fn on_user_event(&mut self, event: Self::UserEvent, ctx: &mut Context) -> EventResponse {
        if let Some(path) = event {
            self.show_path(&path, ctx);
        }
        // Front + focus even if the load failed (the user just asked for this window) — WITHOUT moving it: the composition, panel, and window rect are the operator's and survive a handoff.
        EventResponse::Raise
    }

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
                if let Some(k) = self.crop_drag {
                    let (px, py) = self.image_pt(cx, cy, ctx.viewport);
                    self.move_crop_corner(k, px, py);
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
                // HUD cursor readout: repaint only when the IMAGE pixel under the cursor changes (or the cursor leaves the image), never per screen pixel.
                if self.show_info && self.img_w > 0 {
                    let px = self.image_px_at(cx, cy, ctx.viewport);
                    if px != self.info_px {
                        self.info_px = px;
                        dirty = true;
                    }
                }
                // Pill button hover — driven by the same stamped hit map as the chrome controls.
                for b in [&mut self.btn_one, &mut self.btn_fit, &mut self.btn_export, &mut self.btn_info, &mut self.btn_hdr, &mut self.btn_crop, &mut self.btn_rot_ccw, &mut self.btn_rot_cw, &mut self.btn_xscale, &mut self.btn_yscale, &mut self.btn_clip] {
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
                    if self.btn_export.take_click() {
                        // Same action as the E key — sRGB JPEG beside the source, live exposure baked.
                        match self.export_current_jpeg() {
                            Ok(out) => println!("opsin: wrote {}", out.display()),
                            Err(e) => eprintln!("opsin: JPEG export failed: {e}"),
                        }
                    }
                    if self.btn_info.take_click() {
                        self.toggle_info(ctx);
                    }
                    if self.btn_hdr.take_click() {
                        self.toggle_hdr(ctx);
                    }
                    if self.btn_crop.take_click() {
                        self.toggle_crop(ctx);
                    }
                    if self.btn_rot_ccw.take_click() {
                        self.rotate(false, ctx);
                    }
                    if self.btn_rot_cw.take_click() {
                        self.rotate(true, ctx);
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
                        // Clip indicator: lumis's preview_sub inversion at the encode boundary — after the magic-9 and the EV gain, so it marks display clipping under the CURRENT exposure and tracks the slider live. A cheap re-encode, same cost as an EV tick; `lin` is untouched (the JPEG export stays clean of it by construction).
                        self.clip_show = !self.clip_show;
                        self.btn_clip.set_fill(self.clip_show.then_some(CLIP_ON_FILL));
                        if !self.lin.is_empty() {
                            self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
                            self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
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
                    if self.crop.is_some() {
                        // Crop mode: the nearest corner comes to the cursor and follows it until release. Pan is the navigator's job while cropping.
                        let (px, py) = self.image_pt(ctx.cursor_x, ctx.cursor_y, ctx.viewport);
                        let (x0, y0, x1, y1) = self.crop.unwrap();
                        let corners = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
                        let d2 = |(cx, cy): (usize, usize)| (cx as f32 - px).powi(2) + (cy as f32 - py).powi(2);
                        let k = (0..4).min_by(|&a, &b| d2(corners[a]).total_cmp(&d2(corners[b]))).unwrap();
                        self.crop_drag = Some(k);
                        self.move_crop_corner(k, px, py);
                        ctx.window.request_redraw();
                        return EventResponse::Handled;
                    }
                    self.drag = Some((ctx.cursor_x, ctx.cursor_y));
                    return EventResponse::Handled;
                }
                // Backdrop (letterbox margin) — move the window, panes convention.
                EventResponse::StartWindowDrag
            }
            FEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left } => {
                self.drag = None;
                self.crop_drag = None;
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
                let ctrl = ctx.modifiers.control_key() || ctx.modifiers.super_key();
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
                    // Ctrl+C / Ctrl+V: the IDT clipboard — copy the current frame's DSR magic-9, paste it into the current frame (same camera only). Checked BEFORE the bare V (convert) arm so the modifier wins.
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") => {
                        match self.dir_list.get(self.dir_idx).ok_or_else(|| "no image loaded".to_string()).and_then(|p| crate::idt::IdtClip::copy_from(p)).and_then(|clip| clip.save().map(|path| (clip, path))) {
                            Ok((clip, path)) => println!("opsin: copied IDT from {} → {}\n{}", clip.source, path.display(), clip.to_text()),
                            Err(e) => eprintln!("opsin: copy IDT failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") => {
                        let Some(target) = self.dir_list.get(self.dir_idx).cloned() else {
                            eprintln!("opsin: paste IDT: no image loaded");
                            return EventResponse::Handled;
                        };
                        // Shift = force: paste over a fingerprint mismatch (the lens-change case). The report line carries the WARNING.
                        let force = ctx.modifiers.shift_key();
                        match crate::idt::IdtClip::load().and_then(|clip| clip.paste_into(&target, force)) {
                            Ok(report) => {
                                println!("opsin: {report}");
                                // The file changed under us — reload so the display renders through the pasted IDT.
                                self.show_path(&target, ctx);
                            }
                            Err(e) => eprintln!("opsin: paste IDT failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                        match self.convert_current_to_vsf() {
                            Ok(out) => println!("opsin: wrote {}", out.display()),
                            Err(e) => eprintln!("opsin: convert to VSF failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("e") => {
                        match self.export_current_jpeg() {
                            Ok(out) => println!("opsin: wrote {}", out.display()),
                            Err(e) => eprintln!("opsin: JPEG export failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("i") => {
                        self.toggle_info(ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("h") => {
                        self.toggle_hdr(ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if !ctrl && c.eq_ignore_ascii_case("c") => {
                        self.toggle_crop(ctx);
                        EventResponse::Handled
                    }
                    // r = rotate CW, R (shift) = CCW.
                    Key::Character(c) if c.eq_ignore_ascii_case("r") => {
                        self.rotate(!ctx.modifiers.shift_key(), ctx);
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
                        let v = slider_of_ev(self.ev + delta);
                        self.ev_slider.set_value(v);
                        self.apply_ev(v, ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if c == "0" => {
                        self.ev_slider.set_value(EV_ZERO);
                        self.apply_ev(EV_ZERO, ctx);
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
            // 1:1 / magnification / Fit / JPEG / Info — the band splits in fifths: fluor pill Buttons (same widget family as the slider and chrome) around the LIVE magnification readout (screen px per image px: 1x = pixel-exact certificate, 2.00x = zoomed in past it). The readout derives from the same per-frame span-relative transform as the blit, so it tracks every window op — press 1:1, resize, and it honestly drifts; that's the relative model reporting itself. JPEG is the export pill — the visible face of the E key.
            let (bx, by, bw, bh2) = btns;
            if bw > 2 && bh2 > 0 && self.img_w > 0 {
                let band = (ctx.viewport.effective_span() / (1 << 6) as f32).ceil() as usize;
                let bh = band.min(bh2);
                let quarter = bw as f32 / 5.;
                let font = bh as f32 / 2.;
                let bcy = by as f32 + bh as f32 / 2.;
                // Row 2: rotate CCW / CW, crop — thirds.
                let r2y = (by + bh2 - bh) as f32 + bh as f32 / 2.;
                let third = bw as f32 / 3.;
                for (i, b) in [&mut self.btn_rot_ccw, &mut self.btn_rot_cw, &mut self.btn_crop].into_iter().enumerate() {
                    b.set_rect(bx as f32 + third * (i as f32 + 0.5), r2y, third, bh as f32);
                    b.set_font_size(font);
                    let id = b.hit_id();
                    b.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                }
                self.btn_one.set_rect(bx as f32 + quarter * 0.5, bcy, quarter, bh as f32);
                self.btn_one.set_font_size(font);
                self.btn_fit.set_rect(bx as f32 + quarter * 2.5, bcy, quarter, bh as f32);
                self.btn_fit.set_font_size(font);
                self.btn_export.set_rect(bx as f32 + quarter * 3.5, bcy, quarter, bh as f32);
                self.btn_export.set_font_size(font);
                self.btn_info.set_rect(bx as f32 + quarter * 4.5, bcy, quarter, bh as f32);
                self.btn_info.set_font_size(font);
                let id = self.btn_info.hit_id();
                self.btn_info.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                let id = self.btn_one.hit_id();
                self.btn_one.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                let id = self.btn_fit.hit_id();
                self.btn_fit.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                let id = self.btn_export.hit_id();
                self.btn_export.render_content_into(&mut canvas, 0., 0., ctx.text, clip, Some(&mut self.chrome.hit_test_map), id);
                // "1x" is a CERTIFICATE, not a rounding: bitwise == against the value one_to_one() stored, so it holds iff the span (window + divider geometry) is unchanged since — the moment a resize makes the image resample, equality breaks and the decimals return. Resize back to the identical geometry and exactness honestly comes back.
                let (aw, ah, _) = self.image_area(ctx.viewport);
                let magnification = if self.zoom_rel == 1. / Self::area_span(aw, ah) {
                    "1x".to_string()
                } else {
                    // TRUNCATED to two decimals, never rounded — the readout may understate but never overstate: 0.9999999 reads "0.99x" (it is NOT yet 1; only the == certificate may say "1x"). Integer decomposition so the formatter can't re-round.
                    let centi = (zoom * 100.).trunc() as u64;
                    format!("{}.{:02}x", centi / 100, centi % 100)
                };
                ctx.text.draw_text_center(&mut canvas, &magnification, bx as f32 + quarter * 1.5, (by + bh / 2) as f32, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
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

            // Histogram pills in their OWN band carved from the top of the hist rect — above the plot, never on it. Right-aligned row: [HDR][X ..][Y ..][Clip]. Geometry derives from the hist rect (RU-coherent, no pixel constants).
            let (hx, hy, hw, hh) = hist;
            let pill_h = (hh as f32 / 5.).max(8.);
            let pill_pad = hh as f32 / (1 << 4) as f32;
            let pill_band = (pill_h + pill_pad * 2.) as usize;
            if hw > 0 && hh > pill_band && !self.raw.counts.is_empty() {
                // Four pills must fit the row: shrink from the 3:1 pill when the panel is narrow rather than spill over the divider.
                let pill_w = (pill_h * 3.).min((hw as f32 - pill_pad * 5.) / 4.);
                let cy_pill = hy as f32 + pill_pad + pill_h / 2.;
                let mut right = hx as f32 + hw as f32 - pill_pad;
                for b in [&mut self.btn_clip, &mut self.btn_yscale, &mut self.btn_xscale, &mut self.btn_hdr] {
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
                let dens = self.raw.spread(&codes, self.hist_xlog, bins, 2f32.powf(self.ev));
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
        // Navigator crop rect — the armed crop in thumb space, same XOR marker so it reads on any content (a slitscan band is a sliver of a 32k-tall strip; this is where you see where it sits).
        if let (Some((fx, fy, fw, fh)), Some((x0, y0, x1, y1))) = (fitted, self.crop) {
            let tx = |x: usize| (fx + x * fw / self.img_w.max(1)) as isize;
            let ty = |y: usize| (fy + y * fh / self.img_h.max(1)) as isize;
            let (rx0, ry0, rx1, ry1) = (tx(x0), ty(y0), tx(x1).min((fx + fw) as isize - 1), ty(y1).min((fy + fh) as isize - 1));
            let mut mark = |x: isize, y: isize| {
                if x >= 0 && y >= 0 && (x as usize) < buf_w && (y as usize) < buf_h {
                    target[y as usize * buf_w + x as usize] ^= 0x0080_8080;
                }
            };
            for x in rx0..=rx1 {
                mark(x, ry0);
                mark(x, ry1);
            }
            for y in (ry0 + 1)..ry1 {
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

        // Frame info HUD — bottom-left of the image area, drawn BEFORE the image so the under-compose order puts it on top: static frame lines from `info_lines`, then the live view state and the cursor readout (raw ADC codes of every sample in the tile under the cursor + the linear display value). A translucent dark backdrop keeps it legible over any content.
        if self.show_info && self.img_w > 0 {
            let band = (ctx.viewport.effective_span() / (1 << 6) as f32).ceil();
            // Readable at a glance, not a footnote: the same size as the pill labels' band, ~2× the panel captions.
            let font = band * 0.9;
            let line_h = font * 1.3;
            let pad = band / 2.;
            let mut lines = self.info_lines();
            lines.push(format!("EV {:+.2}  zoom {}x  clip {}  hdr {}", self.ev, trim_f(zoom as f64, 3), if self.clip_show { "on" } else { "off" }, if self.hdr { "on (3x−x³)/2" } else { "off" }));
            if let Some((x0, y0, x1, y1)) = self.crop {
                lines.push(format!("crop {x0},{y0}  {}×{}  (click: nearest corner to cursor)", x1 - x0, y1 - y0));
            }
            if let Some((px, py)) = self.image_px_at(ctx.cursor_x, ctx.cursor_y, ctx.viewport) {
                let names: Vec<String> = self.dec.as_ref().map(|d| d.img.channels.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
                let raw: Vec<String> = self.raw.samples_at(px, py).into_iter().map(|(ch, v)| format!("{} {v}", names.get(ch).map(String::as_str).unwrap_or("?"))).collect();
                let i = (py * self.img_w + px) * 3;
                let lin = if i + 2 < self.lin.len() { format!("  lin {:.4} {:.4} {:.4}", self.lin[i] as f32 / 65535., self.lin[i + 1] as f32 / 65535., self.lin[i + 2] as f32 / 65535.) } else { String::new() };
                lines.push(format!("px {px},{py}  raw {}{lin}", raw.join(" ")));
            }
            let style = fluor::text::TextStyle::new(font, TEXT_GREY);
            let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
            let x0 = pad;
            let box_h = line_h * lines.len() as f32 + pad;
            let y_top = buf_h as f32 - pad - box_h;
            let mut max_w: f32 = 0.;
            for (i, line) in lines.iter().enumerate() {
                let y = y_top + pad / 2. + line_h * (i as f32 + 0.5);
                let w = ctx.text.draw_text_left(&mut canvas, line, x0 + pad / 2., y, &style, clip, None);
                max_w = max_w.max(w);
            }
            let box_w = (max_w + pad).min(divider_px as f32 - x0);
            paint::fill_rect(&mut canvas, x0 as isize, y_top as isize, box_w as isize, box_h as isize, HUD_BG, clip, None);
        }

        // Crop overlay — drawn BEFORE the image so under-compose puts it on top: everything outside the rect dims to the HUD tone, and a hairline rings the rect. Screen coords straight from the same per-frame transform as the blit.
        if let Some((x0, y0, x1, y1)) = self.crop {
            let mut canvas = Canvas::new(target, buf_w, buf_h, ctx.damage);
            let sx0 = (img_ox + x0 as f32 * zoom).round() as isize;
            let sy0 = (img_oy + y0 as f32 * zoom).round() as isize;
            let sx1 = (img_ox + x1 as f32 * zoom).round() as isize;
            let sy1 = (img_oy + y1 as f32 * zoom).round() as isize;
            let (ax0, ay0, ax1, ay1) = (0isize, bar_h as isize, divider_px as isize, buf_h as isize);
            // Four bands: above, below, left, right of the rect — clipped to the image area.
            let band = |c: &mut Canvas, x0: isize, y0: isize, x1: isize, y1: isize| {
                let (x0, y0, x1, y1) = (x0.max(ax0), y0.max(ay0), x1.min(ax1), y1.min(ay1));
                if x1 > x0 && y1 > y0 {
                    paint::fill_rect(c, x0, y0, x1 - x0, y1 - y0, HUD_BG, clip, None);
                }
            };
            band(&mut canvas, ax0, ay0, ax1, sy0);
            band(&mut canvas, ax0, sy1, ax1, ay1);
            band(&mut canvas, ax0, sy0, sx0, sy1);
            band(&mut canvas, sx1, sy0, ax1, sy1);
            for (x, y, w, h) in [(sx0, sy0, sx1 - sx0, 0), (sx0, sy1 - 1, sx1 - sx0, 0), (sx0, sy0, 0, sy1 - sy0), (sx1 - 1, sy0, 0, sy1 - sy0)] {
                if x >= ax0 && x < ax1 && y >= ay0 && y < ay1 {
                    paint::fill_rect(&mut canvas, x, y, w.min(ax1 - x), h.min(ay1 - y), TEXT_GREY, clip, None);
                }
            }
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
        if [self.btn_one.hit_id(), self.btn_fit.hit_id(), self.btn_export.hit_id(), self.btn_info.hit_id(), self.btn_hdr.hit_id(), self.btn_crop.hit_id(), self.btn_rot_ccw.hit_id(), self.btn_rot_cw.hit_id(), self.btn_xscale.hit_id(), self.btn_yscale.hit_id(), self.btn_clip.hit_id()].contains(&hit) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Darkness byte of the encoded pixel's red channel (255 = black, 0 = white).
    fn darkness_r(px: u32) -> u32 {
        (px >> 16) & 0xFF
    }

    #[test]
    fn clip_indicator_marks_at_display_boundary_after_gain() {
        // One pixel per case, red channel carries the probe value; green/blue mid-grey.
        let mid = 20000;
        let lin = vec![
            70000, mid, mid, // over display white at EV 0
            -500, mid, mid, // below black at EV 0
            40000, mid, mid, // legal at EV 0, blows past white at +1 EV
        ];
        // Indicator OFF: everything clamps, over-white renders WHITE (darkness 0), negative renders BLACK (darkness 255).
        let plain = encode_pixels(&lin, 0., false, false);
        assert_eq!(darkness_r(plain[0]), 0);
        assert_eq!(darkness_r(plain[1]), 255);
        // Indicator ON at EV 0: preview_sub inversion — over-white DARK, under-black BLOWN; the legal pixel unaffected.
        let clip0 = encode_pixels(&lin, 0., true, false);
        assert_eq!(darkness_r(clip0[0]), 255, "blown renders dark");
        assert_eq!(darkness_r(clip0[1]), 0, "crushed renders blown");
        assert_eq!(darkness_r(clip0[2]), darkness_r(plain[2]), "legal pixel untouched");
        // Indicator ON at +1 EV: the 40000 pixel now exceeds display white and flips to dark — the indicator tracks the live exposure.
        let clip1 = encode_pixels(&lin, 1., true, false);
        assert_eq!(darkness_r(clip1[2]), 255, "EV pushes it past white; the mark follows");
    }

    #[test]
    fn hdr_rail_is_photons_cubic_on_the_display_domain() {
        use crate::convert::hdr_rail as f;
        assert_eq!(f(0), 0);
        assert_eq!(f(65535), 65535);
        // Matches the real-valued (3x − x³)/2 to within one code everywhere; monotonic; never below identity (brightens, never darkens); slope 3/2 at black.
        let mut prev = 0;
        for x in 0..=65535i64 {
            let y = f(x);
            let exact = (3. * x as f64 - (x as f64).powi(3) / 65535f64.powi(2)) / 2.;
            assert!((y as f64 - exact).abs() <= 1.5, "x={x} y={y} exact={exact}");
            assert!(y >= prev && y >= x, "x={x}");
            prev = y;
        }
        assert_eq!(f(1000), 1500);
        // Overs pin to the rail (the cubic folds back past it — the clamp is load-bearing).
        assert_eq!(f(80000), 65535);
        // The viewer's HDR encode = plain encode of the railed value: screen and export share the one definition.
        let lin = vec![32768, 32768, 32768];
        let hdr = encode_pixels(&lin, 0., false, true);
        let curved = vec![f(32768) as i32; 3];
        let plain = encode_pixels(&curved, 0., false, false);
        assert_eq!(darkness_r(hdr[0]), darkness_r(plain[0]));
        // Clip indicator still fires on the pre-curve boundary under HDR.
        assert_eq!(darkness_r(encode_pixels(&[70000, 100, 100], 0., true, true)[0]), 255);
    }

    #[test]
    fn rotate_rect_rides_the_frame() {
        // A 10×4 frame, rect x∈[2,5) y∈[1,3). CW: the frame becomes 4×10, the rect's x-extent is the old y mirrored: [4−3, 4−1) = [1,3), y-extent = old x [2,5).
        let r = (2, 1, 5, 3);
        assert_eq!(rotate_rect(r, 10, 4, true), (1, 2, 3, 5));
        // CCW undoes CW (dims swap in between); four CW turns are the identity.
        assert_eq!(rotate_rect(rotate_rect(r, 10, 4, true), 4, 10, false), r);
        let mut x = r;
        let (mut w, mut h) = (10, 4);
        for _ in 0..4 {
            x = rotate_rect(x, w, h, true);
            std::mem::swap(&mut w, &mut h);
        }
        assert_eq!(x, r);
    }

    #[test]
    fn spread_gain_shifts_bins_and_grows_clip_spike() {
        let raw = RawView { counts: Vec::new(), sensor_w: 0, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: 0, black: [0.; 3], white: [65535.; 3], bits: 16, orient: 1, pre_w: 0, pre_h: 0, census: [1.; 3] };
        let bins = 64;
        let mut codes = vec![[0u32; 3]; 1 << 16];
        codes[16000][0] = 100; // quarter-scale population
        let d0 = raw.spread(&codes, false, bins, 1.);
        let d1 = raw.spread(&codes, false, bins, 2.);
        let peak = |d: &Vec<[f32; 3]>| d.iter().enumerate().max_by(|a, b| a.1[0].total_cmp(&b.1[0])).map(|(i, _)| i).unwrap();
        // Linear x: doubling the gain doubles the peak's bin position (±1 for the bin floor).
        assert!((peak(&d1) as i32 - 2 * peak(&d0) as i32).abs() <= 1, "peaks {} vs {}", peak(&d1), peak(&d0));
        // Gain pushing data past white collapses it into the last bin — the display-clip spike.
        codes[16000][0] = 0;
        codes[50000][0] = 77;
        let d2 = raw.spread(&codes, false, bins, 2.);
        assert!(d2[bins - 1][0] >= 77. * 0.99, "overspill lands whole in the clip bin, got {}", d2[bins - 1][0]);
    }
}
