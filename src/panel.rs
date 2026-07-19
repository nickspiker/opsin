//! Right tool panel: navigator, histogram, chromaticity chart. Layout is a fraction of window width (RU-coherent, user-draggable at the divider); sections stack vertically.
//!
//! The histogram bins RAW SENSOR COUNTS — pre-matrix, pre-debayer, black-subtracted, and VIEW-LIVE like the chart: every frame, the visible display pixels map back through the orientation bridge to their sensor tiles and every raw sample in those tiles bins into its CFA channel. The x-axis is the sensor's own range (0 = black level, right edge = saturation) in LINEAR counts, or log2 STOPS (0 at saturation down to −bitdepth) behind an explicit toggle — a labelled remap, never a silent curve: in linear a one-stop exposure difference moves every peak exactly 2×. Bins run at [`HIST_OVERSAMPLE`]× the display width; stop gridlines XOR whole vertical hairlines into the OVERSAMPLED columns, so the box-average renders them sub-pixel. Bar heights are oriel's: percentile-normalized counts (87.5th percentile of the nonzero bins → 1/8 height), √-compressed VERTICALLY, additive RGB channels composing toward white.
//!
//! The chart is oriel's Maxwell-triangle, recomputed EVERY FRAME at the exact display size from exactly the pixels visible in the image area — no base resolution, no resampling, no fixed pixel ratio. Pan or zoom and the cloud tracks what you're looking at; chromaticity is exposure-invariant (ratios shrug at a scalar gain), so the EV slider leaves it put. Display-RGB ratios `x = (r + g/2)/(r+g+b)`, `y = 1 − g/(r+g+b)` over an equilateral triangle, density brightening the ramp tint under a 1/5-power compression + 1/8 floor, the spectral locus and Planckian arc drawn as Wu-AA polylines COLOURED BY THEIR OWN LIGHT, the line of purples, a soft D65 dot, and a white crosshair at the display neutral. All overlay weights scale from oriel's 360-wide authoring, so at typical panel sizes it IS oriel 1:1.
//!
//! Everything observer-dependent derives on the fly from an [`Observer`] — cone fundamentals + the LMS→RGB bridge — never baked coordinates, so swapping observers is constructing a different `Observer` and re-rendering.
//!
use vsf::colour::spectrum::ConstSpectrum;
use vsf::colour::{LMS2VSF_RGB, LMS_2000_10DEG_1NM};

/// Histogram bins per display column — binning runs at this multiple of the plot width and the render box-averages back down (oriel's oversample + average), which anti-aliases bar profiles and lets the XOR stop-hairlines land at fractional-pixel weight.
pub const HIST_OVERSAMPLE: usize = 8;
/// Density display: brightness = FLOOR + (1−FLOOR)·(d/dmax)^(1/GAMMA) — oriel's gamut-plot constants.
const DENSITY_FLOOR: f32 = 1. / 8.;
const DENSITY_GAMMA: f32 = 5.;
/// Oriel's histogram normalization: the 87.5th-percentile nonzero bin maps to this fraction of full height (pre-√). Peaks clip; the mid-population stays readable.
const HIST_SCALE: f32 = 1. / 8.;
/// D65 whitepoint LM chromaticity (S ≡ 1) — oriel's constant for the CIE 2006 observer.
const D65_LM: [f32; 2] = [0.881419807891581, 0.916420766198352];
/// Oriel authored its overlay sizes (line weight, D65 dot, crosshair) against a 360-wide plot; weights scale by `chart_w/360` so any display size carries the same proportions.
const ORIEL_PLOT_W: f32 = 360.;

/// The observer: cone fundamentals + the LMS→RGB bridge. Everything the chart draws in chromaticity space derives from this at render time — swap the observer, re-render, done. (The inverse bridge returns when per-image spectral characterization drives a real resolve.)
pub struct Observer {
    pub spectrum: &'static ConstSpectrum,
    pub lms2rgb: [f32; 9],
}

/// Transpose a 3×3 — vsf::colour stores its matrices column-major; the panel applies them row-major (same bridge as convert.rs's `t3`).
const fn t3(m: [f32; 9]) -> [f32; 9] {
    [m[0], m[3], m[6], m[1], m[4], m[7], m[2], m[5], m[8]]
}

impl Observer {
    /// Stockman & Sharpe 2000 10° with the VSF-RGB bridge — the stock human.
    pub fn stock() -> Self {
        Self { spectrum: &LMS_2000_10DEG_1NM, lms2rgb: t3(LMS2VSF_RGB) }
    }
}

