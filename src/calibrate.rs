//! Target-scan calibration thru chameleon — behind the `calibrate` feature, because chameleon is closed source and opsin is not: without the feature there is no Cal pill, no C key, and no chameleon in the build; everything else in opsin is identical.
//!
//! One scan of a VERICHROME target characterizes the sensor: `chameleon::scan_target` finds the target in the frame, solves the camera's DSR IDT, and hands back `magic9inv` — the nine SRATIONALs a DNG stores as ColorMatrix1 — plus the live overlay (the solved patch grid, warped back onto the frame so you can SEE the fit) and a `CalInfo` readout (gamma, IR leak, UV, target life, warnings). opsin runs the scan on a background thread from the file on disk (the scan mutates its input, and the retained decode must stay pristine), then on the UI thread pastes the nine numbers into the DNG in place with the same machinery as Ctrl+V (fingerprint from the file itself), reloads, and draws the overlay through the normal display transform so it tracks pan, zoom, EV and orientation with the image. The overlay is in RAW mosaic pixel coordinates (chameleon's "doubled scan-input space") and raw camera counts above black; the view takes it to display linear through the same per-channel range + display matrix the capture's pixels get, so the two meet on the same tone.

use std::path::Path;

/// The solved overlay: origin + size in RAW pixel coordinates (pre-orientation, full mosaic resolution), RGBA f32 in CAMERA counts above black (chameleon scales it to the scanned white patch's raw level), alpha as chameleon rendered it. Draw it through `convert::camera_to_display` — the capture's own transform.
pub struct Overlay {
    pub x0: usize,
    pub y0: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<f32>,
    /// The logo's display light, pixel for pixel with `rgba` (same origin and size): white wordmark and illuminant-E rail in display-linear Rec.2020, 0..65535, each normalized on its own. The view ADDS it after exposure and rolloff — see [`crate::view::Emissive`]. `rgba` paints that cell true black underneath.
    pub light: Vec<[u16; 3]>,
}

pub struct ScanOutcome {
    /// DNG ColorMatrix1 (XYZ → camera) as verbatim SRATIONALs, straight from chameleon's `magic9inv`.
    pub matrix: [(i32, i32); 9],
    pub overlay: Option<Overlay>,
    /// One HUD line: gamma, IR, UV, target life, and any warning.
    pub readout: String,
    /// chameleon's full report, ANSI stripped, for stdout.
    pub report: String,
    /// The observer the solve targeted, as the settings describe it (e.g. "CIE/CVRL 2006 2°") — into the pasted IDT's provenance.
    pub observer: String,
    /// The target that produced the IDT: (type, serial, its calibration timestamp).
    pub cal: Option<(u32, u64, String)>,
    /// The camera's spectral response under the scan's light, when the target carried spectra — what the nine numbers were derived from, and what a relight needs. Not solved by [`scan`] itself: see `pending_response`.
    pub response: Option<crate::idt::IdtResponse>,
    /// The spectral solve, still to run — it is most of a scan's time (~1.5 s against ~0.7 s for finding the target and solving the matrix), so the view pastes the matrix first and re-pastes with the curves when this comes back. `None` when the target carries no spectra.
    pub pending_response: Option<PendingResponse>,
}

/// A deferred spectral-response solve: the scan's inputs and the settings it ran under, movable to a worker thread.
pub struct PendingResponse {
    input: chameleon::spectral::SpectralInput,
    settings: chameleon::CurrentSettings,
}

impl PendingResponse {
    /// Blocking — call from a thread.
    pub fn solve(self) -> Option<crate::idt::IdtResponse> {
        self.input.encode(&self.settings).map(|r| crate::idt::IdtResponse { start_nm: r.start_nm, step_nm: r.step_nm, channels: r.channels, prior: r.prior, light: Some(r.light) })
    }
}

