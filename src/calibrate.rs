//! Target-scan calibration thru chameleon — behind the `calibrate` feature, because chameleon is closed source and opsin is not: without the feature there is no Cal pill, no C key, and no chameleon in the build; everything else in opsin is identical.
//!
//! One scan of a VERICHROME target characterizes the sensor: `chameleon::scan_target` finds the target in the frame, solves the camera's DSR IDT, and hands back `magic9inv` — the nine SRATIONALs a DNG stores as ColorMatrix1 — plus the live overlay (the solved patch grid, warped back onto the frame so you can SEE the fit) and a `CalInfo` readout (gamma, IR leak, UV, target life, warnings). opsin runs the scan on a background thread from the file on disk (the scan mutates its input, and the retained decode must stay pristine), then on the UI thread pastes the nine numbers into the DNG in place with the same machinery as Ctrl+V (fingerprint from the file itself), reloads, and draws the overlay through the normal display transform so it tracks pan, zoom, EV and orientation with the image. The overlay is in RAW mosaic pixel coordinates (chameleon's "doubled scan-input space"), camera-linear with white = 1; it goes thru the scan's own `cam2terminal9` (normalized to Σ = 3, as chameleon's SCAN path does) into display linear.

use std::path::Path;

/// The solved overlay: origin + size in RAW pixel coordinates (pre-orientation, full mosaic resolution), RGBA f32 in display-linear Rec.2020 (white = 1), alpha as chameleon rendered it.
pub struct Overlay {
    pub x0: usize,
    pub y0: usize,
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<f32>,
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
    // Camera → terminal for the overlay colours, brightness-normalized exactly as chameleon's SCAN path does.
    let mut m = ri.cam2terminal9;
    let total: f32 = m.iter().sum();
    if total.abs() > 1e-6 {
        for v in &mut m {
            *v *= 3. / total;
        }
    }
    let overlay = live.map(|lo| {
        let (x0, y0, w, h, mut rgba) = lo.warped();
        for px in rgba.chunks_exact_mut(4) {
            let (r, g, b) = (px[0], px[1], px[2]);
            px[0] = m[0] * r + m[1] * g + m[2] * b;
            px[1] = m[3] * r + m[4] * g + m[5] * b;
            px[2] = m[6] * r + m[7] * g + m[8] * b;
        }
        Overlay { x0, y0, w, h, rgba }
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