/// Everything the panel precomputes per image — just the navigator thumbnail now: histogram and chart are both per-frame renders from what's in view (see [`render_hist`] / [`render_chart`]).
pub struct PanelTools {
    /// Navigator thumbnail, α+darkness packed, nearest-sampled from the display pixels.
    pub thumb: Vec<u32>,
    pub thumb_w: usize,
    pub thumb_h: usize,
}

/// visible RGB + α → α+darkness u32 (saturating on all channels).
fn pack(r: f32, g: f32, b: f32, a: f32) -> u32 {
    let d = |v: f32| 255 - (v.clamp(0., 255.) as u32);
    ((a.clamp(0., 255.) as u32) << 24) | (d(r) << 16) | (d(g) << 8) | d(b)
}

/// α+darkness u32 → (α, visible RGB bytes as f32).
fn unpack(v: u32) -> (f32, [f32; 3]) {
    let vis = |shift: u32| (255 - ((v >> shift) & 0xFF)) as f32;
    ((v >> 24) as f32, [vis(16), vis(8), vis(0)])
}

/// Oriel's Maxwell-triangle projection of display RGB → chart coords at `w × h`. `None` when the ratios are undefined (zero or non-finite sum) — negative components are allowed (out-of-gamut locus points land outside the triangle and clip at the buffer edge, exactly as oriel draws them).
pub fn project(r: f32, g: f32, b: f32, w: usize, h: usize) -> Option<(f32, f32)> {
    let sum = r + g + b;
    if !sum.is_finite() || sum == 0. {
        return None;
    }
    let gn = g / sum;
    Some(((r / sum + 0.5 * gn) * w as f32, (1. - gn) * h as f32))
}

/// Row-major 3×3 apply: out[o] = m[o·3..]·v.
fn mat3(m: &[f32; 9], v: [f32; 3]) -> [f32; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2],
        m[3] * v[0] + m[4] * v[1] + m[5] * v[2],
        m[6] * v[0] + m[7] * v[1] + m[8] * v[2],
    ]
}

/// Peak-normalize an RGB to 0..1 for line colouring (oriel's `rgb_scale`); negatives ride through and saturate to 0 at pack.
fn peak_norm(rgb: [f32; 3]) -> [f32; 3] {
    let peak = rgb[0].max(rgb[1]).max(rgb[2]);
    if peak > 0. { [rgb[0] / peak, rgb[1] / peak, rgb[2] / peak] } else { [0.; 3] }
}

/// Xiaolin Wu AA line with a colour gradient along its length — oriel's `draw_line_u8` on the α+darkness buffer, at oriel's stroke weight scaled to this chart width (`w/360` minor-axis passes). Coverage blends at gamma 2 (`alias = √c`); over transparent pixels the colour lands directly with α = coverage.
fn draw_gradient_line(buf: &mut [u32], w: usize, h: usize, x0: f32, y0: f32, x1: f32, y1: f32, c0: [f32; 3], c1: [f32; 3]) {
    let weight = ((w as f32 / ORIEL_PLOT_W).round() as i32).max(1);
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    for pass in 0..weight {
        let off = (pass - weight / 2) as f32;
        if steep {
            wu_line(buf, w, h, x0 + off, y0, x1 + off, y1, c0, c1);
        } else {
            wu_line(buf, w, h, x0, y0 + off, x1, y1 + off, c0, c1);
        }
    }
}

