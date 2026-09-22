//! The VIEW — opsin's whole viewer interface as an embeddable component (Nick 2026-09-17: "implement the same interface opsin has in Photon for the image viewer, DNG and all, exposure slider, etc."): the image area with pan/zoom/crop/rotate, the right tool panel (navigator, 1:1/Fit/export/Info, CCW/CW/Crop, histogram with its HDR/X/Y/Clip pills, the exposure slider, the chromaticity chart), the frame-info HUD, and every key binding. It draws into a rectangle the host gives it, stamps its pills into the host's hit map under ids the host allocated, and answers events the host forwards — so the opsin window ([`crate::app::OpsinApp`]) and photon's attachment viewer are the SAME viewer, not two that drift.
//!
//! What stays with the host: window chrome, the top bar and window moves/resizes, folder navigation (arrows), the IDT clipboard (needs the file path), drag-and-drop, the debug chord. The host tells the view its area every frame, forwards pointer/wheel/key events with the hit id it resolved under the cursor, and calls `render` with its canvas and hit map.

use fluor::canvas::Canvas;
use fluor::coord::Coord;
use fluor::event::{CursorIcon, ElementState, Event as FEvent, Key, MouseButton, MouseScrollDelta, NamedKey};
use fluor::geom::Viewport;
use fluor::host::app::{Context, EventResponse};
use fluor::host::chrome::{HIT_NONE, HitId};
use fluor::host::widget::Container;
use fluor::paint::{self, Clip};
use fluor::pixel::{Blend, BlendMode};

use std::path::{Path, PathBuf};

use crate::panel::{HIST_OVERSAMPLE, Observer, PanelTools};

/// Empty-canvas backdrop behind/around the image: opaque near-black in α+darkness packing (α=0xFF, darkness ≈ high = dark visible).
pub const BACKDROP: u32 = 0xFF_F2_F2_F2;
/// Panel background — a shade above the backdrop so the tool area reads as a surface.
const PANEL_BG: u32 = 0xFF_E6_E6_E6;
/// Const-context version of `paint::pack_argb` — same visible-RGB → α+darkness packing.
pub(crate) const fn argb(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((a as u32) << 24) | (((255 - r) as u32) << 16) | (((255 - g) as u32) << 8) | ((255 - b) as u32)
}
/// Divider + section hairlines: flat grey, same 1px weight as Photon's button strokes.
pub(crate) const HAIRLINE: u32 = argb(0x60, 0x60, 0x60, 0xFF);
/// Mic meter colours.
const METER_TROUGH: u32 = argb(0x14, 0x14, 0x14, 0xFF);
const METER_GREEN: u32 = argb(0x30, 0xC0, 0x50, 0xFF);
const METER_YELLOW: u32 = argb(0xE0, 0xC0, 0x20, 0xFF);
const METER_RED: u32 = argb(0xE0, 0x30, 0x30, 0xFF);
/// Panel label text (EV readout, button labels).
const TEXT_GREY: u32 = argb(0xE0, 0xE0, 0xE0, 0xFF);
/// Clip pill fill while the indicator is live — a warning red so the false-colour preview can't be mistaken for the image.
const CLIP_ON_FILL: u32 = argb(0x8B, 0x30, 0x30, 0xFF);
/// Info pill fill while the HUD is shown — a quiet blue-grey, distinct from the clip pill's warning red.
const INFO_ON_FILL: u32 = argb(0x30, 0x48, 0x70, 0xFF);
/// Key-hint text — a step dimmer than the readings, so the numbers stay the thing you read.
const HINT_GREY: u32 = argb(0x9A, 0x9A, 0x9A, 0xFF);
/// HUD backdrop — translucent near-black so the readings stay legible over any image content.
const HUD_BG: u32 = argb(0x10, 0x10, 0x10, 0xB8);

/// Exposure slider range in stops — asymmetric on purpose: −4 is as far as pulling down ever needs to go, but a sensor holds ~12 stops above its noise floor and the signed-linear pipe keeps every one of them, so pushing up runs all the way to +12 to blow a whole frame's highlights through the clip indicator. Gain at +12 is 2^12 · 2^16 (Q16) = 2^28 per i32 sample — nowhere near i64.
const EV_MIN: f32 = -((1 << 2) as f32);
const EV_MAX: f32 = (12) as f32;
/// Slider position (0..1) of 0 EV.
const EV_ZERO: f32 = -EV_MIN / (EV_MAX - EV_MIN);

/// Slider position of "as the file says" (operator EV 0). The baseline MOVES it: a file that declares +4 opens four stops up, so its zero sits four stops along the track, and the travel below it is what reaches below the capture.
fn ev_zero_of(baseline: f32) -> f32 {
    slider_of_ev(0., baseline)
}

/// Slider 0..1 → stops.
fn ev_of_slider(v: f32, baseline: f32) -> f32 {
    (EV_MIN - baseline) + v * (EV_MAX - EV_MIN)
}
/// Stops of operator exposure → slider 0..1. `baseline` is the file's declared opening gain, and the range brackets the TOTAL that reaches the screen — `EV_MIN..EV_MAX` of `ev + baseline`, not of `ev` alone. Without that, a file declaring +4 could only be pulled back to its own capture: the slider bottomed out at −4, which lands at total 0, where sensor saturation maps exactly to display white, so a blown sky stayed white however far you pulled and the preserved above-white data was unreachable (Nick 2026-09-22: "when I put a -4 in Opsin I still get white ... I KNOW there's data there").
fn slider_of_ev(ev: f32, baseline: f32) -> f32 {
    ((ev + baseline).clamp(EV_MIN, EV_MAX) - EV_MIN) / (EV_MAX - EV_MIN)
}

/// The rectangle the view owns on the host's surface, in pixels. The image area is its left part, the tool panel its right part (split by the draggable divider); in plain mode the image takes all of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Cross-thread messages into the host's UI thread for the view's background work.
pub enum Msg {
    /// A path handed over by a later `opsin <file>` launch (see `instance.rs`); `None` = bare `opsin`, just raise the window. The window's, not the view's — carried here so one enum covers the host.
    Open(Option<PathBuf>),
    /// A background target scan finished (the `calibrate` feature): the frame it ran on, and chameleon's verdict.
    #[cfg(feature = "calibrate")]
    Scan(PathBuf, Result<crate::calibrate::ScanOutcome, String>),
    /// From the live capture thread (`live` feature).
    #[cfg(feature = "live")]
    Live(crate::live::LiveMsg),
}

/// Panel section rects (x0, y0, w, h) — named fields, no position-coded indexing.
struct PanelRects {
    nav: (usize, usize, usize, usize),
    btns: (usize, usize, usize, usize),
    hist: (usize, usize, usize, usize),
    ev: (usize, usize, usize, usize),
    /// Live colour-matrix sliders: 3 rows of [r g b gain] + a Reset/Save row. Zero height unless live.
    #[allow(dead_code)]
    mat: (usize, usize, usize, usize),
    chart: (usize, usize, usize, usize),
}

/// One decoded image, ready to install into the view. Produced by [`load_image`] / [`load_image_folded`] / [`Loaded::from_decoded`] — off the UI thread where the host likes — and consumed by [`View::new`] (construction) and [`View::install`] (navigation). Keeps the linear RGB so exposure changes re-encode without re-decoding the source, the raw sensor view so the histogram bins actual counts per frame, and the HUD's frame lines so the decode itself can be dropped where memory is tight.
pub struct Loaded {
    pixels: Vec<u32>,
    lin: Vec<i32>,
    w: usize,
    h: usize,
    raw: RawView,
    /// The decode, retained where the host can afford it (rotation writes its orientation op here for the VSF convert; the calibrate scan grades its entry). `None` = folded/phone load: rotation and the clip toggle work from `lin` alone.
    dec: Option<crate::convert::Decoded>,
    title: String,
    file_name: String,
    /// The HUD's static frame lines (file, camera/sensor, exposure, levels, IDT) — computed once at load, so they outlive `dec`.
    frame_lines: Vec<String>,
    /// Channel names for the cursor readout.
    channel_names: Vec<String>,
    /// The file's declared opening gain (stops), already applied to `lin` by the render — carried only so the HUD can show what it contributed.
    baseline_ev: f32,
    /// The operator's exposure recorded IN THIS FILE, if it records one. `Some` overrides the carried slider position on install; `None` leaves it, which is what keeps arrowing through a folder of ungraded frames at one exposure.
    stored_ev: Option<f32>,
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
    /// The ORIENTED full-resolution dims, and the oriented dims the view actually holds (equal unless the load was folded) — the bridge scales a displayed pixel up to full resolution before the orientation inverse, so a folded phone load still bins the real sensor counts.
    or_w: usize,
    or_h: usize,
    fold_w: usize,
    fold_h: usize,
    /// Per-channel sample census of one CFA tile (Bayer: G = 2) — spread deposits weight by 1/census so green's double sampling stops inflating it (lumis's equal-energy channel weighting, generalized to any tile).
    census: [f32; 3],
}

impl RawView {
    fn empty() -> Self {
        Self { counts: Vec::new(), sensor_w: 0, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: 0, black: [0.; 3], white: [1.; 3], bits: 1, orient: 1, pre_w: 0, pre_h: 0, or_w: 0, or_h: 0, fold_w: 0, fold_h: 0, census: [1.; 3] }
    }

    fn from_image(img: &vsf::spectral_image::SpectralImage, or_w: usize, or_h: usize) -> Self {
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
                    or_w,
                    or_h,
                    fold_w: or_w,
                    fold_h: or_h,
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
                or_w,
                or_h,
                fold_w: or_w,
                fold_h: or_h,
                census: [1.; 3],
            },
        }
    }

    /// A displayed (possibly folded, oriented) pixel → the full-resolution oriented pixel → the pre-orientation sensor-tile coordinate.
    #[inline]
    fn sensor_of(&self, dx: usize, dy: usize) -> (usize, usize) {
        let ux = if self.fold_w == self.or_w { dx } else { (dx * self.or_w / self.fold_w.max(1)).min(self.or_w.saturating_sub(1)) };
        let uy = if self.fold_h == self.or_h { dy } else { (dy * self.or_h / self.fold_h.max(1)).min(self.or_h.saturating_sub(1)) };
        crate::convert::orientation_src(self.orient, self.pre_w, self.pre_h, ux, uy)
    }

    /// Tally every raw sample under display pixel (dx, dy) into the per-ADC-code table: orientation bridge → sensor tile → CFA channel routing (channels past 3 fold onto blue pending the spectral resolve). No axis math here — codes are exact, and [`Self::spread`] owns the mapping.
    #[inline]
    fn collect_codes(&self, dx: usize, dy: usize, codes: &mut [[u32; 3]]) {
        let (sx, sy) = self.sensor_of(dx, dy);
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
        if self.counts.is_empty() || dx >= self.fold_w || dy >= self.fold_h {
            return Vec::new();
        }
        let (sx, sy) = self.sensor_of(dx, dy);
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
            // Effective quantization step: files re-scaled after capture (e.g. a 10-bit frame stretched into 16-bit codes by older lumis saves) only populate every Nth code, and depositing over [v, v+1) would re-comb them. The MODE of the gaps between occupied codes is the honest estimator: a native file's mode is 1 (identical behaviour, bit for bit), a stretched file's is its stretch factor. Irregular stretches (65536/1023 alternates 64/65) leave sub-bin residue only. Scene-content gaps can't skew a mode the way a mean or a max would.
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
pub fn encode_pixels(lin: &[i32], ev: f32, clip_show: bool, hdr: bool) -> Vec<u32> {
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

/// Fold linear i32 RGB to `max_edge` on the long side: a box mean over the source block of each output pixel, integer all the way, the division remainder carried along the row per channel so the row's sum is exact instead of every pixel sitting up to an LSB low. Returns the input untouched when it already fits.
fn fold_linear(lin: Vec<i32>, w: usize, h: usize, max_edge: usize) -> (usize, usize, Vec<i32>) {
    use rayon::prelude::*;
    if w.max(h) <= max_edge || w == 0 || h == 0 {
        return (w, h, lin);
    }
    let (tw, th) = if w >= h { (max_edge, (h * max_edge / w).max(1)) } else { ((w * max_edge / h).max(1), max_edge) };
    let mut out = vec![0i32; tw * th * 3];
    out.par_chunks_mut(tw * 3).enumerate().for_each(|(ty, row)| {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        let mut rem = [0i64; 3];
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let mut acc = [0i64; 3];
            let mut n = 0i64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = (sy * w + sx) * 3;
                    acc[0] += lin[i] as i64;
                    acc[1] += lin[i + 1] as i64;
                    acc[2] += lin[i + 2] as i64;
                    n += 1;
                }
            }
            let n = n.max(1);
            for ch in 0..3 {
                let t = acc[ch] + rem[ch];
                let q = t.div_euclid(n);
                rem[ch] = t - q * n;
                row[tx * 3 + ch] = q as i32;
            }
        }
    });
    (tw, th, out)
}

/// Turn a W×H linear buffer 90°: CW maps source (x, y) to (H−1−y, x) on the H×W result, CCW to (y, W−1−x). The plane itself never moves anywhere else — this is the display's copy.
fn rotate_linear(lin: &[i32], w: usize, h: usize, cw: bool) -> Vec<i32> {
    let mut out = vec![0i32; lin.len()];
    let (nw, nh) = (h, w);
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = if cw { (h - 1 - y, x) } else { (y, w - 1 - x) };
            let s = (y * w + x) * 3;
            let d = (ny * nw + nx) * 3;
            out[d..d + 3].copy_from_slice(&lin[s..s + 3]);
        }
    }
    debug_assert_eq!(nw * nh * 3, out.len());
    out
}