/// Scan `path` for a target. Blocking — call from a thread. `Err` carries chameleon's reason (no target found, overexposed, no settings file…).
pub fn scan(path: &Path) -> Result<ScanOutcome, String> {
    let (mut img, mut ri) = chameleon::read_raw_fallback(path, false).ok_or_else(|| format!("{}: chameleon could not read the frame", path.display()))?;
    let (ok, settings) = chameleon::get_settings();
    if !ok {
        return Err("chameleon settings unavailable (~/.config/Verichrome/settings.cfg)".to_string());
    }
    // Persist the scan (scan.vsf) so the matrix can be re-solved under different settings later without re-scanning.
    ri.save_scan = true;
    // The spectral response comes back as `pending_response`, solved after the matrix is already in the file.
    chameleon::set_defer_spectral(true);
    let r = chameleon::scan_target(&mut chameleon::ImageData::U16Data(&mut img), &mut ri, &settings, true);
    let Some((_, _, _, report, warning, live, cal)) = r else {
        let why = chameleon::get_last_scan_error().unwrap_or_else(|| "no target found".to_string());
        return Err(format!("scan rejected: {why}"));
    };
    let mut matrix = [(0, 0); 9];
    for (i, c) in ri.magic9inv.chunks(8).enumerate() {
        matrix[i] = (i32::from_le_bytes(c[0..4].try_into().unwrap()), i32::from_le_bytes(c[4..8].try_into().unwrap()));
    }
    if matrix.iter().any(|&(_, d)| d == 0) {
        return Err("scan produced a degenerate matrix".to_string());
    }
    // Camera counts above black, untouched: chameleon scales the overlay to the scanned white patch's raw level (so lumis can composite it raw-for-raw before its display matrix), and opsin does the same — the view runs it through `convert::camera_to_display`, the transform the capture itself gets. Pushing it through cam2terminal9 instead left it in raw counts, ~1700× over display white (Nick 2026-10-07: "overlay is still too bright").
    let rail = chameleon::logo::rail_display(&settings);
    let overlay = live.map(|lo| {
        let (x0, y0, w, h) = (lo.min_x, lo.min_y, lo.width, lo.height);
        let (rgba, cov) = rayon::join(|| mesh_warp(&lo, &lo.flat, lo.cross_colour), || mesh_warp(&lo, &lo.logo, [0.; 3]));
        // Normalized over the raster as drawn (raw-coordinate resolution): text and rail each to their own white.
        let light = chameleon::logo::light_layer(&cov, &rail).into_iter().map(to_light).collect();
        Overlay { x0, y0, w, h, rgba, light }
    });
    let readout = match &cal {
        Some(c) => format!(
            "cal: gamma {:.2}  IR {:+.1}% {:+.1}% {:+.1}%  UV {:.1}%  target #{} life {:.0}%{}",
            c.gamma, c.ir[0] * 100., c.ir[1] * 100., c.ir[2] * 100., c.uv * 100., c.serial, c.life * 100.,
            if c.warning.is_empty() { String::new() } else { format!("  ⚠ {}", c.warning.trim()) }
        ),
        None => "cal: scanned".to_string(),
    };
    let mut text: String = report.iter().map(|s| s.to_string()).collect();
    if !warning.is_empty() {
        text.push_str(&warning);
    }
    let target = cal.as_ref().map(|c| (c.target_type, c.serial, c.timestamp.clone()));
    let response = cal.as_ref().and_then(|c| c.spectral.as_ref()).map(|r| crate::idt::IdtResponse { start_nm: r.start_nm, step_nm: r.step_nm, channels: r.channels.clone(), prior: r.prior.clone(), light: Some(r.light) });
    let observer = settings.cmfdescription.clone();
    let pending_response = if response.is_none() { cal.and_then(|c| c.spectral_input).map(|input| PendingResponse { input, settings }) } else { None };
    Ok(ScanOutcome { matrix, overlay, readout, report: strip_ansi(&text), observer, cal: target, response, pending_response })
}

