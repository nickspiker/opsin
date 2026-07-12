//! Right tool panel: navigator, histogram, chromaticity chart. Layout is a fraction of window width (RU-coherent, user-draggable at the divider); sections stack vertically.
//!
//! Everything observer-dependent is derived **on the fly** from an [`Observer`] — cone fundamentals + RGB↔LMS matrices — never baked coordinates, so swapping observers (CVRL 2006, film stocks, tetrachromat...) is constructing a different `Observer` and rebuilding. The chart is MacLeod–Boynton-style (l = L/(L+M), s = S/(L+M)): the spectral locus and Planckian arc are integrated from the observer's own curves, and the lobe interior is tinted by each bin's chromaticity (oriel's approach) with the image's density brightening it under a 1/5-power compression + 1/8 floor, so the whole lobe reads even where the image is empty.
//!
//! Image chromaticities map linear camera RGB thru the observer's `rgb2lms`, an approximation until per-image spectral characterization drives a real resolve — the cloud shows distribution shape, not colorimetric truth.

use vsf::colour::spectrum::ConstSpectrum;
use vsf::colour::{LMS2VSF_RGB, LMS_2000_10DEG_1NM, VSF_RGB2LMS};

/// Chart-space resolution for the chromaticity lobe (bins per axis). A data-grid resolution, not a display size — the render blits it nearest to whatever the panel gives it.
pub const CHART_RES: usize = 1 << 8;
/// Histogram bin count — oversampled relative to any plausible panel width; the render does a peak-preserving downsample per column (oriel's oversample idea, exact-integer instead of dithered since u16 in → bin is exact).
pub const HIST_BINS: usize = 1 << 10;
/// Density display: brightness = FLOOR + (1−FLOOR)·(d/dmax)^(1/GAMMA) — oriel's gamut-plot constants.
const DENSITY_FLOOR: f32 = 1. / 8.;
const DENSITY_GAMMA: f32 = 5.;

/// The observer: cone fundamentals + the RGB↔LMS bridge. Everything the panel draws in chromaticity space derives from this at build time — swap the observer, rebuild, done.
pub struct Observer {
    pub spectrum: &'static ConstSpectrum,
    pub rgb2lms: [f32; 9],
    pub lms2rgb: [f32; 9],
}

impl Observer {
    /// Stockman & Sharpe 2000 10° with the VSF-RGB bridge — the stock human.
    pub fn stock() -> Self {
        Self { spectrum: &LMS_2000_10DEG_1NM, rgb2lms: VSF_RGB2LMS, lms2rgb: LMS2VSF_RGB }
    }
}

/// Everything the panel needs, precomputed once per image (or observer swap). Render-time work is pure blitting.
pub struct PanelTools {
    /// Navigator thumbnail, α+darkness packed, nearest-sampled from the display pixels.
    pub thumb: Vec<u32>,
    pub thumb_w: usize,
    pub thumb_h: usize,
    /// Per-channel histogram, HIST_BINS bins, log-scaled and normalized to 0..1.
    pub hist: Vec<[f32; 3]>,
    /// Chromaticity lobe, CHART_RES × CHART_RES darkness-packed pixels: tint × density brightness inside the locus, 0 (transparent) outside. Row 0 = top (s max).
    pub chart: Vec<u32>,
    /// Spectral locus polyline in chart space (0..1, y down).
    pub locus: Vec<(f32, f32)>,
    /// Blackbody (Planckian) arc in chart space, 1000K → 20000K.
    pub blackbody: Vec<(f32, f32)>,
}

/// Chart-space mapping shared by locus / blackbody / scatter / tint so they land in one coordinate system.
struct ChartMap {
    l_min: f32,
    l_max: f32,
    s_max: f32,
}

impl ChartMap {
    /// Pure linear map, unbounded — callers own their domain. Locus and Planck arc are inside by construction (the bounds were derived FROM the locus); the image scatter drops out-of-chart samples instead of pinning them.
    fn to_chart(&self, l: f32, s: f32) -> (f32, f32) {
        ((l - self.l_min) / (self.l_max - self.l_min), 1. - s / self.s_max)
    }
    /// Inverse: chart bin coords (0..1, y down) → (l, s).
    fn from_chart(&self, x: f32, y: f32) -> (f32, f32) {
        (self.l_min + x * (self.l_max - self.l_min), (1. - y) * self.s_max)
    }
}

