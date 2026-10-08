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
        let (x0, y0, w, h, rgba) = lo.warped();
        let (_, _, _, _, cov) = lo.warped_logo();
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
    Ok(ScanOutcome { matrix, overlay, readout, report: strip_ansi(&text) })
}

/// chameleon's display light (linear, 0..1) → the encode's 0..65535 units.
pub fn to_light(c: [f32; 3]) -> [u16; 3] {
    c.map(|v| (v.clamp(0., 1.) * 65535.).round() as u16)
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