/// One of chameleon's flat overlay rasters (`lo.flat`, or `lo.logo` — both `flat_w × flat_h` RGBA, 7×6 patch cells of `livepatchdim`) placed on the frame thru the scan's own 8×7 mesh: `lo.corners` are the grid nodes composed thru the same refish → deorient → de_transform chain `LiveOverlay::warped` runs per pixel (chameleon's `overlay_corners`), so each patch cell is the quad between its four nodes. Every output pixel is GATHERED — inverse-bilinear into its cell, bilinear taps into the flat raster, 2×2 subsamples — at output resolution, on every core. `warped()` instead splatted all 44 M flat pixels (a 24 MP frame's overlay draws patches 1031 px across) thru a 7733² float intermediate, twice per scan with the logo: ~3.5 s and ~2 GB each, for a 2728×2312 result (2026-10-20). Inside a cell the quad is bilinear where `de_transform` was exactly projective; the cells are a seventh of the target across, so the two agree to a fraction of a pixel. The four bullseye crosses are painted on top in `cross`, alpha 1, as `de_transform` paints them. Returns `lo.width × lo.height` RGBA in `warped()`'s frame (origin `lo.min_x/min_y`).
pub fn mesh_warp(lo: &chameleon::LiveOverlay, layer: &[f32], cross: [f32; 3]) -> Vec<f32> {
    use rayon::prelude::*;
    let (w, h) = (lo.width, lo.height);
    let (fw, fh) = (lo.flat_w, lo.flat_h);
    let pd = lo.livepatchdim as f64;
    let mut out = vec![0f32; w * h * 4];
    if w == 0 || h == 0 || fw == 0 || fh == 0 || layer.len() < fw * fh * 4 {
        return out;
    }
    struct Cell {
        q: [[f64; 2]; 4], // node (cx,cy), (cx+1,cy), (cx+1,cy+1), (cx,cy+1)
        cx: f64,
        cy: f64,
        x0: usize,
        x1: usize,
        y0: usize,
        y1: usize,
    }
    let node = |cx: usize, cy: usize| {
        let c = lo.corners[cy * 8 + cx];
        [c[0] as f64, c[1] as f64]
    };
    let mut cells = Vec::with_capacity(42);
    for cy in 0..6 {
        for cx in 0..7 {
            let q = [node(cx, cy), node(cx + 1, cy), node(cx + 1, cy + 1), node(cx, cy + 1)];
            let (mut lx, mut hx, mut ly, mut hy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for p in &q {
                lx = lx.min(p[0]);
                hx = hx.max(p[0]);
                ly = ly.min(p[1]);
                hy = hy.max(p[1]);
            }
            if !(lx.is_finite() && hx.is_finite() && ly.is_finite() && hy.is_finite()) || hx < 0. || hy < 0. || lx > (w - 1) as f64 || ly > (h - 1) as f64 {
                continue;
            }
            // WHY/PROOF: the mesh nodes are where chameleon found the target in the camera frame and run past its edges — an unclamped bound indexes off the output rows.
            let clamp = |v: f64, n: usize| v.clamp(0., (n - 1) as f64) as usize;
            cells.push(Cell { q, cx: cx as f64, cy: cy as f64, x0: clamp(lx.floor(), w), x1: clamp(hx.ceil(), w), y0: clamp(ly.floor(), h), y1: clamp(hy.ceil(), h) });
        }
    }
    let tap = |fx: f64, fy: f64| -> [f32; 4] {
        // WHY/PROOF: u and v reach 1 inclusive, so the far cell edge lands on or past the flat raster's last column or row — chameleon sizes that raster, and an unclamped tap reads past it.
        let fx = fx.clamp(0., (fw - 1) as f64);
        let fy = fy.clamp(0., (fh - 1) as f64);
        let (ix, iy) = (fx as usize, fy as usize);
        // WHY/PROOF: at the raster's last column or row the bilinear neighbour is one past the end.
        let (ix1, iy1) = ((ix + 1).min(fw - 1), (iy + 1).min(fh - 1));
        let (tx, ty) = ((fx - ix as f64) as f32, (fy - iy as f64) as f32);
        let px = |x: usize, y: usize, c: usize| layer[(y * fw + x) * 4 + c];
        std::array::from_fn(|c| {
            let top = px(ix, iy, c) * (1. - tx) + px(ix1, iy, c) * tx;
            let bot = px(ix, iy1, c) * (1. - tx) + px(ix1, iy1, c) * tx;
            top * (1. - ty) + bot * ty
        })
    };
    // Output pixel i sits AT position i (de_transform splats a source at x into floor(x) with weight 1 − frac), so the subsamples straddle it.
    const SUB: [(f64, f64); 4] = [(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)];
    out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let mut hits = vec![0u8; w];
        for cell in cells.iter().filter(|c| y >= c.y0 && y <= c.y1) {
            for x in cell.x0..=cell.x1 {
                for (ox, oy) in SUB {
                    let Some((u, v)) = inverse_bilinear(&cell.q, [x as f64 + ox, y as f64 + oy]) else { continue };
                    let s = tap((cell.cx + u) * pd, (cell.cy + v) * pd);
                    for c in 0..4 {
                        row[x * 4 + c] += s[c];
                    }
                    hits[x] += 1;
                }
            }
        }
        // Colour is the mean over the subsamples that landed in the overlay (as the splat normalizes by its weights); alpha also carries how much of the pixel the overlay covers, so its outer edge is antialiased.
        for x in 0..w {
            // WHY/PROOF: neighbouring cells share their edge, so a seam pixel collects up to 8 subsample hits — coverage past 4 would push alpha past 1, and live's overlay lerp extrapolates with it.
            let n = hits[x].min(4);
            if hits[x] > 0 {
                let k = 1. / hits[x] as f32;
                for c in 0..3 {
                    row[x * 4 + c] *= k;
                }
                row[x * 4 + 3] *= k * n as f32 / 4.;
            }
        }
    });
    // The bullseye crosses, exactly where and how de_transform paints them (bullseye centres are in the uncropped doubled space: crop, then de_transform's +1).
    let size = lo.bullseyesize;
    let (weight, arm) = ((size / 87.) as usize + 1, (size / 5.) as usize + 1);
    for i in 0..4 {
        // WHY/PROOF: the bullseye centres are where the scan found them, and one can sit left of or above the overlay's crop origin — the subtraction underflows.
        let (Some(bx), Some(by)) = (lo.bullseyesw[i].checked_sub(lo.min_x), lo.bullseyesh[i].checked_sub(lo.min_y)) else { continue };
        let (bx, by) = (bx + 1, by + 1);
        for (hw, hh) in [(weight, arm), (arm, weight)] {
            // WHY/PROOF: a bullseye near the overlay's edge puts its arm past 0 — the subtraction underflows into a huge loop.
            for yy in by.saturating_sub(hh)..by + hh {
                for xx in bx.saturating_sub(hw)..bx + hw {
                    if yy < h - 1 && xx < w - 1 {
                        out[(yy * w + xx) * 4..(yy * w + xx) * 4 + 4].copy_from_slice(&[cross[0], cross[1], cross[2], 1.]);
                    }
                }
            }
        }
    }
    out
}