fn wu_line(buf: &mut [u32], bw: usize, bh: usize, x0: f32, y0: f32, x1: f32, y1: f32, c0: [f32; 3], c1: [f32; 3]) {
    let total = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt().max(1e-6);
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    let (mut x0, mut y0, mut x1, mut y1) = (x0, y0, x1, y1);
    let mut flipped = false;
    if steep {
        std::mem::swap(&mut x0, &mut y0);
        std::mem::swap(&mut x1, &mut y1);
    }
    if x0 > x1 {
        std::mem::swap(&mut x0, &mut x1);
        std::mem::swap(&mut y0, &mut y1);
        flipped = true;
    }
    let dx = x1 - x0;
    let gradient = if dx == 0. { 1. } else { (y1 - y0) / dx };

    let mut plot = |mx: isize, my: isize, c: f32, t: f32| {
        // (mx, my) in the possibly-swapped major/minor frame; un-swap for the buffer.
        let (x, y) = if steep { (my, mx) } else { (mx, my) };
        if x < 0 || x >= bw as isize || y < 0 || y >= bh as isize || c <= 0. {
            return;
        }
        let t = if flipped { 1. - t } else { t };
        let colour = [c0[0] + (c1[0] - c0[0]) * t, c0[1] + (c1[1] - c0[1]) * t, c0[2] + (c1[2] - c0[2]) * t];
        let alias = c.min(1.).sqrt();
        let idx = y as usize * bw + x as usize;
        let (oa, old) = unpack(buf[idx]);
        if oa <= 0. {
            buf[idx] = pack(colour[0] * 256., colour[1] * 256., colour[2] * 256., alias * 255.);
        } else {
            let ch = |i: usize| colour[i] * alias * 256. + (1. - alias) * old[i];
            buf[idx] = pack(ch(0), ch(1), ch(2), alias * 255. + (1. - alias) * oa);
        }
    };

    let xend = (x0 + 0.5).floor();
    let yend = y0 + gradient * (xend - x0);
    let xgap = 1. - (x0 + 0.5).fract();
    let xpxl1 = xend;
    let ypxl1 = yend.floor();
    plot(xpxl1 as isize, ypxl1 as isize, (1. - yend.fract()) * xgap, 0.);
    plot(xpxl1 as isize, ypxl1 as isize + 1, yend.fract() * xgap, 0.);
    let mut intery = yend + gradient;

    let xend2 = (x1 + 0.5).floor();
    let yend2 = y1 + gradient * (xend2 - x1);
    let xgap2 = (x1 + 0.5).fract();
    let xpxl2 = xend2;
    let ypxl2 = yend2.floor();
    plot(xpxl2 as isize, ypxl2 as isize, (1. - yend2.fract()) * xgap2, 1.);
    plot(xpxl2 as isize, ypxl2 as isize + 1, yend2.fract() * xgap2, 1.);

    for x in (xpxl1 as isize + 1)..(xpxl2 as isize) {
        let t = ((x as f32 - x0).powi(2) + (intery - y0).powi(2)).sqrt() / total;
        plot(x, intery.floor() as isize, 1. - intery.fract(), t);
        plot(x, intery.floor() as isize + 1, intery.fract(), t);
        intery += gradient;
    }
}

/// Nearest-sample navigator thumbnail, long axis capped at 512 (source-data cap, not a display size).
fn thumb_from(pixels: &[u32], w: usize, h: usize) -> (Vec<u32>, usize, usize) {
    if w == 0 || h == 0 {
        return (Vec::new(), 0, 0);
    }
    let scale = ((1 << 9) as f32 / w.max(h) as f32).min(1.);
    let tw = ((w as f32 * scale) as usize).max(1);
    let th = ((h as f32 * scale) as usize).max(1);
    let mut t = Vec::with_capacity(tw * th);
    for ty in 0..th {
        let sy = ty * h / th;
        for tx in 0..tw {
            t.push(pixels[sy * w + tx * w / tw]);
        }
    }
    (t, tw, th)
}

/// Render the view-live histogram into `hw × hh` α+darkness pixels from oversampled per-channel bin DENSITIES (`dens.len() == hw·HIST_OVERSAMPLE`, equal-energy spread by the caller from the visible raw samples). Vertical is an honest axis either way: `y_log` false ⇒ height ∝ density (87.5th-percentile nonzero density → HIST_SCALE of full height, taller clips — a scale choice, not a curve); true ⇒ height = log2(1+d)/log2(1+dmax). AA fractional top row in both. `stop_bins` are OVERSAMPLED column indices whose whole vertical line XORs 0x80 per channel byte BEFORE the box-average — a stop hairline at fractional-pixel weight, self-contrasting on bars and background alike.
pub fn render_hist(dens: &[[f32; 3]], hw: usize, hh: usize, stop_bins: &[usize], y_log: bool) -> Vec<u32> {
    let bins = dens.len();
    let heights: Vec<[f32; 3]> = if y_log {
        let dmax = dens.iter().flat_map(|b| b.iter().copied()).fold(0f32, f32::max).max(1.);
        let inv = 1. / (1. + dmax).log2();
        dens.iter()
            .map(|b| [0, 1, 2].map(|ch| ((1. + b[ch].max(0.)).log2() * inv * hh as f32).min(hh as f32 - 1.)))
            .collect()
    } else {
        let mut nonzero: Vec<f32> = dens.iter().flat_map(|b| b.iter().copied()).filter(|&c| c > 0.).collect();
        let scale = if nonzero.is_empty() {
            0.
        } else {
            nonzero.sort_unstable_by(f32::total_cmp);
            HIST_SCALE / nonzero[(0.875 * nonzero.len() as f32) as usize].max(f32::MIN_POSITIVE)
        };
        dens.iter()
            .map(|b| [0, 1, 2].map(|ch| ((b[ch].max(0.) * scale) * hh as f32).min(hh as f32 - 1.)))
            .collect()
    };
    let mut stop_mask = vec![false; bins];
    for &b in stop_bins {
        if b < bins {
            stop_mask[b] = true;
        }
    }
    let mut out = vec![0u32; hw * hh];
    for row in 0..hh {
        let fb = hh - 1 - row;
        for col in 0..hw {
            let mut acc = [0u32; 3];
            for os in 0..HIST_OVERSAMPLE {
                let bi = col * HIST_OVERSAMPLE + os;
                for ch in 0..3 {
                    let n = heights[bi][ch];
                    let mut byte = if fb < n as usize {
                        255u32
                    } else if fb == n as usize {
                        (n.fract() * 256.) as u32
                    } else {
                        0
                    };
                    if stop_mask[bi] {
                        byte ^= 0x80;
                    }
                    acc[ch] += byte;
                }
            }
            let b = |ch: usize| acc[ch] / HIST_OVERSAMPLE as u32;
            out[row * hw + col] = 0xFF000000 | ((255 - b(0)) << 16) | ((255 - b(1)) << 8) | (255 - b(2));
        }
    }
    out
}