/// (l, s) from cone responses; None when the sample carries no L+M energy.
fn mb(lms: [f32; 3]) -> Option<(f32, f32)> {
    let lm = lms[0] + lms[1];
    if lm <= 0. || !lm.is_finite() {
        return None;
    }
    Some((lms[0] / lm, lms[2] / lm))
}

/// Even-odd point-in-polygon over a closed polyline.
fn inside(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    let mut hit = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            hit = !hit;
        }
        j = i;
    }
    hit
}

impl PanelTools {
    /// `pixels` = display α+darkness buffer (navigator source), `lin` = linear SIGNED interleaved RGB, white at 65535, out-of-range preserved (histogram + chromaticity source), both `w × h`. Pass `w = h = 0` for the empty drop-target state — image-derived parts stay blank; the lobe, locus and Planck arc still render from the observer.
    pub fn build(pixels: &[u32], lin: &[i32], w: usize, h: usize, observer: &Observer) -> Self {
        // Navigator thumbnail — nearest sample, long axis capped at 512 (source-data cap, not a display size).
        let (thumb, thumb_w, thumb_h) = if w == 0 || h == 0 {
            (Vec::new(), 0, 0)
        } else {
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
        };

        // Histogram — HIST_BINS per channel, exact integer binning (v >> 6 for 1024 bins), log-compressed. The empty drop-target state is a blank histogram BY DEFINITION — handled as a case, not smuggled thru a defensive peak floor: any real image puts at least one count in some bin, so peak ≥ 1 and the normalisation needs no guard.
        let hist: Vec<[f32; 3]> = if lin.is_empty() {
            vec![[0f32; 3]; HIST_BINS]
        } else {
            let mut counts = vec![[0u32; 3]; HIST_BINS];
            for px in lin.chunks_exact(3) {
                for ch in 0..3 {
                    // The signed pipe puts real values past both display ends; the END BINS ARE OPEN INTERVALS by design — the spike at bin 0 / bin max is the standard clipping indicator (how much of the frame sits at-or-below black, at-or-beyond white). The clamp is that binning semantic, not defense.
                    counts[(px[ch].clamp(0, 65535) as usize * HIST_BINS) >> 16][ch] += 1;
                }
            }
            let peak = counts.iter().flat_map(|b| b.iter()).copied().max().unwrap() as f32;
            let log_peak = (1. + peak).ln();
            counts.iter().map(|b| [0, 1, 2].map(|ch| (1. + b[ch] as f32).ln() / log_peak)).collect()
        };

        // Spectral locus from the observer's cone fundamentals — first pass finds the chart bounds, second emits chart-space points.
        let spec = observer.spectrum;
        let n = spec.num_samples();
        let mut raw_locus: Vec<(f32, f32)> = Vec::with_capacity(n);
        for i in 0..n {
            let d = &spec.data[i * 3..i * 3 + 3];
            if let Some(p) = mb([d[0], d[1], d[2]]) {
                raw_locus.push(p);
            }
        }
        let l_min = raw_locus.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
        let l_max = raw_locus.iter().map(|p| p.0).fold(0.0f32, f32::max);
        let s_max = raw_locus.iter().map(|p| p.1).fold(0.0f32, f32::max);
        let map = ChartMap { l_min: l_min - 0.02, l_max: l_max + 0.02, s_max: s_max * 1.04 };
        let locus: Vec<(f32, f32)> = raw_locus.iter().map(|&(l, s)| map.to_chart(l, s)).collect();

        // Blackbody arc — Planck's law (c2 = hc/k = 1.4388e-2 m·K) integrated against the same curves, 1000K → 20000K log-spaced.
        let mut blackbody = Vec::with_capacity(64);
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
            if let Some((l, s)) = mb(lms) {
                blackbody.push(map.to_chart(l, s));
            }
        }