/// (u, v) ∈ [0,1]² with `p = q0 + (q1−q0)u + (q3−q0)v + (q0−q1+q2−q3)uv` — the point's place in the bilinear quad `q` (corners in order (0,0), (1,0), (1,1), (0,1)), or `None` outside it. The closed form: a quadratic in v, then u from whichever axis is better conditioned.
fn inverse_bilinear(q: &[[f64; 2]; 4], p: [f64; 2]) -> Option<(f64, f64)> {
    let sub = |a: [f64; 2], b: [f64; 2]| [a[0] - b[0], a[1] - b[1]];
    let cross = |a: [f64; 2], b: [f64; 2]| a[0] * b[1] - a[1] * b[0];
    let e = sub(q[1], q[0]);
    let f = sub(q[3], q[0]);
    let g = [q[0][0] - q[1][0] + q[2][0] - q[3][0], q[0][1] - q[1][1] + q[2][1] - q[3][1]];
    let hv = sub(p, q[0]);
    let k2 = cross(g, f);
    let k1 = cross(e, f) + cross(hv, g);
    let k0 = cross(hv, e);
    let u_of = |v: f64| {
        let (dx, dy) = (e[0] + g[0] * v, e[1] + g[1] * v);
        if dx.abs() >= dy.abs() { (hv[0] - f[0] * v) / dx } else { (hv[1] - f[1] * v) / dy }
    };
    let inside = |u: f64, v: f64| (0. ..=1.).contains(&u) && (0. ..=1.).contains(&v);
    if k2.abs() <= 1e-9 * k1.abs() {
        if k1 == 0. {
            return None;
        }
        let v = -k0 / k1;
        let u = u_of(v);
        return inside(u, v).then_some((u, v));
    }
    let disc = k1 * k1 - 4. * k0 * k2;
    if disc < 0. {
        return None;
    }
    let r = disc.sqrt();
    for v in [(-k1 - r) / (2. * k2), (-k1 + r) / (2. * k2)] {
        let u = u_of(v);
        if inside(u, v) {
            return Some((u, v));
        }
    }
    None
}