/// Fixed-precision float with trailing zeros (and a bare point) trimmed — "6.9", not "6.900".
fn trim_f(v: f64, prec: usize) -> String {
    let s = format!("{v:.prec$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// The HUD's static frame lines: file → camera/sensor → exposure → levels → IDT (grade, class, illuminant, the nine numbers). Everything here is a reading, not an interpretation.
fn frame_lines(dec: &crate::convert::Decoded, meta: Option<&crate::tiff::FrameMeta>, file_name: &str, file_size: u64) -> Vec<String> {
    let img = &dec.img;
    let mut v = Vec::new();
    let size = |b: u64| if b >= 1 << 20 { format!("{:.1} MB", b as f64 / (1u64 << 20) as f64) } else { format!("{:.0} kB", b as f64 / 1024.) };
    let rat = |(n, d): (u32, u32)| if d == 0 { 0. } else { n as f64 / d as f64 };
    let datetime = meta.and_then(|m| m.datetime.clone()).unwrap_or_default();
    v.push(format!("{file_name}  {}  {datetime}", size(file_size)));
    let layout = match &img.layout {
        vsf::spectral_image::PlaneLayout::Mosaic { cfa } => format!("CFA {}×{} {:?}", cfa.shape[1], cfa.shape[0], cfa.data),
        vsf::spectral_image::PlaneLayout::Planar => "planar".to_string(),
    };
    v.push(format!("{} {}  {}×{}  {} ch  {}-bit  {layout}", img.make, img.model, img.width, img.height, img.channel_count(), dec.src_bits));
    if let Some(m) = meta {
        let mut parts = Vec::new();
        if let Some(f) = m.focal {
            parts.push(format!("{} mm", trim_f(rat(f), 3)));
        }
        if let Some(f) = m.f_number {
            parts.push(format!("f/{:.1}", rat(f)));
        }
        if let Some(t) = m.exposure_s {
            let t = rat(t);
            parts.push(if t > 0. && t < 1. { format!("1/{:.0} s ({:.4} s)", 1. / t, t) } else { format!("{t:.3} s") });
        }
        if let Some(i) = m.iso {
            parts.push(format!("ISO {i}"));
        }
        if let Some(b) = m.baseline_exposure {
            parts.push(format!("baseline {:+.2} EV", if b.1 == 0 { 0. } else { b.0 as f64 / b.1 as f64 }));
        }
        if !parts.is_empty() {
            v.push(parts.join("  "));
        }
    }
    let orient = crate::convert::orientation_code(img);
    let level = |l: &[f32]| if l.iter().all(|v| v == &l[0]) { trim_f(l[0] as f64, 2) } else { format!("{l:?}") };
    v.push(format!("black {}  white {}  orientation {orient}", level(&img.black), level(&img.white)));
    match img.profile.as_ref().and_then(|p| p.entries.first().map(|e| (p, e))) {
        Some((p, e)) => {
            let ill = match e.illuminant {
                17 | 2 => "A",
                20 => "D55",
                21 => "D65",
                22 => "D75",
                23 => "D50",
                0 => "-",
                _ => "?",
            };
            let name = meta.and_then(|m| m.profile_name.clone()).map(|n| format!("  \"{n}\"")).unwrap_or_default();
            v.push(format!("IDT {}  {}  {}  {ill}{name}", e.source, e.grade.as_str(), e.class.as_str()));
            if let Some((m, _)) = p.dng_colormatrix[0] {
                for r in 0..3 {
                    v.push(format!("   {:>9.5} {:>9.5} {:>9.5}", m[r * 3], m[r * 3 + 1], m[r * 3 + 2]));
                }
            }
        }
        // No profile is a statement, not a gap: the samples are VSF RGB by specification. A mosaic with no profile is the one shape that cannot literally be that (a CFA plane is sensor counts), so it says so.
        None => v.push(if matches!(&img.layout, vsf::spectral_image::PlaneLayout::Mosaic { .. }) { "IDT none — a mosaic with no characterization: sensor counts rendered as VSF RGB".to_string() } else { "IDT none — VSF RGB (untagged samples are VSF RGB by specification)".to_string() }),
    }
    v
}

impl Loaded {
    /// Assemble a view-ready image from a decode: the linear render (EXIF orientation applied by `to_linear`), display pixels at EV 0, the raw sensor view, the HUD lines. `max_edge` folds the display copy to that long edge (a phone keeps 2048; twelve bytes a pixel), the raw counts stay at full resolution for the histogram; `keep_decode = false` drops the decode once its facts are captured.
    pub fn from_decoded(dec: crate::convert::Decoded, meta: Option<crate::tiff::FrameMeta>, file_name: &str, file_size: u64, max_edge: Option<usize>, keep_decode: bool) -> Result<Loaded, String> {
        // `baseline_ev` is already on the decode — `load_any` read it from the headers, so every render path has it, not just this one. A VSF has nowhere to carry it yet, so a DNG→VSF convert still drops it.
        let stored_ev = crate::convert::stored_exposure_ev(&dec.img);
        let (w, h, lin) = crate::convert::to_linear(&dec)?;
        let mut raw = RawView::from_image(&dec.img, w, h);
        let (w, h, lin) = match max_edge {
            Some(e) => fold_linear(lin, w, h, e),
            None => (w, h, lin),
        };
        raw.fold_w = w;
        raw.fold_h = h;
        let baseline_ev = dec.baseline_ev;
        let pixels = encode_pixels(&lin, 0., false, false);
        let title = format!("opsin — {file_name} ({}×{}, {} ch, {}-bit)", dec.img.width, dec.img.height, dec.img.channel_count(), dec.src_bits);
        let frame_lines = frame_lines(&dec, meta.as_ref(), file_name, file_size);
        let channel_names = dec.img.channels.iter().map(|c| c.name.clone()).collect();
        Ok(Loaded { pixels, lin, w, h, raw, dec: keep_decode.then_some(dec), title, file_name: file_name.to_string(), frame_lines, channel_names, baseline_ev, stored_ev })
    }

    /// The empty drop-target state — no image, the panel's locus/Planck chart renders from the observer alone.
    pub fn empty() -> Self {
        Loaded { pixels: Vec::new(), lin: Vec::new(), w: 0, h: 0, raw: RawView::empty(), dec: None, file_name: String::new(), title: "opsin — drop an image".to_string(), frame_lines: Vec::new(), channel_names: Vec::new(), baseline_ev: 0., stored_ev: None }
    }

    pub fn dims(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    pub fn title(&self) -> &str {
        &self.title
    }
}

/// Decode `path` (any supported format) at full resolution, decode retained — the desktop viewer's load.
pub fn load_image(path: &Path) -> Result<Loaded, String> {
    load_image_folded(path, None, true)
}

/// Decode `path` with the display copy folded to `max_edge` and the decode dropped unless `keep_decode` — the phone's load. Header-only reads for the HUD; a non-TIFF source (JXL/JPEG/VSF) simply has no EXIF block here.
pub fn load_image_folded(path: &Path, max_edge: Option<usize>, keep_decode: bool) -> Result<Loaded, String> {
    let dec = crate::convert::load_any(path)?;
    let meta = crate::tiff::FrameMeta::read_path(path).ok();
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Loaded::from_decoded(dec, meta, &file_name, file_size, max_edge, keep_decode)
}

/// The viewer. See the module doc for the host contract.
pub struct View {
    /// Display-ready α+darkness pixels, img_w × img_h, gamma-2 encoded from the linear rendering.
    pixels: Vec<u32>,
    img_w: usize,
    img_h: usize,
    /// View transform in RELATIVE units (no stored pixels anywhere in the image pipe). `zoom_rel` is image scale per unit of the image area's harmonic-mean span (2wh/(w+h)); the drawn zoom is `zoom_rel * area_span`, derived fresh from the live area every frame. So the composition scales continuously and smoothly with the host — resize any edge, drag the divider, maximize: the image tracks like every other UI element, fitted or zoomed alike, no modes.
    zoom_rel: f32,
    /// The image-space point (as fractions of image dims) anchored at the centre of the image area. Pan and navigator move these; resizes preserve them — which IS composition preservation.
    cx_frac: f32,
    cy_frac: f32,
    /// Cursor position at the last drag event while panning; None = not panning.
    drag: Option<(Coord, Coord)>,
    /// One-shot fit queued for the next render — the host may construct the view before it knows its real area, so load-time fitting is lazy (render is the first call guaranteed real dims). NOT a mode: the span-relative transform preserves the fitted composition thru resizes by construction.
    needs_fit: bool,
    /// The rectangle the host gave us, set by [`View::set_area`].
    area: Area,
    /// Right tool panel width as a fraction of the area width; the divider drags it.
    panel_frac: f32,
    divider_drag: bool,
    tools: PanelTools,
    /// Linear SIGNED Rec.2020 of the current image (white = 65535, out-of-range preserved) — kept so exposure re-encodes without re-decoding the source, so the EV multiply can recover clipped-at-display speculars and sub-black noise, and as the chart's per-frame chromaticity source.
    lin: Vec<i32>,
    /// The raw sensor view — the histogram's per-frame source.
    raw: RawView,
    /// Histogram x-axis: false = linear counts, true = log2 stops. Y-axis likewise: linear count vs log2 count. Independent pills; every combination is a labelled remap, never a silent curve.
    hist_xlog: bool,
    hist_ylog: bool,
    /// Clip indicator on/off — lumis's raw inversion at the encode boundary: blown highlights render dark, crushed shadows render blown, channel-wise.
    clip_show: bool,
    /// The retained decode where the host kept it (see [`Loaded`]).
    dec: Option<crate::convert::Decoded>,
    /// The histogram's axis pills + the clip toggle, overlaid top-right of the histogram rect. Labels read the CURRENT mode.
    btn_xscale: fluor::widgets::Button,
    btn_yscale: fluor::widgets::Button,
    btn_clip: fluor::widgets::Button,
    /// Exposure in stops (gain = 2^ev in linear), [EV_MIN]..=[EV_MAX].
    ev: f32,
    /// The open file's declared opening gain (stops), already in the render — shown in the HUD so the slider's 0 is readable as "as the file says" rather than as "no gain".
    baseline_ev: f32,
    /// The panel's exposure slider (fluor widget, value 0..1 ↔ EV_MIN..EV_MAX; 0 EV sits at [EV_ZERO]).
    ev_slider: fluor::widgets::Slider,
    /// 1:1 / Fit — fluor pill Buttons, same widget family as the slider and chrome. Geometry is set every frame from panel_rects; hit silhouettes stamp into the host's hit map at render, so dispatch rides the host's Container walk.
    btn_one: fluor::widgets::Button,
    btn_fit: fluor::widgets::Button,
    /// Export pill — the visible face of the `E` key: an sRGB JPEG beside the source when the view has one, else a request the host answers ([`View::take_export_request`]; photon saves the original).
    btn_export: fluor::widgets::Button,
    /// Frame-info HUD toggle pill — the visible face of the `I` key; filled while the HUD is shown.
    btn_info: fluor::widgets::Button,
    /// True while dragging the exposure slider handle.
    ev_drag: bool,
    /// True while dragging inside the navigator — every cursor move re-centers the main view live.
    nav_drag: bool,
    /// The host's wake-sender, for background work that must land on the UI thread (the target scan).
    wake: Option<std::sync::Arc<dyn fluor::host::WakeSender<Msg>>>,
    /// Where the shown image came from on disk, when it did — export, convert and the calibrate scan need a path; an in-memory host leaves it `None`.
    source: Option<PathBuf>,
    /// Set by the export pill / `E` when there is no source path to write beside; the host drains it.
    export_requested: bool,
    /// Set when plain mode flips; the host drains it to wipe its chrome hit stamps.
    plain_changed: bool,
    /// Target-scan state (`calibrate` feature): the solved overlay in raw coordinates, the one-line readout for the HUD, and whether a scan is in flight (the pill reads "Scanning…").
    #[cfg(feature = "calibrate")]
    cal_overlay: Option<crate::calibrate::Overlay>,
    #[cfg(feature = "calibrate")]
    cal_readout: Option<String>,
    #[cfg(feature = "calibrate")]
    cal_busy: bool,
    #[cfg(feature = "calibrate")]
    btn_cal: fluor::widgets::Button,
    /// Live capture (`live` feature): the shared state with the capture thread while running, the 12 matrix sliders (3 output rows × [r g b gain]), the slider being dragged, the status line, and the Reset/Save pills. The matrix section shows only while live.
    #[cfg(feature = "live")]
    live: Option<std::sync::Arc<crate::live::Shared>>,
    #[cfg(feature = "live")]
    live_status: Option<String>,
    #[cfg(feature = "live")]
    mat_sliders: Vec<fluor::widgets::Slider>,
    #[cfg(feature = "live")]
    mat_drag: Option<usize>,
    #[cfg(feature = "live")]
    btn_live: fluor::widgets::Button,
    #[cfg(feature = "live")]
    btn_mat_reset: fluor::widgets::Button,
    #[cfg(feature = "live")]
    btn_mat_save: fluor::widgets::Button,
    /// Crop rect in DISPLAY pixels (after orientation), `[x0, x1) × [y0, y1)`; `Some` = crop mode on. Toggling on seeds the full frame and fits; a click or drag on the image moves the NEAREST corner to the cursor (no handles, no modes); toggling off clears and fits. Armed = the JPEG exports the rect and `V` records a `crop` view op. A view op: it culls, the plane never changes.
    crop: Option<(usize, usize, usize, usize)>,
    /// The corner being dragged (0 = x0y0, 1 = x1y0, 2 = x0y1, 3 = x1y1) while the button is down in crop mode.
    crop_drag: Option<usize>,
    btn_crop: fluor::widgets::Button,
    /// 90° rotates: turn the display copy, compose onto the retained decode's orientation op when there is one. Display-only, recorded by `V`; the stored plane is never touched.
    btn_rot_ccw: fluor::widgets::Button,
    btn_rot_cw: fluor::widgets::Button,
    /// HDR highlight rolloff at the encode boundary (screen AND JPEG export — they must agree): `(3x − x³)/2` after the clamp, per channel, in linear. Brightens (slope 1.5 at black) and compresses the top into a soft shoulder instead of a hard stop; pull EV down ~3× and the reclaimed range is ~1.5 stops of highlight. `H` / the HDR pill. A CREATIVE op — recorded as `dr_curve` when converting to VSF, never silent.
    hdr: bool,
    btn_hdr: fluor::widgets::Button,
    /// Controls hidden — image only, edge to edge: no panel, no HUD. `F` enters fitted, `N` enters at 1:1 centred; either key (or Escape) leaves.
    plain: bool,
    /// Frame info HUD (metadata + live stats) over the image area's bottom-left; `I` toggles. Default on — opsin is an instrument, the readings are the point.
    show_info: bool,
    title: String,
    file_name: String,
    frame_lines: Vec<String>,
    channel_names: Vec<String>,
    /// Image pixel under the cursor at the last HUD-relevant redraw — the redraw gate for cursor motion (only a CHANGE of image pixel repaints, so high zoom doesn't repaint per screen pixel).
    info_px: Option<(usize, usize)>,
}

impl View {
    /// Build the view around a loaded image, allocating its widget ids from the host's counter. The area is a placeholder until the host's first [`View::set_area`]; the load-time fit runs at the first render.
    pub fn new(loaded: Loaded, hit_counter: &mut HitId) -> Self {
        let ev_slider = fluor::widgets::Slider::new(hit_counter, 0., 0., 1., 1., EV_ZERO);
        let btn_one = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "1:1");
        let btn_fit = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Fit");
        let btn_export = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "JPEG");
        let btn_hdr = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "HDR");
        let btn_crop = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Crop");
        #[cfg(feature = "calibrate")]
        let btn_cal = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Cal");
        #[cfg(feature = "live")]
        let btn_live = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Live");
        #[cfg(feature = "live")]
        let btn_mat_reset = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Reset");
        #[cfg(feature = "live")]
        let btn_mat_save = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Save");
        #[cfg(feature = "live")]
        let mat_sliders: Vec<fluor::widgets::Slider> = (0..12).map(|i| fluor::widgets::Slider::new(hit_counter, 0., 0., 1., 1., mat_slider_pos(i, crate::live::IDENTITY[i / 4][i % 4]))).collect();
        let btn_rot_ccw = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "CCW");
        let btn_rot_cw = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "CW");
        let mut btn_info = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Info");
        btn_info.set_fill(Some(INFO_ON_FILL));
        let btn_xscale = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "X Lin");
        let btn_yscale = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Y Lin");
        let btn_clip = fluor::widgets::Button::new(hit_counter, 0., 0., 1., 1., 1., "Clip");

        let tools = PanelTools::new(&loaded.pixels, loaded.w, loaded.h);

        Self {
            pixels: loaded.pixels,
            img_w: loaded.w,
            img_h: loaded.h,
            zoom_rel: 0.,
            cx_frac: 0.5,
            cy_frac: 0.5,
            drag: None,
            needs_fit: true,
            area: Area { x: 0., y: 0., w: 1280., h: 800. },
            panel_frac: 7. / (1 << 5) as f32,
            divider_drag: false,
            tools,
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
            baseline_ev: 0.,
            ev_slider,
            btn_one,
            btn_fit,
            btn_export,
            btn_info,
            ev_drag: false,
            nav_drag: false,
            wake: None,
            source: None,
            export_requested: false,
            plain_changed: false,
            #[cfg(feature = "calibrate")]
            cal_overlay: None,
            #[cfg(feature = "calibrate")]
            cal_readout: None,
            #[cfg(feature = "calibrate")]
            cal_busy: false,
            #[cfg(feature = "calibrate")]
            btn_cal,
            #[cfg(feature = "live")]
            live: None,
            #[cfg(feature = "live")]
            live_status: None,
            #[cfg(feature = "live")]
            mat_sliders,
            #[cfg(feature = "live")]
            mat_drag: None,
            #[cfg(feature = "live")]
            btn_live,
            #[cfg(feature = "live")]
            btn_mat_reset,
            #[cfg(feature = "live")]
            btn_mat_save,
            crop: None,
            crop_drag: None,
            btn_crop,
            btn_rot_ccw,
            btn_rot_cw,
            hdr: false,
            btn_hdr,
            plain: false,
            show_info: true,
            title: loaded.title,
            file_name: loaded.file_name,
            frame_lines: loaded.frame_lines,
            channel_names: loaded.channel_names,
            info_px: None,
        }
    }

    // ── host contract ──

    /// The rectangle the view draws in and answers events for. Set on every resize (the transform is span-relative, so the composition rides the change on its own).
    pub fn set_area(&mut self, area: Area) {
        self.area = area;
    }

    pub fn area(&self) -> Area {
        self.area
    }

    /// The host's wake-sender for the view's background work.
    pub fn set_wake(&mut self, wake: std::sync::Arc<dyn fluor::host::WakeSender<Msg>>) {
        self.wake = Some(wake);
    }

    /// Where the shown image lives on disk (the desktop viewer); `None` for an in-memory host.
    pub fn set_source(&mut self, path: Option<PathBuf>) {
        self.source = path;
    }

    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Relabel the export pill (photon says "Save": it hands the original over, no JPEG is written).
    pub fn set_export_label(&mut self, label: &str) {
        self.btn_export.set_label(label);
    }

    /// The export pill / `E` fired with no source path to write beside — the host does what export means for it.
    pub fn take_export_request(&mut self) -> bool {
        std::mem::take(&mut self.export_requested)
    }

    /// Plain mode flipped since the last take — the host's chrome hit stamps are stale.
    pub fn take_plain_changed(&mut self) -> bool {
        std::mem::take(&mut self.plain_changed)
    }

    pub fn plain(&self) -> bool {
        self.plain
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn has_image(&self) -> bool {
        self.img_w > 0 && self.img_h > 0
    }

    pub fn ev(&self) -> f32 {
        self.ev
    }

    /// Total display gain in stops: the operator's slider PLUS the file's declared opening gain. The screen gets the baseline half thru the characterization matrix (`display_matrix` folds in `2^baseline`, so it is already inside `lin`) and the slider half at the encode boundary — so anything that has to agree with the screen and works from RAW COUNTS, like the histogram, has to add both. Having one definition is the point: when the baseline landed in the matrix and the histogram kept using the slider alone, the two silently diverged by exactly the baseline.
    fn total_ev(&self) -> f32 {
        self.ev + self.baseline_ev
    }

    /// Every hit id the view's widgets answer to — the host's cursor cue and press routing.
    pub fn hit_ids(&self) -> Vec<HitId> {
        #[allow(unused_mut)]
        let mut ids = vec![self.ev_slider.hit_id(), self.btn_one.hit_id(), self.btn_fit.hit_id(), self.btn_export.hit_id(), self.btn_info.hit_id(), self.btn_hdr.hit_id(), self.btn_crop.hit_id(), self.btn_rot_ccw.hit_id(), self.btn_rot_cw.hit_id(), self.btn_xscale.hit_id(), self.btn_yscale.hit_id(), self.btn_clip.hit_id()];
        #[cfg(feature = "calibrate")]
        ids.push(self.btn_cal.hit_id());
        #[cfg(feature = "live")]
        {
            ids.extend([self.btn_live.hit_id(), self.btn_mat_reset.hit_id(), self.btn_mat_save.hit_id()]);
            ids.extend(self.mat_sliders.iter().map(|s| s.hit_id()));
        }
        ids
    }

    /// Does this hit id belong to one of the view's widgets?
    pub fn owns_hit(&self, hit: HitId) -> bool {
        hit != HIT_NONE && self.hit_ids().contains(&hit)
    }

    /// The cursor cue for a point, if the view has an opinion: horizontal arrows on the divider band (and while dragging it), the hand over its pills and while panning.
    pub fn cursor_for(&self, x: Coord, y: Coord, hit: HitId) -> Option<CursorIcon> {
        if self.divider_drag || (!self.plain && self.inside(x, y) && (x - self.divider_x()).abs() <= self.divider_grab()) {
            return Some(CursorIcon::EwResize);
        }
        if self.owns_hit(hit) || self.drag.is_some() {
            return Some(CursorIcon::Pointer);
        }
        None
    }

    fn inside(&self, x: Coord, y: Coord) -> bool {
        x >= self.area.x && y >= self.area.y && x < self.area.x + self.area.w && y < self.area.y + self.area.h
    }

    // ── geometry ──

    /// Divider grab band half-width — RU-scaled like every other affordance (no pixel constants).
    fn divider_grab(&self) -> f32 {
        (self.area.w.min(self.area.h) / (1 << 5) as f32).ceil() / 4.
    }

    /// The image area's width — the whole area with the controls hidden, else what the divider leaves.
    fn image_w(&self) -> f32 {
        if self.plain { self.area.w } else { self.area.w * (1. - self.panel_frac) }
    }

    /// Screen x of the panel divider (left edge of the panel).
    fn divider_x(&self) -> f32 {
        self.area.x + self.image_w()
    }

    /// Image area geometry (x, y, width, height) in screen px, live from the area.
    fn image_area(&self) -> (f32, f32, f32, f32) {
        (self.area.x, self.area.y, self.image_w(), self.area.h)
    }

    /// Harmonic-mean span of the image area, 2wh/(w+h) — the universal scaling base (smooth in both dims, biased toward the smaller one).
    fn area_span(aw: f32, ah: f32) -> f32 {
        2. * aw * ah / (aw + ah).max(1.)
    }

    /// The view transform in screen pixels — (zoom, ox, oy) — derived fresh from the live area at every use. Nothing pixel-valued is ever stored: zoom = zoom_rel × area span, and (ox, oy) place the anchored image fraction at the area centre. This derivation is what makes every host op (edge resize, divider drag, maximize) scale the composition continuously.
    fn view_px(&self) -> (f32, f32, f32) {
        let (ax, ay, aw, ah) = self.image_area();
        let zoom = self.zoom_rel * Self::area_span(aw, ah);
        let ox = ax + aw * 0.5 - self.cx_frac * self.img_w as f32 * zoom;
        let oy = ay + ah * 0.5 - self.cy_frac * self.img_h as f32 * zoom;
        (zoom, ox, oy)
    }

    /// Fit the image inside the image area with a small margin and center it — a one-shot that SETS the relative composition. Because the transform is span-relative, the fitted composition then rides every resize on its own. No-op in the empty state.
    pub fn fit(&mut self) {
        if self.img_w == 0 || self.img_h == 0 {
            return;
        }
        // With a crop armed, the crop IS the frame: fit and centre on it.
        let (rx0, ry0, rx1, ry1) = self.crop.unwrap_or((0, 0, self.img_w, self.img_h));
        let (rw, rh) = ((rx1 - rx0).max(1) as f32, (ry1 - ry0).max(1) as f32);
        let (_, _, aw, ah) = self.image_area();
        let zoom = (aw / rw).min(ah / rh) * (1. - 1. / (1 << 6) as f32);
        self.zoom_rel = zoom / Self::area_span(aw, ah);
        self.cx_frac = (rx0 as f32 + rw / 2.) / self.img_w as f32;
        self.cy_frac = (ry0 as f32 + rh / 2.) / self.img_h as f32;
    }

    /// 1:1 — one image pixel per screen pixel, EXACTLY: stores `zoom_rel = 1/span` bitwise so the readout's `==` test can certify pixel-exactness (and lose it the moment a resize changes the span). Zooms about the image-area centre for free — the anchored fractions ARE the centre point.
    pub fn one_to_one(&mut self) {
        let (_, _, aw, ah) = self.image_area();
        self.zoom_rel = 1. / Self::area_span(aw, ah);
    }

    /// Rescale around a screen-space anchor so the image point under the cursor stays put — computed in derived pixel space, stored back as relative state. Zoom is unbounded — the blit cost is capped by screen area at any zoom, and the wheel factor is strictly positive so zoom can't reach 0. The host's pinch and Ctrl+wheel land here.
    pub fn zoom_around(&mut self, factor: f32, ax: f32, ay: f32) {
        if self.img_w == 0 || self.img_h == 0 || factor <= 0. {
            return;
        }
        let (zoom, ox, oy) = self.view_px();
        let (px, py, aw, ah) = self.image_area();
        let zoom2 = zoom * factor;
        let ox2 = ax - (ax - ox) * factor;
        let oy2 = ay - (ay - oy) * factor;
        self.zoom_rel = zoom2 / Self::area_span(aw, ah);
        self.cx_frac = (px + aw * 0.5 - ox2) / (self.img_w as f32 * zoom2);
        self.cy_frac = (py + ah * 0.5 - oy2) / (self.img_h as f32 * zoom2);
    }

    /// Pan by a screen-pixel delta (a touch drag or trackpad scroll from the host).
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        if self.img_w == 0 || self.img_h == 0 {
            return;
        }
        let (zoom, _, _) = self.view_px();
        if zoom <= 0. {
            return;
        }
        self.cx_frac -= dx / (self.img_w as f32 * zoom);
        self.cy_frac -= dy / (self.img_h as f32 * zoom);
    }

    /// Continuous image coordinate of a screen point (unclamped — the caller decides what off-image means).
    fn image_pt(&self, x: f32, y: f32) -> (f32, f32) {
        let (zoom, ox, oy) = self.view_px();
        ((x - ox) / zoom, (y - oy) / zoom)
    }

    /// Is this screen point on the drawn image itself (as opposed to backdrop)?
    fn on_image(&self, x: f32, y: f32) -> bool {
        let (zoom, ox, oy) = self.view_px();
        x >= ox && y >= oy && x < ox + self.img_w as f32 * zoom && y < oy + self.img_h as f32 * zoom && x < self.divider_x()
    }

    /// The image pixel under screen point (x, y), if it's on the drawn image.
    fn image_px_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        if !self.on_image(x, y) {
            return None;
        }
        let (zoom, ox, oy) = self.view_px();
        Some((((x - ox) / zoom) as usize, ((y - oy) / zoom) as usize))
    }

    /// Panel section rects (x0, y0, w, h) in pixels — named, not position-coded. Stacked top-down inside the panel with uniform padding; each section keeps its natural aspect (navigator = image aspect, histogram = 2:1, buttons/slider = thin bands, chart = square) and the stack just runs off the bottom on short areas.
    fn panel_rects(&self, viewport: Viewport) -> PanelRects {
        let vw = (self.area.x + self.area.w) as usize;
        let vh = (self.area.y + self.area.h) as usize;
        let dx = self.divider_x() as usize;
        let pad = pad_of(viewport);
        let band = band_of(viewport);
        let x0 = (dx + 1 + pad).min(vw);
        let w = vw.saturating_sub(x0 + pad);
        let mut y = (self.area.y as usize + pad).min(vh);
        let ah = self.area.h as usize;

        let nav_h = if self.img_w > 0 { (w * self.img_h / self.img_w).min(ah / 3) } else { 0 };
        let nav = (x0, y, w, nav_h);
        y += nav_h + pad;

        // Two rows: [1:1][mag][Fit][export][Info] over [CCW][CW][Crop], a half-pad between. The empty viewer keeps one row for the controls that need no image (Live).
        #[cfg(feature = "live")]
        let empty_h = band;
        #[cfg(not(feature = "live"))]
        let empty_h = 0;
        let btn_h = if self.img_w > 0 { band * 2 + pad / 2 } else { empty_h };
        let btns = (x0, y.min(vh), w, btn_h);
        y += btn_h + pad;

        let hist_h = (w / 2).min(ah / 4);
        let hist = (x0, y.min(vh), w, hist_h);
        y += hist_h + pad;

        let ev = (x0, y.min(vh), w, band);
        y += band + pad;

        #[cfg(feature = "live")]
        let mat_h = if self.live.is_some() { band * 5 + pad * 2 } else { 0 };
        #[cfg(not(feature = "live"))]
        let mat_h = 0;
        let mat = (x0, y.min(vh), w, mat_h);
        if mat_h > 0 {
            y += mat_h + pad;
        }

        let chart_h = w.min(vh.saturating_sub(y + pad));
        let chart = (x0, y.min(vh), w, chart_h);
        PanelRects { nav, btns, hist, ev, mat, chart }
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

    /// Center the main view on the navigator-space point under the cursor (clamped to the thumb, so dragging past the edge pins to the edge). The navigator IS the fraction space — the cursor's thumb fractions become the anchored composition directly.
    fn nav_center(&mut self, viewport: Viewport, cx: f32, cy: f32) {
        let Some((fx, fy, fw, fh)) = self.nav_fit(viewport) else {
            return;
        };
        if self.zoom_rel <= 0. {
            return;
        }
        self.cx_frac = ((cx - fx as f32) / fw as f32).clamp(0., 1.);
        self.cy_frac = ((cy - fy as f32) / fh as f32).clamp(0., 1.);
    }

    // ── state changes ──

    /// Apply the slider's 0..1 value as stops and re-encode the display pixels. The panel thumbnail tracks (cheap), the histogram tracks too (the same gain remaps its bins — see the render arm), and the chart alone stays put (chromaticity ratios shrug at a scalar).
    fn apply_ev(&mut self, value01: f32, ctx: &mut Context) {
        let ev = ev_of_slider(value01, self.baseline_ev);
        if (ev - self.ev).abs() < 1e-4 || self.lin.is_empty() {
            return;
        }
        self.ev = ev;
        #[cfg(feature = "live")]
        if let Some(shared) = &self.live {
            shared.ev_gain.store(2f32.powf(ev).to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
        self.reencode();
        ctx.window.request_redraw();
    }

    /// Nudge the exposure by `delta` stops (the host's keys or gestures), or reset to 0 with `delta = None`.
    pub fn nudge_ev(&mut self, delta: Option<f32>, ctx: &mut Context) {
        let v = match delta {
            Some(d) => slider_of_ev(self.ev + d, self.baseline_ev),
            None => ev_zero_of(self.baseline_ev),
        };
        self.ev_slider.set_value(v);
        self.apply_ev(v, ctx);
    }

    /// Display pixels + the navigator thumb from the linear buffer under the current exposure, clip and HDR settings.
    fn reencode(&mut self) {
        if self.lin.is_empty() {
            return;
        }
        self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
        self.tools.refresh_thumb(&self.pixels, self.img_w, self.img_h);
    }

    /// Swap a decoded image into the view: title, pixels, dims, panel tools, redraw. EVERY setting the operator holds survives a load — exposure, clip, histogram axes, the HUD, the panel split — whether the frame arrives by arrow, drop, or socket handoff. Same dimensions ⇒ pan and zoom stay put too, so stepping through a burst or LED sequence compares like with like; different dimensions ⇒ refit.
    pub fn install(&mut self, loaded: Loaded, ctx: &mut Context) {
        let same_geometry = loaded.w == self.img_w && loaded.h == self.img_h && self.img_w > 0;
        self.title = loaded.title;
        self.img_w = loaded.w;
        self.img_h = loaded.h;
        self.tools = PanelTools::new(&loaded.pixels, loaded.w, loaded.h);
        self.lin = loaded.lin;
        self.raw = loaded.raw;
        self.dec = loaded.dec;
        self.frame_lines = loaded.frame_lines;
        self.channel_names = loaded.channel_names;
        #[cfg(feature = "calibrate")]
        if loaded.file_name != self.file_name {
            // The overlay belongs to the frame it was solved on.
            self.cal_overlay = None;
            self.cal_readout = None;
        }
        self.file_name = loaded.file_name;
        self.baseline_ev = loaded.baseline_ev;
        // A file that RECORDS an exposure wins: that op is this operator's own grade of this frame, so opening it should show it graded. A file that records none leaves the slider alone, which is what keeps arrowing through a folder of ungraded frames at one exposure.
        if let Some(ev) = loaded.stored_ev {
            self.ev = ev;
        }
        // The baseline moves where a given EV sits on the track, so the handle is re-placed whether or not the EV itself changed — and the EV is held to what this file can reach.
        self.ev = self.ev.clamp(EV_MIN - self.baseline_ev, EV_MAX - self.baseline_ev);
        self.ev_slider.set_value(slider_of_ev(self.ev, self.baseline_ev));
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
            self.fit();
        }
        ctx.window.request_redraw();
    }

    /// Convert the current image to a VSF-Image beside the source (`<stem>.vsf`). Returns the written path for the caller to surface.
    fn convert_current_to_vsf(&self) -> Result<PathBuf, String> {
        let Some(src) = self.source.as_deref() else {
            return Err("no source file to convert".to_string());
        };
        if crate::sniff::sniff_path(src) == Some(crate::sniff::Kind::Vsf) {
            return Err("already a VSF image".to_string());
        }
        let out = src.with_extension("vsf");
        let mut dec = crate::convert::load_any(src)?;
        // Record the live view ops — APPENDED to the translateration log, so the ingest-recorded orientation op rides ahead. `exposure` (Technical: a scalar shifts no hue) when EV ≠ 0; `dr_curve` (CREATIVE: a curve is a deliberate look) when HDR is on. Neither is ever baked into the plane; an op-less log is never created.
        // The display orientation as the view holds it NOW (ingest's EXIF code composed with any 90° turns): replace the ingest-recorded op's param, or insert one at the head so it rides ahead of everything else.
        let code = self.raw.orient;
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

    /// Export the current view as an sRGB JPEG beside the source (`<stem>.jpg`) — the live exposure baked in, everything else exactly the rendering on screen. `self.lin` already carries the orientation, so the JPEG lands the way the view shows it. Returns the written path for the caller to surface; `Ok(None)` = no source to write beside, the host was asked instead.
    fn export_current_jpeg(&mut self) -> Result<Option<PathBuf>, String> {
        if self.lin.is_empty() {
            return Err("no image loaded".to_string());
        }
        // Live frames have no source file: they land in ~/Pictures/opsin as live-<unix seconds>.jpg.
        #[cfg(feature = "live")]
        let live_out = self.live.as_ref().map(|_| {
            let dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Pictures").join("opsin");
            let _ = std::fs::create_dir_all(&dir);
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            dir.join(format!("live-{secs}.jpg"))
        });
        #[cfg(not(feature = "live"))]
        let live_out: Option<PathBuf> = None;
        let out = match live_out.or_else(|| self.source.as_ref().map(|p| p.with_extension("jpg"))) {
            Some(p) => p,
            None => {
                self.export_requested = true;
                return Ok(None);
            }
        };
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
        Ok(Some(out))
    }

    fn export(&mut self) {
        match self.export_current_jpeg() {
            Ok(Some(out)) => println!("opsin: wrote {}", out.display()),
            Ok(None) => {}
            Err(e) => eprintln!("opsin: JPEG export failed: {e}"),
        }
    }

    /// Controls on/off. `F` refits on the way in and out; `N` enters at 1:1 centred and leaves keeping the composition.
    fn set_plain(&mut self, plain: bool, ctx: &mut Context) {
        self.plain = plain;
        self.plain_changed = true;
        ctx.window.request_redraw();
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

    /// Crop mode on/off — `X` and the Crop pill share this. On: the rect seeds as the full frame (nothing dimmed yet; pull corners in), off: cleared. Both refit, so the composition always frames what's armed.
    fn toggle_crop(&mut self, ctx: &mut Context) {
        if self.img_w == 0 {
            return;
        }
        self.crop = if self.crop.is_none() { Some((0, 0, self.img_w, self.img_h)) } else { None };
        self.crop_drag = None;
        self.btn_crop.set_fill(self.crop.is_some().then_some(INFO_ON_FILL));
        self.fit();
        ctx.window.request_redraw();
    }

    /// Rotate the DISPLAY 90° (cw or ccw): turn the linear copy, compose onto the retained decode's orientation op when one is held (so `V` records it), carry the crop rect through the same rotation, refit. The plane itself never moves.
    pub fn rotate(&mut self, cw: bool, ctx: &mut Context) {
        if self.img_w == 0 || self.img_h == 0 {
            return;
        }
        let code = crate::convert::rotate_code(self.raw.orient, cw);
        if let Some(dec) = self.dec.as_mut() {
            let op = vsf::spectral_image::ViewOp { name: "orientation".to_string(), class: vsf::spectral_image::IdtClass::Technical, params: vec![code as f32] };
            match &mut dec.img.view {
                Some(v) => match v.ops.iter_mut().find(|o| o.name == "orientation") {
                    Some(o) => o.params = vec![code as f32],
                    None => v.ops.insert(0, op),
                },
                None => dec.img.view = Some(vsf::spectral_image::ViewTransform { space: "vsf_rgb_linear".to_string(), ops: vec![op] }),
            }
        }
        let (ow, oh) = (self.img_w, self.img_h);
        let lin = rotate_linear(&self.lin, ow, oh, cw);
        // The crop rides the rotation: CW maps (x, y) → (H − y, x) on the old W×H frame; CCW maps (x, y) → (y, W − x).
        self.crop = self.crop.map(|r| rotate_rect(r, ow, oh, cw));
        self.img_w = oh;
        self.img_h = ow;
        self.lin = lin;
        self.raw.orient = code;
        std::mem::swap(&mut self.raw.or_w, &mut self.raw.or_h);
        std::mem::swap(&mut self.raw.fold_w, &mut self.raw.fold_h);
        self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
        self.tools = PanelTools::new(&self.pixels, self.img_w, self.img_h);
        self.fit();
        ctx.window.request_redraw();
    }

    /// HDR rolloff on/off — `H` and the HDR pill share this. A re-encode, same cost as an EV tick.
    fn toggle_hdr(&mut self, ctx: &mut Context) {
        self.hdr = !self.hdr;
        #[cfg(feature = "live")]
        if let Some(shared) = &self.live {
            shared.hdr.store(self.hdr, std::sync::atomic::Ordering::Relaxed);
        }
        self.btn_hdr.set_fill(self.hdr.then_some(INFO_ON_FILL));
        self.reencode();
        ctx.window.request_redraw();
    }

    /// Clip indicator on/off — lumis's preview_sub inversion at the encode boundary — after the magic-9 and the EV gain, so it marks display clipping under the CURRENT exposure and tracks the slider live. A cheap re-encode; `lin` is untouched (the JPEG export stays clean of it by construction).
    fn toggle_clip(&mut self, ctx: &mut Context) {
        self.clip_show = !self.clip_show;
        #[cfg(feature = "live")]
        if let Some(shared) = &self.live {
            shared.clip.store(self.clip_show, std::sync::atomic::Ordering::Relaxed);
        }
        self.btn_clip.set_fill(self.clip_show.then_some(CLIP_ON_FILL));
        self.reencode();
        ctx.window.request_redraw();
    }

    /// Frame-info HUD on/off — the `I` key and the Info pill share this; the pill's fill tracks the state.
    fn toggle_info(&mut self, ctx: &mut Context) {
        self.show_info = !self.show_info;
        self.btn_info.set_fill(self.show_info.then_some(INFO_ON_FILL));
        ctx.window.request_redraw();
    }

    /// Kick off a target scan of the current frame on a background thread (`calibrate` feature). The scan reads the file from disk itself — it mutates its input, and the retained decode stays pristine — and reports back thru the wake-sender as `Msg::Scan`.
    #[cfg(feature = "calibrate")]
    fn start_scan(&mut self, ctx: &mut Context) {
        #[cfg(feature = "live")]
        if let Some(shared) = &self.live {
            // Live: the capture thread scans its next frame — hold the target up.
            shared.scan_requested.store(true, std::sync::atomic::Ordering::Relaxed);
            self.cal_readout = Some("cal: scanning next frame…".to_string());
            ctx.window.request_redraw();
            return;
        }
        let Some(path) = self.source.clone() else { return };
        let Some(wake) = self.wake.clone() else { return };
        if self.cal_busy {
            return;
        }
        if !matches!(crate::sniff::sniff_path(&path), Some(crate::sniff::Kind::Tiff)) {
            eprintln!("opsin: calibrate: {} is not a DNG — a scan needs the raw mosaic and a ColorMatrix1 slot to paste into", path.display());
            return;
        }
        self.cal_busy = true;
        self.cal_readout = Some("cal: scanning…".to_string());
        self.btn_cal.set_label("Scanning…");
        ctx.window.request_redraw();
        std::thread::Builder::new()
            .name("opsin-scan".into())
            .spawn(move || {
                let result = crate::calibrate::scan(&path);
                let _ = wake.send(Msg::Scan(path, result));
            })
            .ok();
    }

    /// The scan came back: paste the nine numbers into the DNG in place (same machinery as Ctrl+V — the fingerprint is the file's own, so it matches by construction), reload so the frame renders through them, grade the entry `unit`/`relative` (this IS a measurement of this sensor), and keep the overlay + readout. A rejected scan leaves the file untouched and says why.
    #[cfg(feature = "calibrate")]
    fn finish_scan(&mut self, path: PathBuf, result: Result<crate::calibrate::ScanOutcome, String>, ctx: &mut Context) {
        self.cal_busy = false;
        self.btn_cal.set_label("Cal");
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                eprintln!("opsin: calibrate: {e}");
                self.cal_readout = Some(format!("cal: {e}"));
                ctx.window.request_redraw();
                return;
            }
        };
        print!("{}", outcome.report);
        let pasted = crate::idt::IdtClip::copy_from(&path).and_then(|mut clip| {
            clip.matrix = outcome.matrix;
            clip.illuminant = 23; // D50 — what chameleon writes.
            clip.source = format!("chameleon scan of {}", path.display());
            clip.paste_into(&path, false)
        });
        match pasted {
            Ok(report) => println!("opsin: {report}"),
            Err(e) => {
                eprintln!("opsin: calibrate: paste failed: {e}");
                self.cal_readout = Some(format!("cal: solved, but paste failed: {e}"));
                ctx.window.request_redraw();
                return;
            }
        }
        // Only reload if this is still the frame on screen (a scan can outlive a navigation).
        if self.source.as_deref() == Some(path.as_path()) {
            match load_image(&path) {
                Ok(loaded) => self.install(loaded, ctx),
                Err(e) => eprintln!("opsin: {}: {e}", path.display()),
            }
            if let Some(dec) = self.dec.as_mut() {
                if let Some(e) = dec.img.profile.as_mut().and_then(|p| p.entries.first_mut()) {
                    e.grade = vsf::spectral_image::ProfileGrade::Unit;
                    e.class = vsf::spectral_image::IdtClass::Relative;
                    e.source = "chameleon_scan".to_string();
                }
            }
            self.cal_overlay = outcome.overlay;
            self.cal_readout = Some(outcome.readout);
        }
        ctx.window.request_redraw();
    }

    /// Live on/off — `L` and the Live pill. On: the saved matrix (or identity) into the sliders, the capture thread started, the viewer becomes the viewfinder. Off: the thread stopped (the loopback stream ends with it) and the last frame stays on screen.
    #[cfg(feature = "live")]
    fn toggle_live(&mut self, ctx: &mut Context) {
        if let Some(shared) = self.live.take() {
            shared.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            self.btn_live.set_fill(None);
            self.live_status = Some("live: stopped".into());
            ctx.window.request_redraw();
            return;
        }
        let Some(wake) = self.wake.clone() else { return };
        let m = crate::live::load_matrix().unwrap_or(crate::live::default_matrix());
        self.set_matrix(m, ctx);
        let shared = crate::live::Shared::new(m);
        shared.clip.store(self.clip_show, std::sync::atomic::Ordering::Relaxed);
        // Slider only, deliberately: live frames come from the camera, not from the open file, so the file's baseline has nothing to say about them.
        shared.ev_gain.store(2f32.powf(self.ev).to_bits(), std::sync::atomic::Ordering::Relaxed);
        shared.hdr.store(self.hdr, std::sync::atomic::Ordering::Relaxed);
        match crate::live::start(shared.clone(), move |m| {
            let _ = wake.send(Msg::Live(m));
        }) {
            Ok(_) => {
                self.live = Some(shared);
                self.btn_live.set_fill(Some(INFO_ON_FILL));
                self.live_status = Some("live: starting…".into());
            }
            Err(e) => {
                eprintln!("opsin: live: {e}");
                self.live_status = Some(format!("live: {e}"));
            }
        }
        ctx.window.request_redraw();
    }

    /// A message from the capture thread: a viewfinder frame becomes the current image (same install as a file, minus the decode); a scan result updates the sliders and the readout.
    #[cfg(feature = "live")]
    fn on_live(&mut self, m: crate::live::LiveMsg, ctx: &mut Context) {
        match m {
            crate::live::LiveMsg::Status(s) => self.live_status = Some(s),
            crate::live::LiveMsg::Level => {}
            crate::live::LiveMsg::Frame(f) => {
                if let Some(shared) = &self.live {
                    shared.frame_pending.store(false, std::sync::atomic::Ordering::Relaxed);
                }
                let first = self.img_w != f.w || self.img_h != f.h;
                self.img_w = f.w;
                self.img_h = f.h;
                self.lin = f.lin;
                self.pixels = encode_pixels(&self.lin, self.ev, self.clip_show, self.hdr);
                // The camera's own 8-bit codes for the histogram: planar, white 255.
                let n = f.w * f.h;
                let mut planar = vec![0u16; n * 3];
                for i in 0..n {
                    planar[i] = f.codes[i * 3];
                    planar[n + i] = f.codes[i * 3 + 1];
                    planar[2 * n + i] = f.codes[i * 3 + 2];
                }
                self.raw = RawView { counts: planar, sensor_w: f.w, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: n, black: [0.; 3], white: [255.; 3], bits: 8, orient: 1, pre_w: f.w, pre_h: f.h, or_w: f.w, or_h: f.h, fold_w: f.w, fold_h: f.h, census: [1.; 3] };
                if first {
                    self.tools = PanelTools::new(&self.pixels, f.w, f.h);
                    self.dec = None;
                    self.frame_lines = vec![format!("live  {}×{}  8-bit planar", f.w, f.h)];
                    self.channel_names = vec!["R".into(), "G".into(), "B".into()];
                    self.file_name = "live".to_string();
                    self.title = format!("opsin — live ({}×{})", f.w, f.h);
                    self.crop = None;
                    self.fit();
                } else {
                    self.tools.refresh_thumb(&self.pixels, f.w, f.h);
                }
            }
            #[cfg(feature = "calibrate")]
            crate::live::LiveMsg::Scan(result) => match result {
                Ok(r) => {
                    print!("{}", r.report);
                    self.cal_readout = Some(r.readout);
                    if let Some(shared) = &self.live {
                        let m = *shared.matrix.lock().unwrap();
                        for i in 0..12 {
                            self.mat_sliders[i].set_value(mat_slider_pos(i, m[i / 4][i % 4]));
                        }
                    }
                }
                Err(e) => {
                    eprintln!("opsin: live scan: {e}");
                    self.cal_readout = Some(format!("cal: {e}"));
                }
            },
        }
        ctx.window.request_redraw();
    }

    /// The matrix as the sliders hold it.
    #[cfg(feature = "live")]
    fn matrix(&self) -> crate::live::Sliders {
        let mut m = crate::live::IDENTITY;
        for i in 0..12 {
            m[i / 4][i % 4] = mat_slider_val(i, self.mat_sliders[i].value());
        }
        m
    }

    #[cfg(feature = "live")]
    fn set_matrix(&mut self, m: crate::live::Sliders, ctx: &mut Context) {
        for i in 0..12 {
            self.mat_sliders[i].set_value(mat_slider_pos(i, m[i / 4][i % 4]));
        }
        self.push_matrix();
        ctx.window.request_redraw();
    }

    /// Sliders → the capture thread.
    #[cfg(feature = "live")]
    fn push_matrix(&self) {
        if let Some(shared) = &self.live {
            *shared.matrix.lock().unwrap() = self.matrix();
        }
    }

    /// A background message for the view (scan result, live frame). Returns true when it was the view's.
    pub fn on_msg(&mut self, msg: Msg, ctx: &mut Context) -> bool {
        let _ = &ctx;
        match msg {
            Msg::Open(_) => false,
            #[cfg(feature = "calibrate")]
            Msg::Scan(path, result) => {
                self.finish_scan(path, result, ctx);
                true
            }
            #[cfg(feature = "live")]
            Msg::Live(m) => {
                self.on_live(m, ctx);
                true
            }
        }
    }

    /// The host's focus edge: the live viewfinder costs as much as the stream, so it runs only while the host is the window being looked at.
    pub fn set_focused(&mut self, focused: bool) {
        #[cfg(feature = "live")]
        if let Some(shared) = &self.live {
            shared.viewfinder.store(focused, std::sync::atomic::Ordering::Relaxed);
            if focused {
                shared.frame_pending.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        #[cfg(not(feature = "live"))]
        let _ = focused;
    }

    // ── events ──

    /// A pointer, wheel or key event the host forwards, with `hit` = the id the host's hit map holds under the cursor. `Handled` = consumed; `Close` = Escape asked to leave the viewer; `Pass` = not the view's (dead panel space, backdrop, an unbound key) — the host decides (a window drag, its own binding).
    pub fn on_event(&mut self, event: &FEvent, ctx: &mut Context, hit: HitId) -> EventResponse {
        match event {
            FEvent::CursorMoved { .. } => {
                let (cx, cy) = (ctx.cursor_x, ctx.cursor_y);
                #[cfg(feature = "live")]
                if let Some(i) = self.mat_drag {
                    self.mat_sliders[i].set_value_from_x(cx);
                    self.push_matrix();
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
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
                    // Clamp justified: external user input (a drag can leave the area entirely); the stops keep both the image area and the panel usable. 1/8 .. 1/2 of the area width.
                    self.panel_frac = (1. - (cx - self.area.x) / self.area.w.max(1.)).clamp(1. / (1 << 3) as f32, 1. / (1 << 1) as f32);
                    self.plain_changed = true; // the pills moved: the host's stale stamps must go
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                if let Some(k) = self.crop_drag {
                    let (px, py) = self.image_pt(cx, cy);
                    self.move_crop_corner(k, px, py);
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                if let Some((lx, ly)) = self.drag {
                    // Pan: shift the anchored fraction by the cursor delta in image-fraction space. Guarded by construction — drag only starts on_image, so img dims and zoom are nonzero.
                    self.pan_by(cx - lx, cy - ly);
                    self.drag = Some((cx, cy));
                    ctx.window.request_redraw();
                    return EventResponse::Handled;
                }
                let mut dirty = false;
                // HUD cursor readout: repaint only when the IMAGE pixel under the cursor changes (or the cursor leaves the image), never per screen pixel.
                if self.show_info && self.img_w > 0 {
                    let px = self.image_px_at(cx, cy);
                    if px != self.info_px {
                        self.info_px = px;
                        dirty = true;
                    }
                }
                // Pill hover — driven by the host's stamped hit map.
                for b in self.buttons_mut() {
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
            FEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left } => {
                let (cx, cy) = (ctx.cursor_x, ctx.cursor_y);
                if self.owns_hit(hit) {
                    // A pill: dispatch thru the Container walk (they only bump a counter), then poll and act.
                    let mods = ctx.modifiers;
                    let mut response = EventResponse::Handled;
                    self.visit(&mut |w| {
                        if w.id() == hit {
                            if let Some(c) = w.click() {
                                response = c.on_click(cx, cy, mods);
                            }
                        }
                    });
                    let _ = response;
                    self.poll_pills(ctx);
                    // The slider band is a pill too: a press jumps the handle and starts the drag.
                    if hit == self.ev_slider.hit_id() && !self.lin.is_empty() {
                        self.ev_drag = true;
                        self.ev_slider.set_value_from_x(cx);
                        let v = self.ev_slider.value();
                        self.apply_ev(v, ctx);
                    }
                    #[cfg(feature = "live")]
                    if let Some(i) = self.mat_sliders.iter().position(|s| s.hit_id() == hit) {
                        self.mat_drag = Some(i);
                        self.mat_sliders[i].set_value_from_x(cx);
                        self.push_matrix();
                        ctx.window.request_redraw();
                    }
                    return EventResponse::Handled;
                }
                if !self.inside(cx, cy) {
                    return EventResponse::Pass;
                }
                if !self.plain && (cx - self.divider_x()).abs() <= self.divider_grab() {
                    self.divider_drag = true;
                    return EventResponse::Handled;
                }
                if !self.plain && cx > self.divider_x() {
                    let rects = self.panel_rects(ctx.viewport);
                    let hit_rect = |r: (usize, usize, usize, usize)| r.2 > 0 && cx >= r.0 as f32 && cx < (r.0 + r.2) as f32 && cy >= r.1 as f32 && cy < (r.1 + r.3) as f32;
                    // Exposure slider — press anywhere on the band jumps the handle there and starts the drag.
                    if hit_rect(rects.ev) && !self.lin.is_empty() {
                        self.ev_drag = true;
                        self.ev_slider.set_value_from_x(cx);
                        let v = self.ev_slider.value();
                        self.apply_ev(v, ctx);
                        return EventResponse::Handled;
                    }
                    // Navigator press → live drag: center immediately and keep re-centering on every cursor move until release. Panel dead space (gaps, section padding) is the host's (a window drag on the desktop).
                    if self.nav_hit(ctx.viewport, cx, cy).is_some() {
                        self.nav_drag = true;
                        self.nav_center(ctx.viewport, cx, cy);
                        ctx.window.request_redraw();
                        return EventResponse::Handled;
                    }
                    return EventResponse::Pass;
                }
                if self.on_image(cx, cy) {
                    if self.crop.is_some() {
                        // Crop mode: the nearest corner comes to the cursor and follows it until release. Pan is the navigator's job while cropping.
                        let (px, py) = self.image_pt(cx, cy);
                        let (x0, y0, x1, y1) = self.crop.unwrap();
                        let corners = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
                        let d2 = |(kx, ky): (usize, usize)| (kx as f32 - px).powi(2) + (ky as f32 - py).powi(2);
                        let k = (0..4).min_by(|&a, &b| d2(corners[a]).total_cmp(&d2(corners[b]))).unwrap();
                        self.crop_drag = Some(k);
                        self.move_crop_corner(k, px, py);
                        ctx.window.request_redraw();
                        return EventResponse::Handled;
                    }
                    self.drag = Some((cx, cy));
                    return EventResponse::Handled;
                }
                // Backdrop (letterbox margin) — the host's.
                EventResponse::Pass
            }
            FEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left } => {
                let was = self.drag.is_some() || self.crop_drag.is_some() || self.divider_drag || self.ev_drag || self.nav_drag;
                self.drag = None;
                self.crop_drag = None;
                #[cfg(feature = "live")]
                {
                    self.mat_drag = None;
                }
                self.divider_drag = false;
                self.ev_drag = false;
                self.nav_drag = false;
                if was { EventResponse::Handled } else { EventResponse::Pass }
            }
            FEvent::MouseWheel { delta } => {
                if !self.inside(ctx.cursor_x, ctx.cursor_y) && !cfg!(target_os = "android") {
                    return EventResponse::Pass;
                }
                match delta {
                    // Pixel deltas are a finger or a trackpad moving the picture: PAN.
                    MouseScrollDelta::Pixels(x, y) => self.pan_by(*x as f32, *y as f32),
                    // Line deltas are a wheel: ZOOM about the pointer — the ecosystem zoom curve, inherited from fluor (asymmetric BY DESIGN: in ×32/31, out ×32/33; incommensurate, so notch combos are dense and any zoom is reachable).
                    MouseScrollDelta::Lines(_, y) => self.zoom_around(fluor::geom::zoom_step_factor(*y), ctx.cursor_x, ctx.cursor_y),
                }
                ctx.window.request_redraw();
                EventResponse::Handled
            }
            FEvent::Focused(focused) => {
                self.set_focused(*focused);
                EventResponse::Pass
            }
            FEvent::KeyboardInput { event } => {
                if event.state != ElementState::Pressed {
                    return EventResponse::Pass;
                }
                let ctrl = ctx.modifiers.control_key() || ctx.modifiers.super_key();
                match &event.logical_key {
                    Key::Named(NamedKey::Escape) if self.plain => {
                        // Controls back first; a second Escape closes.
                        self.set_plain(false, ctx);
                        self.fit();
                        EventResponse::Handled
                    }
                    Key::Named(NamedKey::Escape) => EventResponse::Close,
                    Key::Character(c) if ctrl => {
                        let _ = c;
                        EventResponse::Pass
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("v") => {
                        match self.convert_current_to_vsf() {
                            Ok(out) => println!("opsin: wrote {}", out.display()),
                            Err(e) => eprintln!("opsin: convert to VSF failed: {e}"),
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("e") => {
                        self.export();
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
                    Key::Character(c) if c.eq_ignore_ascii_case("x") => {
                        self.toggle_crop(ctx);
                        EventResponse::Handled
                    }
                    #[cfg(feature = "calibrate")]
                    Key::Character(c) if c.eq_ignore_ascii_case("c") => {
                        self.start_scan(ctx);
                        EventResponse::Handled
                    }
                    #[cfg(feature = "live")]
                    Key::Character(c) if c.eq_ignore_ascii_case("l") => {
                        self.toggle_live(ctx);
                        EventResponse::Handled
                    }
                    // Space: record the live stream (HEVC MOV + aligned mic + TOD timecode) — start / stop.
                    #[cfg(feature = "live")]
                    Key::Named(NamedKey::Space) if self.live.is_some() => {
                        if let Some(shared) = &self.live {
                            let mut rec = shared.record.lock().unwrap();
                            if rec.is_some() {
                                *rec = None;
                            } else {
                                let dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Videos").join("opsin");
                                *rec = Some(dir.join(format!("live-{}.mov", crate::live::local_stamp())));
                            }
                        }
                        ctx.window.request_redraw();
                        EventResponse::Handled
                    }
                    // , and . nudge the operator's A/V constant by 10 ms while live (the camera's own delay + the downstream we can't see); Save persists it with the matrix.
                    #[cfg(feature = "live")]
                    Key::Character(c) if (c == "," || c == ".") && self.live.is_some() => {
                        if let Some(shared) = &self.live {
                            let cur = shared.extra_ms.load(std::sync::atomic::Ordering::Relaxed) as i32;
                            let next = (cur + if c == "," { -10 } else { 10 }).clamp(0, 1500) as u32;
                            shared.extra_ms.store(next, std::sync::atomic::Ordering::Relaxed);
                        }
                        ctx.window.request_redraw();
                        EventResponse::Handled
                    }
                    // r = rotate CW, R (shift) = CCW.
                    Key::Character(c) if c.eq_ignore_ascii_case("r") => {
                        self.rotate(!ctx.modifiers.shift_key(), ctx);
                        EventResponse::Handled
                    }
                    // F: image only, fitted edge to edge — and back. N: image only at native 1:1, centred — and back (composition kept).
                    Key::Character(c) if c.eq_ignore_ascii_case("f") => {
                        let plain = !self.plain;
                        self.set_plain(plain, ctx);
                        self.fit();
                        EventResponse::Handled
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("n") => {
                        let entering = !self.plain;
                        self.set_plain(entering, ctx);
                        if entering {
                            self.cx_frac = 0.5;
                            self.cy_frac = 0.5;
                            self.one_to_one();
                        }
                        EventResponse::Handled
                    }
                    Key::Character(c) if c == "1" => {
                        // 1:1 pixels, about the image-area centre — same exact moment as the button.
                        self.one_to_one();
                        ctx.window.request_redraw();
                        EventResponse::Handled
                    }
                    // Exposure: +/− nudge a third of a stop, 0 resets.
                    Key::Character(c) if c == "+" || c == "=" || c == "-" => {
                        self.nudge_ev(Some(if c == "-" { -1. / 3. } else { 1. / 3. }), ctx);
                        EventResponse::Handled
                    }
                    Key::Character(c) if c == "0" => {
                        self.nudge_ev(None, ctx);
                        EventResponse::Handled
                    }
                    _ => EventResponse::Pass,
                }
            }
            _ => EventResponse::Pass,
        }
    }

    /// Every pill, for hover flips and press polling.
    fn buttons_mut(&mut self) -> Vec<&mut fluor::widgets::Button> {
        #[allow(unused_mut)]
        let mut pills: Vec<&mut fluor::widgets::Button> = vec![&mut self.btn_one, &mut self.btn_fit, &mut self.btn_export, &mut self.btn_info, &mut self.btn_hdr, &mut self.btn_crop, &mut self.btn_rot_ccw, &mut self.btn_rot_cw, &mut self.btn_xscale, &mut self.btn_yscale, &mut self.btn_clip];
        #[cfg(feature = "calibrate")]
        pills.push(&mut self.btn_cal);
        #[cfg(feature = "live")]
        pills.extend([&mut self.btn_live, &mut self.btn_mat_reset, &mut self.btn_mat_save]);
        pills
    }

    /// The pills fired thru the Container walk only bump counters — poll and act. Public so a host that dispatches presses thru its own walk (photon's arbiter) can settle them.
    pub fn poll_pills(&mut self, ctx: &mut Context) {
        if self.btn_one.take_click() {
            // 1:1 — a MOMENT: exactly one image pixel per screen pixel right now; the composition then scales relatively like everything else.
            self.one_to_one();
            ctx.window.request_redraw();
        }
        if self.btn_fit.take_click() {
            self.fit();
            ctx.window.request_redraw();
        }
        if self.btn_export.take_click() {
            self.export();
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
        #[cfg(feature = "calibrate")]
        if self.btn_cal.take_click() {
            self.start_scan(ctx);
        }
        #[cfg(feature = "live")]
        {
            if self.btn_live.take_click() {
                self.toggle_live(ctx);
            }
            if self.btn_mat_reset.take_click() {
                // Reset is the whole starting point: the saved matrix AND exposure back to 0 (its gain rides the stream too).
                self.set_matrix(crate::live::load_matrix().unwrap_or(crate::live::default_matrix()), ctx);
                let zero = ev_zero_of(self.baseline_ev);
                self.ev_slider.set_value(zero);
                self.apply_ev(zero, ctx);
                self.live_status = Some("live: matrix and exposure reset".into());
            }
            if self.btn_mat_save.take_click() {
                let m = self.matrix();
                let extra = self.live.as_ref().map(|s| s.extra_ms.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or_else(crate::live::load_extra_ms);
                match crate::live::save_matrix_and_extra(&m, extra) {
                    Ok(p) => {
                        // Saved with gains folded in; the sliders follow (gains back to 1), as chameleon does.
                        let mut folded = m;
                        for r in 0..3 {
                            for c in 0..3 {
                                folded[r][c] *= m[r][3];
                            }
                            folded[r][3] = 1.;
                        }
                        self.set_matrix(folded, ctx);
                        self.live_status = Some(format!("live: matrix saved → {}", p.display()));
                    }
                    Err(e) => self.live_status = Some(format!("live: save failed: {e}")),
                }
            }
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
            self.toggle_clip(ctx);
        }
    }

    // ── render ──

    /// Draw the view into its area of the host's canvas (front-to-back under-blend, like every fluor consumer: panel content, then the HUD and overlays, then the image, then the backdrop under everything) and stamp its pills into `hit_map`. The load-time fit runs here, the first call guaranteed real dims.
    pub fn render(&mut self, canvas: &mut Canvas, viewport: Viewport, text: &mut fluor::text::TextRenderer, damage_clip: fluor::canvas::PixelRect, cursor: (Coord, Coord), hit_map: &mut [HitId]) {
        if self.needs_fit {
            self.needs_fit = false;
            self.fit();
        }
        let buf_w = canvas.width;
        let buf_h = canvas.height;
        let clip = Some(Clip::new(damage_clip.x0, damage_clip.y0, damage_clip.x1, damage_clip.y1));
        // The whole image transform for this frame, derived from the live area — the ONLY place pixel values exist, and they exist for exactly one frame.
        let (zoom, img_ox, img_oy) = self.view_px();
        let area_x0 = (self.area.x.max(0.) as usize).min(buf_w);
        let area_y0 = (self.area.y.max(0.) as usize).min(buf_h);
        let area_x1 = ((self.area.x + self.area.w).max(0.) as usize).min(buf_w);
        let area_y1 = ((self.area.y + self.area.h).max(0.) as usize).min(buf_h);
        let divider_px = if self.plain { area_x1 } else { (self.divider_x() as usize).clamp(area_x0, area_x1.saturating_sub(1).max(area_x0)) };
        let PanelRects { nav: _, btns, hist, ev: ev_rect, mat, chart } = self.panel_rects(viewport);
        let band = band_of(viewport);

        // ── Right tool panel ── (none with the controls hidden: the divider sits at the area edge and nothing below draws)
        if !self.plain {
            // Divider — 1px vertical hairline down the area (fill_rect's 0-width hairline convention).
            paint::fill_rect(canvas, divider_px as isize, area_y0 as isize, 0, (area_y1 - area_y0) as isize, HAIRLINE, clip, None);
            // Section separators under nav-buttons and hist.
            for &(sx, sy, sw, sh) in &[btns, hist] {
                paint::fill_rect(canvas, sx as isize, (sy + sh) as isize + 4, sw as isize, 0, HAIRLINE, clip, None);
            }
            // 1:1 / magnification / Fit / export / Info — the band splits in fifths: fluor pill Buttons around the LIVE magnification readout (screen px per image px: 1x = pixel-exact certificate, 2.00x = zoomed in past it). The readout derives from the same per-frame span-relative transform as the blit, so it tracks every host op — press 1:1, resize, and it honestly drifts; that's the relative model reporting itself.
            let (bx, by, bw, bh2) = btns;
            // Empty viewer: just the Live pill, full width of the band — start the camera without opening a file first.
            #[cfg(feature = "live")]
            if bw > 2 && bh2 > 0 && self.img_w == 0 {
                let font = bh2 as f32 / 2.;
                self.btn_live.set_rect(bx as f32 + bw as f32 / 2., by as f32 + bh2 as f32 / 2., bw as f32 / 3., bh2 as f32);
                self.btn_live.set_font_size(font);
                let id = self.btn_live.hit_id();
                self.btn_live.render_content_into(canvas, 0., 0., text, clip, Some(&mut *hit_map), id);
            }
            if bw > 2 && bh2 > 0 && self.img_w > 0 {
                let bh = band.min(bh2);
                let quarter = bw as f32 / 5.;
                let font = bh as f32 / 2.;
                let bcy = by as f32 + bh as f32 / 2.;
                // Row 2: rotate CCW / CW, crop — thirds.
                let r2y = (by + bh2 - bh) as f32 + bh as f32 / 2.;
                #[allow(unused_mut)]
                let mut row2: Vec<&mut fluor::widgets::Button> = vec![&mut self.btn_rot_ccw, &mut self.btn_rot_cw, &mut self.btn_crop];
                #[cfg(feature = "calibrate")]
                row2.push(&mut self.btn_cal);
                #[cfg(feature = "live")]
                row2.push(&mut self.btn_live);
                let third = bw as f32 / row2.len() as f32;
                for (i, b) in row2.into_iter().enumerate() {
                    b.set_rect(bx as f32 + third * (i as f32 + 0.5), r2y, third, bh as f32);
                    b.set_font_size(font);
                    let id = b.hit_id();
                    b.render_content_into(canvas, 0., 0., text, clip, Some(&mut *hit_map), id);
                }
                for (b, slot) in [(&mut self.btn_one, 0.5), (&mut self.btn_fit, 2.5), (&mut self.btn_export, 3.5), (&mut self.btn_info, 4.5)] {
                    b.set_rect(bx as f32 + quarter * slot, bcy, quarter, bh as f32);
                    b.set_font_size(font);
                    let id = b.hit_id();
                    b.render_content_into(canvas, 0., 0., text, clip, Some(&mut *hit_map), id);
                }
                // "1x" is a CERTIFICATE, not a rounding: bitwise == against the value one_to_one() stored, so it holds iff the span (area + divider geometry) is unchanged since — the moment a resize makes the image resample, equality breaks and the decimals return. Resize back to the identical geometry and exactness honestly comes back.
                let (_, _, aw, ah) = self.image_area();
                let magnification = if self.zoom_rel == 1. / Self::area_span(aw, ah) {
                    "1x".to_string()
                } else {
                    // TRUNCATED to two decimals, never rounded — the readout may understate but never overstate: 0.9999999 reads "0.99x" (it is NOT yet 1; only the == certificate may say "1x"). Integer decomposition so the formatter can't re-round.
                    let centi = (zoom * 100.).trunc() as u64;
                    format!("{}.{:02}x", centi / 100, centi % 100)
                };
                text.draw_text_center(canvas, &magnification, bx as f32 + quarter * 1.5, (by + bh / 2) as f32, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
            }
            // Exposure slider + EV label. Label takes the band's left end, the slider the rest; the fluor Slider paints the lumis-style white/black track + circular handle.
            let (ex, ey, ew, eh) = ev_rect;
            if ew > 0 && eh > 0 && !self.lin.is_empty() {
                let label_w = eh * 4;
                let font = eh as f32 * (7. / (1 << 3) as f32);
                // Truncated toward zero, never rounded — same integer decomposition as the magnification readout, so the label shows the stored value's truth: three 1/3-stop nudges display "+0.99" because the float accumulation genuinely is a hair under a stop.
                let centi = (self.ev * 100.).trunc() as i32;
                let label = format!("{}{}.{:02}", if centi < 0 { '-' } else { '+' }, (centi / 100).abs(), (centi % 100).abs());
                text.draw_text_left(canvas, &label, ex as f32, (ey + eh / 2) as f32, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
                let sw = ew.saturating_sub(label_w);
                if sw > eh {
                    self.ev_slider.set_rect((ex + label_w + sw / 2) as f32, (ey + eh / 2) as f32, sw as f32, eh as f32);
                    let id = self.ev_slider.hit_id();
                    self.ev_slider.render_content_into(canvas, Some(&mut *hit_map), id);
                }
            }

            // Live colour matrix — three rows of four sliders ([r in][g in][b in][gain] per output row, labelled R/G/B), then Reset/Save. Slider positions map −12…+12 (gain 0…12) like chameleon's window; the numbers themselves read in the HUD.
            #[cfg(feature = "live")]
            {
                let (mx, my, mw, mh) = mat;
                if mh > 0 && mw > 0 {
                    let font = band as f32 * (7. / (1 << 3) as f32);
                    let label_w = band * 2;
                    let sw = (mw.saturating_sub(label_w)) as f32 / 4.;
                    for r in 0..3 {
                        let cy = (my + r * band) as f32 + band as f32 / 2.;
                        text.draw_text_left(canvas, ["R", "G", "B"][r], mx as f32, cy, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
                        for c in 0..4 {
                            let sl = &mut self.mat_sliders[r * 4 + c];
                            let cx = mx as f32 + label_w as f32 + sw * (c as f32 + 0.5);
                            sl.set_rect(cx, cy, sw * 0.9, band as f32 * 0.8);
                            let id = sl.hit_id();
                            sl.render_content_into(canvas, Some(&mut *hit_map), id);
                        }
                    }
                    let ry = (my + band * 3 + pad_of(viewport)) as f32 + band as f32 / 2.;
                    for (i, b) in [&mut self.btn_mat_reset, &mut self.btn_mat_save].into_iter().enumerate() {
                        b.set_rect(mx as f32 + mw as f32 * (0.25 + 0.5 * i as f32), ry, mw as f32 / 3., band as f32);
                        b.set_font_size(band as f32 / 2.);
                        let id = b.hit_id();
                        b.render_content_into(canvas, 0., 0., text, clip, Some(&mut *hit_map), id);
                    }
                    // Mic meter — a real bar, fixed place and width: the full section width under Reset/Save. −60…0 dBFS left to right, green to −12, yellow to −3, red above; a dark trough behind it, a hairline at each 12 dB. Label at the left end, dB at the right.
                    if let Some(shared) = &self.live {
                        let peak = f32::from_bits(shared.mic_peak.load(std::sync::atomic::Ordering::Relaxed));
                        let db = if peak > 0. { (20. * peak.log10()).max(-60.) } else { -60. };
                        let label_w = band * 2;
                        let (tx, ty, tw, th) = (mx + label_w, my + band * 4 + pad_of(viewport) * 2, mw.saturating_sub(label_w + band * 3), (band * 3 / 5).max(3));
                        let cy = ty as f32 + th as f32 / 2.;
                        text.draw_text_left(canvas, "mic", mx as f32, cy, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
                        // Front-to-back: the level fill and the hairlines first, the trough LAST so it shows only where nothing else did.
                        let fill = ((db + 60.) / 60. * tw as f32) as usize;
                        let zone = |lo_db: f32, hi_db: f32| -> (usize, usize) { (((lo_db + 60.) / 60. * tw as f32) as usize, ((hi_db + 60.) / 60. * tw as f32) as usize) };
                        for (lo, hi, colour) in [(-60., -12., METER_GREEN), (-12., -3., METER_YELLOW), (-3., 0., METER_RED)] {
                            let (z0, z1) = zone(lo, hi);
                            let end = fill.min(z1);
                            if end > z0 {
                                paint::fill_rect(canvas, (tx + z0) as isize, ty as isize, (end - z0) as isize, th as isize, colour, clip, None);
                            }
                        }
                        for mark in [-48., -36., -24., -12.] {
                            let x = tx + ((mark + 60.) / 60. * tw as f32) as usize;
                            paint::fill_rect(canvas, x as isize, ty as isize, 0, th as isize, HAIRLINE, clip, None);
                        }
                        paint::fill_rect(canvas, tx as isize, ty as isize, tw as isize, th as isize, METER_TROUGH, clip, None);
                        text.draw_text_right(canvas, &format!("{db:.0} dB"), (mx + mw) as f32, cy, &fluor::text::TextStyle::new(font, TEXT_GREY), clip, None);
                    }
                }
            }
            #[cfg(not(feature = "live"))]
            let _ = mat;

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
                    b.render_content_into(canvas, 0., 0., text, clip, Some(&mut *hit_map), id);
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
                    (area_y0..area_y1)
                        .into_par_iter()
                        .with_min_len(((area_y1 - area_y0) / 8).max(1))
                        .fold(
                            || vec![[0u32; 3]; 1 << 16],
                            |mut c, sy| {
                                let fy = (sy as f32 - img_oy) / zoom;
                                if fy >= 0. && (fy as usize) < img_h {
                                    for sx in area_x0..divider_px {
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
                let dens = self.raw.spread(&codes, self.hist_xlog, bins, self.total_ev().exp2());
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
                        put(canvas.pixels, buf_w, hx + col, hy + row, hist_px[row * hw + col]);
                    }
                }
            }
            // Navigator thumbnail — nearest blit into the ASPECT-FITTED sub-rect (letterboxed, centered): the navigator never stretches the image. `nav_fit` is the shared truth for blit, view-rect overlay, and cursor mapping.
            let fitted = self.nav_fit(viewport);
            if let Some((fx, fy, fw, fh)) = fitted {
                for ty in 0..fh.min(buf_h.saturating_sub(fy)) {
                    let sy = ty * self.tools.thumb_h / fh;
                    for tx in 0..fw.min(buf_w.saturating_sub(fx)) {
                        let sx = tx * self.tools.thumb_w / fw;
                        put(canvas.pixels, buf_w, fx + tx, fy + ty, self.tools.thumb[sy * self.tools.thumb_w + sx]);
                    }
                }
            }
            // Navigator view rect — the TRUE viewport rect in thumb space, unclamped (it slides off the thumb edge when panned past the image instead of shrinking and sticking), drawn AFTER the thumbnail as a wrapping-add-of-128 marker on each gamma-encoded RGB byte (`b ^ 0x80` — self-contrasting on any content).
            if let (Some((fx, fy, fw, fh)), true) = (fitted, zoom > 0. && self.img_w > 0) {
                let sx = fw as f32 / (self.img_w as f32 * zoom);
                let sy = fh as f32 / (self.img_h as f32 * zoom);
                let rx0 = fx as isize + ((area_x0 as f32 - img_ox) * sx) as isize;
                let ry0 = fy as isize + ((area_y0 as f32 - img_oy) * sy) as isize;
                let rx1 = fx as isize + ((divider_px as f32 - img_ox) * sx) as isize;
                let ry1 = fy as isize + ((area_y1 as f32 - img_oy) * sy) as isize;
                let (cx0, cy0) = (fx as isize, fy as isize);
                let (cx1, cy1) = ((fx + fw) as isize, (fy + fh) as isize);
                let target = &mut *canvas.pixels;
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
                let target = &mut *canvas.pixels;
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
                    // Parallel fold over screen rows, split into few bands so the per-band grid merges stay far below the sample splats.
                    (area_y0..area_y1)
                        .into_par_iter()
                        .with_min_len(((area_y1 - area_y0) / 8).max(1))
                        .fold(
                            || vec![0u32; dw * dh],
                            |mut grid, sy| {
                                let fy = (sy as f32 - img_oy) / zoom;
                                if fy >= 0. && (fy as usize) < img_h {
                                    let row = (fy as usize) * img_w;
                                    for sx in area_x0..divider_px {
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
                            put(canvas.pixels, buf_w, ox + px, oy + py, v);
                        }
                    }
                }
            }
            // Panel background under all tool content.
            paint::fill_rect(canvas, (divider_px + 1) as isize, area_y0 as isize, (area_x1.saturating_sub(divider_px + 1)) as isize, (area_y1 - area_y0) as isize, PANEL_BG, clip, None);
        }

        // Frame info HUD — bottom-left of the image area, drawn BEFORE the image so the under-compose order puts it on top: the static frame lines, then the live view state and the cursor readout (raw ADC codes of every sample in the tile under the cursor + the linear display value). A translucent dark backdrop keeps it legible over any content.
        if self.show_info && self.img_w > 0 && !self.plain {
            let bandf = band as f32;
            // Readable at a glance, not a footnote: the same size as the pill labels' band, ~2× the panel captions.
            let font = bandf * 0.9;
            let line_h = font * 1.3;
            let pad = bandf / 2.;
            let mut lines = self.frame_lines.clone();
            // With a baseline in play the slider's 0 is not "no gain" — it is "as the file says", so the HUD spells out both and their sum rather than leaving the difference invisible.
            let ev_part = if self.baseline_ev.abs() > 1e-4 { format!("EV {:+.2}  baseline {:+.2}  total {:+.2}", self.ev, self.baseline_ev, self.total_ev()) } else { format!("EV {:+.2}", self.ev) };
            lines.push(format!("{ev_part}  zoom {}x  clip {}  hdr {}", trim_f(zoom as f64, 3), if self.clip_show { "on" } else { "off" }, if self.hdr { "on (3x−x³)/2" } else { "off" }));
            if let Some((x0, y0, x1, y1)) = self.crop {
                lines.push(format!("crop {x0},{y0}  {}×{}  (click: nearest corner to cursor)", x1 - x0, y1 - y0));
            }
            #[cfg(feature = "calibrate")]
            if let Some(r) = &self.cal_readout {
                lines.push(r.clone());
            }
            #[cfg(feature = "live")]
            if self.live.is_some() {
                if let Some(st) = &self.live_status {
                    lines.push(st.clone());
                }
                let m = self.matrix();
                for r in 0..3 {
                    lines.push(format!("   {} {:+.3} {:+.3} {:+.3}  ×{:.2}", ["R", "G", "B"][r], m[r][0], m[r][1], m[r][2], m[r][3]));
                }
                if let Some(shared) = &self.live {
                    let pipe = shared.pipe_ms10.load(std::sync::atomic::Ordering::Relaxed) as f32 / 10.;
                    let extra = shared.extra_ms.load(std::sync::atomic::Ordering::Relaxed);
                    lines.push(format!("A/V: opsin {pipe:.0} ms + camera/downstream {extra} ms (, . adjust) = mic delayed {:.0} ms", pipe + extra as f32));
                    if let Some(p) = shared.record.lock().unwrap().as_ref() {
                        let frames = shared.rec_frames.load(std::sync::atomic::Ordering::Relaxed);
                        let secs = frames / 15;
                        lines.push(format!("● REC {:02}:{:02}:{:02}  → {}   (Space stops)", secs / 3600, secs / 60 % 60, secs % 60, p.display()));
                    }
                }
            }
            if let Some((px, py)) = self.image_px_at(cursor.0, cursor.1) {
                let raw: Vec<String> = self.raw.samples_at(px, py).into_iter().map(|(ch, v)| format!("{} {v}", self.channel_names.get(ch).map(String::as_str).unwrap_or("?"))).collect();
                let i = (py * self.img_w + px) * 3;
                let lin = if i + 2 < self.lin.len() { format!("  lin {:.4} {:.4} {:.4}", self.lin[i] as f32 / 65535., self.lin[i + 1] as f32 / 65535., self.lin[i + 2] as f32 / 65535.) } else { String::new() };
                lines.push(format!("px {px},{py}  raw {}{lin}", raw.join(" ")));
            }
            let style = fluor::text::TextStyle::new(font, TEXT_GREY);
            let x0 = self.area.x + pad;
            let box_h = line_h * lines.len() as f32 + pad;
            let y_top = area_y1 as f32 - pad - box_h;
            let mut max_w: f32 = 0.;
            for (i, line) in lines.iter().enumerate() {
                let y = y_top + pad / 2. + line_h * (i as f32 + 0.5);
                let w = text.draw_text_left(canvas, line, x0 + pad / 2., y, &style, clip, None);
                max_w = max_w.max(w);
            }
            let box_w = (max_w + pad).min(divider_px as f32 - x0);
            paint::fill_rect(canvas, x0 as isize, y_top as isize, box_w as isize, box_h as isize, HUD_BG, clip, None);

            // Key hints — bottom-right of the image area, dimmer than the readings, right-aligned so they hang off the edge the panel starts at. Only the keys this build has.
            let mut hints: Vec<&str> = vec![
                "← →  next / previous",
                "F  image only, fitted    N  image only, 1:1",
                "1  1:1    I  info    E  export    V  convert to VSF",
                "+ − 0  exposure    H  HDR    X  crop    r R  rotate",
            ];
            if self.source.is_some() {
                hints.push("Ctrl+C / Ctrl+V  copy / paste IDT  (Shift forces)");
            }
            #[cfg(feature = "calibrate")]
            hints.push("C  calibrate (scan for target)");
            #[cfg(feature = "live")]
            hints.push("L  live camera    Space  record    , .  A/V delay ±10 ms");
            hints.push("Esc  close");
            let hint_style = fluor::text::TextStyle::new(font * 0.85, HINT_GREY);
            let hint_h = line_h * 0.85;
            let hbox_h = hint_h * hints.len() as f32 + pad;
            let hy_top = area_y1 as f32 - pad - hbox_h;
            let hx1 = divider_px as f32 - pad;
            let hbox_w = hints.iter().map(|l| text.measure_text(l, &hint_style)).fold(0f32, f32::max) + pad;
            // Only if it clears the readings box — on a narrow area the readings win.
            if hx1 - hbox_w > x0 + box_w + pad {
                for (i, line) in hints.iter().enumerate() {
                    let y = hy_top + pad / 2. + hint_h * (i as f32 + 0.5);
                    text.draw_text_right(canvas, line, hx1 - pad / 2., y, &hint_style, clip, None);
                }
                paint::fill_rect(canvas, (hx1 - hbox_w) as isize, hy_top as isize, hbox_w as isize, hbox_h as isize, HUD_BG, clip, None);
            }
        }

        // Crop overlay — drawn BEFORE the image so under-compose puts it on top: everything outside the rect dims to the HUD tone, and a hairline rings the rect. Screen coords straight from the same per-frame transform as the blit.
        if let Some((x0, y0, x1, y1)) = self.crop {
            let sx0 = (img_ox + x0 as f32 * zoom).round() as isize;
            let sy0 = (img_oy + y0 as f32 * zoom).round() as isize;
            let sx1 = (img_ox + x1 as f32 * zoom).round() as isize;
            let sy1 = (img_oy + y1 as f32 * zoom).round() as isize;
            let (ax0, ay0, ax1, ay1) = (area_x0 as isize, area_y0 as isize, divider_px as isize, area_y1 as isize);
            // Four bands: above, below, left, right of the rect — clipped to the image area.
            let band_fill = |c: &mut Canvas, x0: isize, y0: isize, x1: isize, y1: isize| {
                let (x0, y0, x1, y1) = (x0.max(ax0), y0.max(ay0), x1.min(ax1), y1.min(ay1));
                if x1 > x0 && y1 > y0 {
                    paint::fill_rect(c, x0, y0, x1 - x0, y1 - y0, HUD_BG, clip, None);
                }
            };
            band_fill(canvas, ax0, ay0, ax1, sy0);
            band_fill(canvas, ax0, sy1, ax1, ay1);
            band_fill(canvas, ax0, sy0, sx0, sy1);
            band_fill(canvas, sx1, sy0, ax1, sy1);
            for (x, y, w, h) in [(sx0, sy0, sx1 - sx0, 0), (sx0, sy1 - 1, sx1 - sx0, 0), (sx0, sy0, 0, sy1 - sy0), (sx1 - 1, sy0, 0, sy1 - sy0)] {
                if x >= ax0 && x < ax1 && y >= ay0 && y < ay1 {
                    paint::fill_rect(canvas, x, y, w.min(ax1 - x), h.min(ay1 - y), TEXT_GREY, clip, None);
                }
            }
        }

        // Calibration overlay — the solved target grid warped back onto the frame, drawn BEFORE the image so it composes on top. It lives in RAW mosaic coordinates: every display pixel walks the same orientation bridge as the histogram to its sensor position, then ×tile into raw space, so the overlay tracks pan, zoom, rotation and EV with the image and can never drift from it.
        #[cfg(feature = "calibrate")]
        if let (Some(ov), true) = (&self.cal_overlay, self.img_w > 0) {
            let gain = self.total_ev().exp2();
            let (tw, th) = (self.raw.tile_w.max(1), self.raw.tile_h.max(1));
            let x0 = (img_ox.max(0.) as usize).max(area_x0);
            let y0 = (img_oy.max(0.) as usize).max(area_y0);
            let x1 = ((img_ox + self.img_w as f32 * zoom).max(0.) as usize).min(divider_px);
            let y1 = ((img_oy + self.img_h as f32 * zoom).max(0.) as usize).min(area_y1);
            for sy in y0..y1 {
                let dy = ((sy as f32 - img_oy) / zoom) as usize;
                if dy >= self.img_h {
                    continue;
                }
                for sx in x0..x1 {
                    let dx = ((sx as f32 - img_ox) / zoom) as usize;
                    if dx >= self.img_w {
                        continue;
                    }
                    let (px, py) = self.raw.sensor_of(dx, dy);
                    let (rx, ry) = (px * tw, py * th);
                    if rx < ov.x0 || ry < ov.y0 || rx >= ov.x0 + ov.w || ry >= ov.y0 + ov.h {
                        continue;
                    }
                    let i = ((ry - ov.y0) * ov.w + (rx - ov.x0)) * 4;
                    let a = ov.rgba[i + 3].clamp(0., 1.);
                    if a <= 0. {
                        continue;
                    }
                    let enc = |v: f32| ((v * gain).clamp(0., 1.).sqrt() * 255.) as u8;
                    let colour = argb(enc(ov.rgba[i]), enc(ov.rgba[i + 1]), enc(ov.rgba[i + 2]), (a * 255.) as u8);
                    put(canvas.pixels, buf_w, sx, sy, colour);
                }
            }
        }

        // Nearest-neighbour blit of the image rect ∩ image area (left of the divider, inside the area). Per-row source index precomputed once; per-pixel work is one under() compose.
        let x0 = (img_ox.max(0.) as usize).max(area_x0);
        let y0 = (img_oy.max(0.) as usize).max(area_y0);
        let x1 = ((img_ox + self.img_w as f32 * zoom).max(0.) as usize).min(divider_px);
        let y1 = ((img_oy + self.img_h as f32 * zoom).max(0.) as usize).min(area_y1);
        let target = &mut *canvas.pixels;
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

        // Backdrop under everything else, the area only.
        for sy in area_y0..area_y1 {
            for px in &mut target[sy * buf_w + area_x0..sy * buf_w + area_x1] {
                *px = px.under(BACKDROP, BlendMode::Normal);
            }
        }
    }
}

impl Container for View {
    fn visit(&mut self, f: &mut dyn FnMut(&mut dyn fluor::host::widget::Widget)) {
        f(&mut self.btn_one);
        f(&mut self.btn_fit);
        f(&mut self.btn_export);
        f(&mut self.btn_info);
        f(&mut self.btn_hdr);
        f(&mut self.btn_crop);
        f(&mut self.btn_rot_ccw);
        f(&mut self.btn_rot_cw);
        #[cfg(feature = "calibrate")]
        f(&mut self.btn_cal);
        #[cfg(feature = "live")]
        {
            f(&mut self.btn_live);
            f(&mut self.btn_mat_reset);
            f(&mut self.btn_mat_save);
            for s in &mut self.mat_sliders {
                f(s);
            }
        }
        f(&mut self.btn_xscale);
        f(&mut self.btn_yscale);
        f(&mut self.btn_clip);
        f(&mut self.ev_slider);
    }
}

/// Carry a display-space rect `[x0, x1) × [y0, y1)` on an `ow × oh` frame through a 90° turn of that frame. CW maps a point (x, y) → (oh − 1 − y, x), so the x-extent becomes [oh − y1, oh − y0) and the y-extent [x0, x1); CCW maps (x, y) → (y, ow − 1 − x).
fn rotate_rect((x0, y0, x1, y1): (usize, usize, usize, usize), ow: usize, oh: usize, cw: bool) -> (usize, usize, usize, usize) {
    if cw { (oh - y1, x0, oh - y0, x1) } else { (y0, ow - x1, y1, ow - x0) }
}

/// The panel's section padding — RU-coherent, derived from the host viewport's span.
fn pad_of(viewport: Viewport) -> usize {
    (viewport.effective_span() / (1 << 7) as f32).ceil() as usize
}

/// The panel's control band height (pill rows, the slider).
fn band_of(viewport: Viewport) -> usize {
    (viewport.effective_span() / (1 << 6) as f32).ceil() as usize
}

/// Live matrix slider mapping: columns 0..3 span −12…+12, the gain column 0…12 (chameleon's ranges).
#[cfg(feature = "live")]
fn mat_slider_pos(i: usize, v: f32) -> f32 {
    if i % 4 == 3 { (v / 12.).clamp(0., 1.) } else { ((v + 12.) / 24.).clamp(0., 1.) }
}
#[cfg(feature = "live")]
fn mat_slider_val(i: usize, pos: f32) -> f32 {
    if i % 4 == 3 { pos * 12. } else { pos * 24. - 12. }
}

/// Under-compose one pixel, bounds-unchecked by construction (callers clamp rects to the buffer).
#[inline]
fn put(target: &mut [u32], buf_w: usize, x: usize, y: usize, colour: u32) {
    let i = y * buf_w + x;
    target[i] = target[i].under(colour, BlendMode::Normal);
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
    fn rotate_linear_turns_the_buffer_and_four_turns_return_it() {
        // 3×2 image, pixel value = its index so every position is distinct.
        let (w, h) = (3, 2);
        let lin: Vec<i32> = (0..(w * h) as i32).flat_map(|i| [i, i, i]).collect();
        let cw = rotate_linear(&lin, w, h, true);
        // CW: source (0,0) lands top-right of the 2×3 result → x' = h-1 = 1, y' = 0 → index 1.
        assert_eq!(cw[1 * 3], 0);
        // Source (2,1) (bottom-right) lands bottom-left → x' = 0, y' = 2 → index 2*2+0 = 4.
        assert_eq!(cw[4 * 3], 5);
        let ccw = rotate_linear(&cw, h, w, false);
        assert_eq!(ccw, lin);
        let mut x = lin.clone();
        let (mut cw_w, mut cw_h) = (w, h);
        for _ in 0..4 {
            x = rotate_linear(&x, cw_w, cw_h, true);
            std::mem::swap(&mut cw_w, &mut cw_h);
        }
        assert_eq!(x, lin);
    }

    #[test]
    fn fold_keeps_row_sums_exact_and_small_images_untouched() {
        let (w, h) = (4, 2);
        let lin: Vec<i32> = (0..(w * h * 3) as i32).collect();
        let (fw, fh, same) = fold_linear(lin.clone(), w, h, 8);
        assert_eq!((fw, fh), (w, h));
        assert_eq!(same, lin);
        let (fw, fh, folded) = fold_linear(lin, w, h, 2);
        assert_eq!((fw, fh), (2, 1));
        // Each output pixel is the mean of a 2×2 block; the carried remainder keeps the row's channel sums exact.
        assert_eq!(folded.len(), 6);
    }

    #[test]
    fn the_slider_brackets_the_total_not_the_operators_dial() {
        // With no baseline, nothing moves: the track is EV_MIN..EV_MAX of operator stops and 0 sits at EV_ZERO.
        assert!((ev_of_slider(0., 0.) - EV_MIN).abs() < 1e-5);
        assert!((ev_of_slider(1., 0.) - EV_MAX).abs() < 1e-5);
        assert!((ev_zero_of(0.) - EV_ZERO).abs() < 1e-5);

        // A file declaring +4 still reaches EV_MIN..EV_MAX of TOTAL — which is the whole point.
        let b = 4.;
        let total = |v: f32| ev_of_slider(v, b) + b;
        assert!((total(0.) - EV_MIN).abs() < 1e-5, "full left reaches four stops BELOW the capture, not merely back to it");
        assert!((total(1.) - EV_MAX).abs() < 1e-5);

        // The regression this fixes: the old mapping bottomed out at operator -4, i.e. total 0, where sensor
        // saturation lands exactly on display white — so a blown sky stayed white however far you pulled.
        assert!(ev_of_slider(0., b) < -4., "full left must go below -4 operator stops when the file declares +4");

        // "As the file says" stays reachable, and its detent slides along the track with the baseline.
        assert!((ev_of_slider(ev_zero_of(b), b)).abs() < 1e-5, "the detent is still operator EV 0");
        assert!(ev_zero_of(b) > ev_zero_of(0.), "a positive baseline pushes the zero detent up the track");

        // Round trip, both directions, baseline or not.
        for &bl in &[0., 4., -2.5] {
            for &ev in &[-3., 0., 2., 7.] {
                let v = slider_of_ev(ev, bl);
                if v > 0. && v < 1. {
                    assert!((ev_of_slider(v, bl) - ev).abs() < 1e-4, "round trip at baseline {bl} ev {ev}");
                }
            }
        }
    }

    #[test]
    fn spread_gain_shifts_bins_and_grows_clip_spike() {
        let raw = RawView { counts: Vec::new(), sensor_w: 0, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: 0, black: [0.; 3], white: [65535.; 3], bits: 16, orient: 1, pre_w: 0, pre_h: 0, or_w: 0, or_h: 0, fold_w: 0, fold_h: 0, census: [1.; 3] };
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

    #[test]
    fn folded_raw_bridge_scales_a_display_pixel_to_the_sensor() {
        // A planar 8×4 sensor shown folded to 4×2: display (3, 1) → full (6, 2).
        let counts: Vec<u16> = (0..(8 * 4 * 3) as u16).collect();
        let raw = RawView { counts, sensor_w: 8, tile_w: 1, tile_h: 1, cfa: Vec::new(), planar_n: 32, black: [0.; 3], white: [255.; 3], bits: 8, orient: 1, pre_w: 8, pre_h: 4, or_w: 8, or_h: 4, fold_w: 4, fold_h: 2, census: [1.; 3] };
        assert_eq!(raw.sensor_of(3, 1), (6, 2));
        let s = raw.samples_at(3, 1);
        assert_eq!(s[0], (0, (2 * 8 + 6) as u16));
        assert!(raw.samples_at(4, 0).is_empty(), "past the folded width is off-image");
    }
}