/// Render the chart at exactly `w × h` from a density grid of the same dims: triangle tint, then the overlays in oriel's order — line of purples, D65 dot, neutral crosshair, Planckian arc, spectral locus lobe. The arcs are coloured by their own light: each vertex's LMS runs thru the observer's bridge and peak-normalizes to the actual display colour of that temperature / wavelength. Called per frame; at panel sizes the whole render is ~a millisecond.
pub fn render_chart(density: &[u32], w: usize, h: usize, observer: &Observer) -> Vec<u32> {
    use rayon::prelude::*;
    // max(1) is the no-density path, not defense: a blank grid must read d/dmax = 0 so the triangle renders flat at DENSITY_FLOOR (the drop-target state still shows the full chart).
    let dmax = density.iter().copied().max().unwrap_or(0).max(1) as f32;

    // Density-brightened tint ramps under the triangle mask (1px AA at the edges) — oriel's background verbatim: red grows rightward, green upward, blue toward bottom-left.
    let mut buf = vec![0u32; w * h];
    buf.par_chunks_mut(w).enumerate().for_each(|(hy, row)| {
        for (wx, px) in row.iter_mut().enumerate() {
            let triangle = (hy as f32 / h as f32 * w as f32 / 2. - (wx as f32 - w as f32 / 2.).abs()).clamp(0., 1.);
            if triangle <= 0. {
                continue;
            }
            let v = ((density[hy * w + wx] as f32 / dmax).powf(1. / DENSITY_GAMMA) * (1. - DENSITY_FLOOR) + DENSITY_FLOOR) / w as f32;
            *px = pack(
                v * wx as f32 * triangle * 256.,
                v * (w - hy - 1) as f32 * triangle * 256.,
                v * (hy as f32 - wx as f32).max(0.) * triangle * 256.,
                triangle * 255.,
            );
        }
    });

    // Spectral locus vertices: project every sample, remember its own colour. First/last also anchor the line of purples.
    let spec = observer.spectrum;
    let n = spec.num_samples();
    let mut locus: Vec<((f32, f32), [f32; 3])> = Vec::with_capacity(n);
    for i in 0..n {
        let d = &spec.data[i * 3..i * 3 + 3];
        let rgb = mat3(&observer.lms2rgb, [d[0], d[1], d[2]]);
        if let Some(pt) = project(rgb[0], rgb[1], rgb[2], w, h) {
            locus.push((pt, peak_norm(rgb)));
        }
    }

    // Line of purples: violet end → red end, gradient blue→red (oriel's endpoint colours).
    if let (Some(&(first, _)), Some(&(last, _))) = (locus.first(), locus.last()) {
        draw_gradient_line(&mut buf, w, h, first.0, first.1, last.0, last.1, [0., 0., 1.], [1., 0., 0.]);
    }

    // D65 dot — soft white disc, oriel's size/softness at this chart's scale.
    let d65_rgb = mat3(&observer.lms2rgb, [D65_LM[0], D65_LM[1], 1.]);
    if let Some((dx, dy)) = project(d65_rgb[0], d65_rgb[1], d65_rgb[2], w, h) {
        let scale = w as f32 / ORIEL_PLOT_W;
        let radius = 4.216491804 * scale;
        let softness = 0.0751 / (scale * scale);
        let r_sq = radius * radius;
        for py in ((dy - radius).max(0.) as usize)..=(((dy + radius) as usize).min(h.saturating_sub(1))) {
            for px in ((dx - radius).max(0.) as usize)..=(((dx + radius) as usize).min(w.saturating_sub(1))) {
                let dist_sq = (px as f32 - dx).powi(2) + (py as f32 - dy).powi(2);
                if dist_sq <= r_sq {
                    let a = ((r_sq - dist_sq) * softness).clamp(0., 1.);
                    let idx = py * w + px;
                    let (oa, old) = unpack(buf[idx]);
                    buf[idx] = pack(
                        old[0] * (1. - a) + 256. * a,
                        old[1] * (1. - a) + 256. * a,
                        old[2] * (1. - a) + 256. * a,
                        oa * (1. - a) + 255. * a,
                    );
                }
            }
        }
    }

    // Output-neutral crosshair: white cross at the display's equal-energy point (centre-x, g = 1/3), arm length and stroke at oriel's proportions.
    let cx = w / 2;
    let cy = 2 * h / 3;
    let arm = ((7. * w as f32 / ORIEL_PLOT_W) as isize).max(3);
    let stroke = ((w as f32 / ORIEL_PLOT_W).round() as isize).max(1);
    for d in -arm..=arm {
        for s in 0..stroke {
            let (px, py) = (cx as isize + d, cy as isize + s - stroke / 2);
            if px >= 0 && (px as usize) < w && py >= 0 && (py as usize) < h {
                buf[py as usize * w + px as usize] = pack(255., 255., 255., 255.);
            }
            let (px, py) = (cx as isize + s - stroke / 2, cy as isize + d);
            if px >= 0 && (px as usize) < w && py >= 0 && (py as usize) < h {
                buf[py as usize * w + px as usize] = pack(255., 255., 255., 255.);
            }
        }
    }

    // Blackbody arc — Planck's law (c2 = hc/k = 1.4388e-2 m·K) integrated against the observer's own curves, 1000K → 20000K log-spaced, each segment wearing its temperature's colour.
    let mut prev: Option<((f32, f32), [f32; 3])> = None;
    for step in 0..64 {
        let t = 1000. * (20.0f32).powf(step as f32 / 63.);
        let mut lms = [0f32; 3];
        for i in 0..n {
            let wl_m = spec.wavelength_at(i) * 1e-9;
            let planck = 1. / (wl_m.powi(5) * ((1.4388e-2 / (wl_m * t)).exp() - 1.));
            let d = &spec.data[i * 3..i * 3 + 3];
            for ch in 0..3 {
                lms[ch] += planck * d[ch];
            }
        }
        let rgb = mat3(&observer.lms2rgb, lms);
        if let Some(pt) = project(rgb[0], rgb[1], rgb[2], w, h) {
            let col = peak_norm(rgb);
            if let Some((p, pc)) = prev {
                draw_gradient_line(&mut buf, w, h, p.0, p.1, pt.0, pt.1, pc, col);
            }
            prev = Some((pt, col));
        }
    }

    // The locus lobe itself, topmost — every segment in its own spectral colour.
    for pair in locus.windows(2) {
        let ((p0, c0), (p1, c1)) = (pair[0], pair[1]);
        draw_gradient_line(&mut buf, w, h, p0.0, p0.1, p1.0, p1.1, c0, c1);
    }

    buf
}

impl PanelTools {
    /// Assemble the panel for an image: the thumbnail from the display pixels.
    pub fn new(pixels: &[u32], w: usize, h: usize) -> Self {
        let (thumb, thumb_w, thumb_h) = thumb_from(pixels, w, h);
        Self { thumb, thumb_w, thumb_h }
    }

    /// Re-sample the navigator thumbnail from (re-encoded) display pixels — cheap, so it tracks exposure changes live. The histogram describes the capture, not the view, and stays put.
    pub fn refresh_thumb(&mut self, pixels: &[u32], w: usize, h: usize) {
        if self.thumb_w == 0 || self.thumb_h == 0 || w == 0 || h == 0 {
            return;
        }
        for ty in 0..self.thumb_h {
            let sy = ty * h / self.thumb_h;
            for tx in 0..self.thumb_w {
                self.thumb[ty * self.thumb_w + tx] = pixels[sy * w + tx * w / self.thumb_w];
            }
        }
    }
}