/// chameleon's display light (linear, 0..1) → the encode's 0..65535 units.
pub fn to_light(c: [f32; 3]) -> [u16; 3] {
    c.map(|v| (v * 65535.).round() as u16)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    /// The overlay must land on the capture's own tone. Scan a real target, paste the solve (as the view does), render the frame, and compare every opaque overlay pixel with the capture beneath it in display-linear: the patch wedges — the bulk of the overlay, drawn in reflectance — sit on top of their own patches, so they must agree. Before the fix the overlay arrived in raw counts and ran ~1700× (+10.7 stops) over the frame (Nick 2026-10-07: "overlay is still too bright").
    #[test]
    fn overlay_lands_on_the_capture() {
        let src = std::path::Path::new("/mnt/Harbor/Code/chameleon/Colour.dng");
        if !src.exists() {
            return;
        }
        let dir = std::env::temp_dir().join("opsin-overlay-test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("Colour.dng");
        std::fs::copy(src, &p).unwrap();
        let out = super::scan(&p).expect("scan");
        let ov = out.overlay.expect("overlay");
        crate::idt::IdtClip::copy_from(&p)
            .and_then(|mut clip| {
                clip.matrix = out.matrix;
                clip.illuminant = 23;
                clip.provenance = crate::idt::IdtProvenance::of_scan(&out.observer, out.cal.clone());
                clip.response = out.response.clone();
                clip.paste_into(&p, false)
            })
            .expect("paste");
        let dec = crate::convert::load_any(&p).unwrap();
        let (w, h, lin) = crate::convert::to_linear(&dec).unwrap();
        let gain = dec.baseline_ev.exp2();
        let m = crate::convert::camera_to_display(&dec, crate::convert::Target::Rec2020);
        // The logo rides beside as light: white text exactly at display white somewhere, nothing past it.
        assert_eq!(ov.light.len() * 4, ov.rgba.len());
        assert!(ov.light.iter().any(|c| *c == [65535; 3]), "the wordmark never reaches display white");
        let (mut near, mut over, mut n) = (0usize, 0usize, 0usize);
        for ry in (ov.y0..ov.y0 + ov.h).step_by(2) {
            for rx in (ov.x0..ov.x0 + ov.w).step_by(2) {
                let i = ((ry - ov.y0) * ov.w + (rx - ov.x0)) * 4;
                let (dx, dy) = (rx / 2, ry / 2);
                if ov.rgba[i + 3] < 0.999 || dx >= w || dy >= h {
                    continue;
                }
                let (r, g, b) = (ov.rgba[i], ov.rgba[i + 1], ov.rgba[i + 2]);
                let o: f32 = (0..3).map(|c| m[c * 3] * r + m[c * 3 + 1] * g + m[c * 3 + 2] * b).sum::<f32>() * gain;
                let cap: f32 = (0..3).map(|c| lin[(dy * w + dx) * 3 + c] as f32 / 65535.).sum();
                if cap < 0.02 || o <= 0. {
                    continue;
                }
                let stops = (o / cap).log2();
                n += 1;
                near += (stops.abs() < 1.) as usize;
                over += (stops > 3.) as usize;
            }
        }
        assert!(n > 1000, "too few comparable pixels: {n}");
        // The UI cells' dark grounds over the grey card legitimately sit a few stops under; the wedges must match, and nothing may run blown over the frame.
        assert!(near * 10 > n * 4, "only {near}/{n} overlay pixels within a stop of the capture");
        assert!(over * 100 < n, "{over}/{n} overlay pixels more than 3 stops OVER the capture");
    }
}