        // Image chromaticity density — subsample to ~200k pixels, camera RGB → LMS thru the observer's bridge, accumulate a CHART_RES² grid.
        let total = w * h;
        let stride = (total >> 18).max(1);
        let m = &observer.rgb2lms;
        let mut grid = vec![0u32; CHART_RES * CHART_RES];
        for pi in (0..total).step_by(stride) {
            let r = lin[pi * 3] as f32;
            let g = lin[pi * 3 + 1] as f32;
            let b = lin[pi * 3 + 2] as f32;
            let lms = [
                m[0] * r + m[3] * g + m[6] * b,
                m[1] * r + m[4] * g + m[7] * b,
                m[2] * r + m[5] * g + m[8] * b,
            ];
            if let Some((l, s)) = mb(lms) {
                let (x, y) = map.to_chart(l, s);
                // Out-of-chart samples (camera space can push lms past the locus bounds) are DROPPED, not pinned — pinning fabricated density ridges along the chart border. The range test proves the bin indices in-bounds, so they carry no re-check.
                if x >= 0. && x <= 1. && y >= 0. && y <= 1. {
                    let gx = (x * (CHART_RES - 1) as f32) as usize;
                    let gy = (y * (CHART_RES - 1) as f32) as usize;
                    grid[gy * CHART_RES + gx] += 1;
                }
            }
        }
        // max(1) justified — it is the no-density path, not defense: with no image (drop-target state) or every sample out-of-chart, the grid is all zero and d/dmax must read 0 so the lobe renders flat at DENSITY_FLOOR (the documented "lobe, locus and arc still render" behavior).
        let dmax = grid.iter().copied().max().unwrap().max(1) as f32;

        // The lobe: per bin, inside-locus test (decimated polygon — the curve is smooth, every 4th vertex is plenty for a 192² grid), chromaticity tint from the observer's lms→rgb bridge, density-driven brightness.
        let mut pip: Vec<(f32, f32)> = locus.iter().copied().step_by(4).collect();
        pip.push(locus[locus.len() - 1]);
        let lm2rgb = &observer.lms2rgb;
        let mut chart = vec![0u32; CHART_RES * CHART_RES];
        for gy in 0..CHART_RES {
            for gx in 0..CHART_RES {
                let cx = gx as f32 / (CHART_RES - 1) as f32;
                let cy = gy as f32 / (CHART_RES - 1) as f32;
                if !inside(&pip, cx, cy) {
                    continue;
                }
                let (l, s) = map.from_chart(cx, cy);
                // Chromaticity → a representative colour: fix L+M = 1 so (l, s) IS the cone vector, run it thru the observer's bridge, clamp out-of-gamut negatives, normalise peak to 1.
                let lms = [l, 1. - l, s];
                let mut rgb = [0f32; 3];
                for o in 0..3 {
                    rgb[o] = (lm2rgb[o * 3] * lms[0] + lm2rgb[o * 3 + 1] * lms[1] + lm2rgb[o * 3 + 2] * lms[2]).max(0.);
                }
                let peak = rgb[0].max(rgb[1]).max(rgb[2]);
                if peak <= 0. {
                    continue;
                }
                let d = grid[gy * CHART_RES + gx] as f32;
                let brightness = DENSITY_FLOOR + (1. - DENSITY_FLOOR) * (d / dmax).powf(1. / DENSITY_GAMMA);
                let to_byte = |v: f32| (v / peak * brightness * 255.) as u32;
                let (r8, g8, b8) = (to_byte(rgb[0]), to_byte(rgb[1]), to_byte(rgb[2]));
                chart[gy * CHART_RES + gx] = 0xFF000000 | ((255 - r8) << 16) | ((255 - g8) << 8) | (255 - b8);
            }
        }

        Self { thumb, thumb_w, thumb_h, hist, chart, locus, blackbody }
    }

    /// Re-sample the navigator thumbnail from (re-encoded) display pixels — cheap, so it tracks exposure changes live. Everything else in the panel describes the capture, not the view, and stays put.
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
