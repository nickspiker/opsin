//! Image loading + VSF conversion, used by the viewer. `load_any` decodes any supported source (VSF-Image passthrough, or camera RAW/DNG via iris) into a [`Decoded`] — a `SpectralImage` carrying its own tiered colour_profile; `to_linear` renders it, deriving the display matrix from that profile and baking the debayer bin + matrix into one integer pass (signed output); `write_vsf` saves the image.
//!
//! Colour, per the VERICHROME IDT taxonomy: the DNG path is an **Absolute IDT** — camera → XYZ → linear **VSF RGB** by straight matrix inversion, illuminant cast preserved, no chromatic adaptation (that's a Creative IDT and not welcome here). The STORED reference is VSF RGB (spectral 703/523/462nm primaries, Illuminant E) — never Rec.2020 or XYZ, which are display/rendering targets resolved at read time. **Relative (DSR)** rendering comes from a chameleon magic-9 when one exists for the source — that entry just gets elected first. The monitor is assumed Rec.2020 primaries / gamma-2 — an assumption that lives at DISPLAY (concatenation `VSF_RGB2REC2020 × stored`), NOT in the stored file. Sources without any matrix render raw-camera.
//!
//! Matrix convention: opsin stays row-major throughout (row = output channel, `m[out*3 + in]`), matching the DNG ColorMatrix spec and this crate's `inv3`/`matmul3`/`build_coefs`. vsf::colour stores the SAME matrices column-major, so its numeric constants are pulled in via the `t3` transpose — one source of truth for the primaries, no convention clash.

use std::path::Path;
use vsf::spectral_image::{self, ColourProfile, IdtClass, PlaneLayout, ProfileEntry, ProfileGrade, Provenance, SpectralChannel, SpectralImage, Transfer};
use vsf::{BitPackedTensor, Tensor};

/// Extensions the viewer will try to open + arrow-navigate. `vsf` is the native container; the rest go through iris (50+ RAW formats — this is a representative common subset, not exhaustive).
pub const SUPPORTED_EXTS: &[&str] = &[
    "vsf", "dng", "arw", "cr2", "cr3", "nef", "nrw", "raf", "rw2", "orf", "pef", "srw", "raw", "tif", "tiff",
];

/// Is `path` a file the viewer can open (by extension)?
pub fn is_supported(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => SUPPORTED_EXTS.contains(&e.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// A decoded image ready to render: the spectral data, carrying its own tiered [`vsf::spectral_image::ColourProfile`] in `img.profile` (`None` ⇒ render raw-camera). The display matrix is derived from the profile at render time — nothing display-space is stored.
pub struct Decoded {
    pub img: SpectralImage,
}

/// Transpose a 3×3 — bridges vsf::colour's column-major storage to opsin's row-major convention. const so the bridged constants are compile-time.
const fn t3(m: [f32; 9]) -> [f32; 9] {
    [m[0], m[3], m[6], m[1], m[4], m[7], m[2], m[5], m[8]]
}

/// CIE XYZ → linear VSF RGB (row-major), from vsf's authoritative constant. The stored characterization target: spectral 703/523/462nm primaries, Illuminant E white.
const XYZ_TO_VSF_RGB: [f32; 9] = t3(vsf::colour::XYZ2VSF_RGB);

/// linear VSF RGB → linear Rec.2020 (row-major) — the DISPLAY concatenation applied to the stored camera→VSF-RGB matrix. Never stored; the monitor assumption lives only here.
const VSF_RGB_TO_REC2020: [f32; 9] = t3(vsf::colour::VSF_RGB2REC2020);

fn matmul3(a: &[f32; 9], b: &[f32; 9]) -> [f32; 9] {
    let mut m = [0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            m[r * 3 + c] = a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
        }
    }
    m
}

fn inv3(m: &[f32; 9]) -> Option<[f32; 9]> {
    let det = m[0] * (m[4] * m[8] - m[5] * m[7])
        - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6]);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv_det = 1. / det;
    Some([
        (m[4] * m[8] - m[5] * m[7]) * inv_det,
        (m[2] * m[7] - m[1] * m[8]) * inv_det,
        (m[1] * m[5] - m[2] * m[4]) * inv_det,
        (m[5] * m[6] - m[3] * m[8]) * inv_det,
        (m[0] * m[8] - m[2] * m[6]) * inv_det,
        (m[2] * m[3] - m[0] * m[5]) * inv_det,
        (m[3] * m[7] - m[4] * m[6]) * inv_det,
        (m[1] * m[6] - m[0] * m[7]) * inv_det,
        (m[0] * m[4] - m[1] * m[3]) * inv_det,
    ])
}

/// EXIF LightSource code → CIE XYZ whitepoint (Y = 1). The codes DNG CalibrationIlluminant tags actually use; anything unrecognised (including 0 = absent) assumes D65 — the same scene-illuminant assumption as the Rec.2020 monitor target.
fn illuminant_xyz(code: u16) -> [f32; 3] {
    match code {
        2 | 17 => [1.09850, 1., 0.35585],  // tungsten / Standard A
        20 => [0.95682, 1., 0.92149],      // D55
        22 => [0.94972, 1., 1.22638],      // D75
        23 => [0.96422, 1., 0.82521],      // D50
        _ => [0.95047, 1., 1.08883],       // D65 / daylight / default
    }
}

/// **Absolute IDT** characterization from a DNG colour matrix (`XYZ → camera`): camera → XYZ → linear **VSF RGB**, straight inversion, NO chromatic adaptation and NO scaling — the scene illuminant's cast is preserved as captured, per the VERICHROME taxonomy (chromatic adaptation / "white balance" is a Creative IDT). The matrix is stored unscaled; the illuminant code rides alongside so display can re-derive an exposure scalar. `None` if the matrix is singular. `source` names which DNG matrix this came from.
fn derive_profile(cm: [f32; 9], illuminant: u16, source: &str) -> Option<ProfileEntry> {
    let cam_to_xyz = inv3(&cm)?;
    let matrix = matmul3(&XYZ_TO_VSF_RGB, &cam_to_xyz);
    Some(ProfileEntry {
        matrix,
        source: source.to_string(),
        class: IdtClass::Absolute,
        grade: ProfileGrade::Model,
        illuminant,
        transfer: Transfer::Linear,
    })
}

/// The display matrix for a characterized image: `VSF_RGB2REC2020 × entries[0]`, then normalized so the elected entry's illuminant lands at display peak 1 (a legally-exposed scene doesn't clip). The scalar is DERIVED here, never stored — it depends on the monitor target. `None` when uncharacterized, the target isn't VSF RGB, or the result is singular ⇒ render raw-camera.
fn display_matrix(img: &SpectralImage) -> Option<[f32; 9]> {
    let profile = img.profile.as_ref()?;
    if profile.target != "vsf_rgb" {
        return None;
    }
    let entry = profile.entries.first()?;
    let mut disp = matmul3(&VSF_RGB_TO_REC2020, &entry.matrix);

    // Exposure scalar: the illuminant's own landing in display space. cam_wp = CM·wp, and disp·cam_wp reduces to (XYZ→Rec2020)·wp — independent of the camera matrix — so we compute it straight from the illuminant whitepoint.
    let xyz_to_rec2020 = matmul3(&VSF_RGB_TO_REC2020, &XYZ_TO_VSF_RGB);
    let wp = illuminant_xyz(entry.illuminant);
    let lit = [
        xyz_to_rec2020[0] * wp[0] + xyz_to_rec2020[1] * wp[1] + xyz_to_rec2020[2] * wp[2],
        xyz_to_rec2020[3] * wp[0] + xyz_to_rec2020[4] * wp[1] + xyz_to_rec2020[5] * wp[2],
        xyz_to_rec2020[6] * wp[0] + xyz_to_rec2020[7] * wp[1] + xyz_to_rec2020[8] * wp[2],
    ];
    let peak = lit[0].max(lit[1]).max(lit[2]);
    if peak <= 0. || !peak.is_finite() {
        return None;
    }
    for v in &mut disp {
        *v /= peak;
    }
    Some(disp)
}

/// Decode any supported source into a [`Decoded`]: VSF-Image files are read directly (no cmx yet); everything else is ingested through iris (camera RAW / DNG) with a camera→Rec.2020 cmx when a ColorMatrix1 is present.
pub fn load_any(input: &Path) -> Result<Decoded, String> {
    match input.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("vsf") => {
            let bytes = std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?;
            let img = spectral_image::read(&bytes).map_err(|e| e.to_string())?;
            // The profile round-trips inside the file, so a reopened VSF renders colour-managed — no separate cmx to reconstruct.
            Ok(Decoded { img })
        }
        _ => ingest_image(input),
    }
}

/// Camera RAW / DNG → [`Decoded`], in memory (no file written). Decodes via iris, packs the sensor plane as a `BitPackedTensor` at native bit depth, records the CFA as a channel-index tile, and derives the camera→Rec.2020 cmx from the DNG ColorMatrix1 when present (3-channel sources only).
pub fn ingest_image(input: &Path) -> Result<Decoded, String> {
    let (info, pixels) = iris::read_dng(input).ok_or_else(|| format!("{}: unable to decode", input.display()))?;

    let bit_depth = if info.bitdepth >= 1 && info.bitdepth <= 16 { info.bitdepth } else { 16 };

    let (channels, layout, samples) = if info.rgb {
        // Already-demosaiced source (RGB TIFF-like): de-interleave [h,w,3] → planar [3,h,w].
        let n = info.width * info.height;
        if pixels.len() != n * 3 {
            return Err(format!("RGB source pixel count {} != {}×{}×3", pixels.len(), info.width, info.height));
        }
        let mut planar = vec![0u16; n * 3];
        for i in 0..n {
            planar[i] = pixels[i * 3];
            planar[n + i] = pixels[i * 3 + 1];
            planar[2 * n + i] = pixels[i * 3 + 2];
        }
        (
            rgb_channel_names().to_vec(),
            PlaneLayout::Planar,
            BitPackedTensor::pack(bit_depth, vec![3, info.height, info.width], &planar),
        )
    } else {
        let tile_h = info.cfah as usize;
        let tile_w = info.cfaw as usize;
        if tile_h * tile_w == 0 || info.cfa.len() != tile_h * tile_w {
            return Err(format!("CFA tile {}×{} doesn't match pattern length {}", tile_h, tile_w, info.cfa.len()));
        }
        if pixels.len() != info.width * info.height {
            return Err(format!("mosaic pixel count {} != {}×{}", pixels.len(), info.width, info.height));
        }
        let k = (*info.cfa.iter().max().unwrap() as usize) + 1;
        let names: Vec<String> = if k == 3 { rgb_channel_names().to_vec() } else { (0..k).map(|i| format!("ch{i}")).collect() };
        (
            names,
            PlaneLayout::Mosaic { cfa: Tensor::new(vec![tile_h, tile_w], info.cfa.clone()) },
            BitPackedTensor::pack(bit_depth, vec![info.height, info.width], &pixels),
        )
    };
    let k = channels.len();

    // Tiered colour_profile only for 3-channel sources with a DNG colour matrix — Absolute-IDT `model`-grade entries (see derive_profile). BOTH matrices become entries, daylight-characterized one FIRST (better fit for typical scenes; ordering is a reader policy, not a destroyed decision — the loser is still carried). The verbatim DNG tags ride alongside so the derivation is auditable and re-derivable. Multispectral (k≠3) awaits the spectral resolve.
    let profile = if k == 3 {
        let daylight = |code: u16| matches!(code, 0 | 1 | 9 | 10 | 20 | 21 | 22 | 23);
        let e1 = info.colourmatrix1.and_then(|m| derive_profile(m, info.calibrationilluminant1, "dng_colormatrix1"));
        let e2 = info.colourmatrix2.and_then(|m| derive_profile(m, info.calibrationilluminant2, "dng_colormatrix2"));
        // Order best-first: put the daylight-family entry ahead of the other.
        let cm2_first = daylight(info.calibrationilluminant2) && !daylight(info.calibrationilluminant1);
        let entries: Vec<ProfileEntry> = if cm2_first {
            [e2, e1].into_iter().flatten().collect()
        } else {
            [e1, e2].into_iter().flatten().collect()
        };
        if entries.is_empty() {
            None
        } else {
            Some(ColourProfile {
                target: "vsf_rgb".to_string(),
                entries,
                dng_colormatrix: [
                    info.colourmatrix1.map(|m| (m, info.calibrationilluminant1)),
                    info.colourmatrix2.map(|m| (m, info.calibrationilluminant2)),
                ],
                patches: None,
                cal: None,
            })
        }
    } else {
        None
    };

    let img = SpectralImage {
        width: info.width,
        height: info.height,
        channels: channels.into_iter().map(|name| SpectralChannel { name, curve: None }).collect(),
        layout,
        samples,
        black: vec![info.black; k],
        white: vec![info.white; k],
        make: info.make.trim_end_matches('\0').trim().to_string(),
        model: info.model.trim_end_matches('\0').trim().to_string(),
        provenance: Provenance::default(),
        profile,
        view: None,
    };

    Ok(Decoded { img })
}

/// Serialize a `SpectralImage` to a VSF-Image file.
pub fn write_vsf(img: &SpectralImage, output: &Path) -> Result<(), String> {
    let bytes = spectral_image::write(img)?;
    std::fs::write(output, &bytes).map_err(|e| format!("{}: {e}", output.display()))
}

fn rgb_channel_names() -> [String; 3] {
    ["R".to_string(), "G".to_string(), "B".to_string()]
}

/// Fixed-point shift for the render path. Q24 with u16 samples and i64 accumulation leaves ~2^23 of headroom even for a 36-sample X-Trans tile; coefficient quantisation error is ~2^-24 relative — three orders below the u16 output step.
const QSHIFT: u32 = 24;

/// Per-camera-channel integer render constants: `contribution_o = (coef[o] · raw_count) >> QSHIFT`, with `bias[o]` (the accumulated black level, same scale) subtracted once per output pixel. Derived in f64 from cmx × bin-weight × black/white normalisation × 65535 — float exists only HERE, deriving constants; the per-pixel loop is integer multiply-accumulate and one shift.
struct ChannelCoef {
    coef: [i64; 3],
}

/// Per-channel sensor level under the two contracts the metadata actually blesses: one scalar broadcast to every channel, or exactly one level per channel. Anything else is a malformed file — fail loud, never silently reuse a neighbour's level.
fn level(levels: &[f32], ch: usize, k: usize, what: &str) -> Result<f32, String> {
    match levels.len() {
        1 => Ok(levels[0]),
        n if n == k => Ok(levels[ch]),
        n => Err(format!("{what} level: {n} entries for {k} channels — neither scalar nor per-channel")),
    }
}

/// Build per-channel coefficient rows + the per-output bias. `tile_count[ch]` = samples of that channel per accumulation unit (tile for mosaic, 1 for planar). Channels ≥ 3 (or with degenerate ranges) get zero rows.
fn build_coefs(img: &SpectralImage, cmx: &Option<[f32; 9]>, tile_count: &[f64]) -> Result<(Vec<ChannelCoef>, [i64; 3]), String> {
    let identity = [1f32, 0., 0., 0., 1., 0., 0., 0., 1.];
    let m = cmx.as_ref().unwrap_or(&identity);
    let k = tile_count.len();
    let mut rows = Vec::with_capacity(k);
    let mut bias = [0i64; 3];
    let scale = (1u64 << QSHIFT) as f64;
    // Levels are validated against the IMAGE's channel count, not the render subset: planar renders take the first three channels of a possibly-wider file whose level arrays cover all its channels.
    let kimg = img.channel_count();
    for ch in 0..k {
        let black = level(&img.black, ch, kimg, "black")? as f64;
        let white = level(&img.white, ch, kimg, "white")? as f64;
        let range = white - black;
        let mut coef = [0i64; 3];
        if ch < 3 && range > 0. && tile_count[ch] > 0. {
            for o in 0..3 {
                let c = m[o * 3 + ch] as f64 * 65535. / (tile_count[ch] * range);
                coef[o] = (c * scale).round() as i64;
                // The black level enters once per sample; a tile has tile_count samples of this channel.
                bias[o] += (c * scale * black * tile_count[ch]).round() as i64;
            }
        }
        rows.push(ChannelCoef { coef });
    }
    Ok((rows, bias))
}

/// Round-to-nearest shift-down, SIGNED — no clamp: sub-black noise stays negative and above-white speculars stay above white, so exposure can move them back into view. The single display clamp lives at the encode boundary, after the exposure multiply. Magnitude proof for the i32 cast: coef·raw ≈ |m|·65535·(raw−black)/range, and raw overshoots white by small factors — a few 2^20 at the wildest, nowhere near 2^31.
#[inline]
fn q_to_lin(acc: i64) -> i32 {
    ((acc + (1i64 << (QSHIFT - 1))) >> QSHIFT) as i32
}

/// Render to linear SIGNED interleaved RGB, white at 65535 — values outside 0..65535 are preserved (negative = read noise below black / out-of-Rec.2020-gamut; above = speculars past the illuminant peak), so exposure can recover them; the single display clamp happens at the encode boundary. Mosaic: each CFA tile → one output pixel (2:1 for a 2×2 Bayer). The debayer bin, the black/white normalisation, and the camera→Rec.2020 cmx are **baked into integer Q24 constants** — the per-pixel work is one `i64` multiply-accumulate per (sample × output) and a shift; no float touches a pixel (see [`build_coefs`]). Without a cmx the constants encode a plain channel-averaged bin (raw camera space). Planar sources take the first three channels.
pub fn to_linear(dec: &Decoded) -> Result<(usize, usize, Vec<i32>), String> {
    let img = &dec.img;
    // Display matrix derived fresh from the stored VSF-RGB profile: VSF_RGB2REC2020 × elected entry, illuminant-normalized. None ⇒ raw-camera bin.
    let cmx = display_matrix(img);
    let counts = img.samples.unpack_u16();

    let (out_w, out_h, rgb) = match &img.layout {
        PlaneLayout::Mosaic { cfa } => {
            let th = cfa.shape[0];
            let tw = cfa.shape[1];
            let ow = img.width / tw;
            let oh = img.height / th;

            // Per-CFA-channel sample count in one tile (uniform across the image).
            let kmax = *cfa.data.iter().max().unwrap_or(&0) as usize + 1;
            let mut tile_count = vec![0f64; kmax];
            for &c in &cfa.data {
                tile_count[c as usize] += 1.;
            }
            let (rows, bias) = build_coefs(img, &cmx, &tile_count)?;

            let mut rgb = vec![0i32; ow * oh * 3];
            for by in 0..oh {
                for bx in 0..ow {
                    let mut acc = [-bias[0], -bias[1], -bias[2]];
                    for ty in 0..th {
                        let row = (by * th + ty) * img.width + bx * tw;
                        for tx in 0..tw {
                            let c = &rows[cfa.data[ty * tw + tx] as usize].coef;
                            let v = counts[row + tx] as i64;
                            acc[0] += c[0] * v;
                            acc[1] += c[1] * v;
                            acc[2] += c[2] * v;
                        }
                    }
                    let out = (by * ow + bx) * 3;
                    rgb[out] = q_to_lin(acc[0]);
                    rgb[out + 1] = q_to_lin(acc[1]);
                    rgb[out + 2] = q_to_lin(acc[2]);
                }
            }
            (ow, oh, rgb)
        }
        PlaneLayout::Planar => {
            let k = img.channel_count();
            if k < 3 {
                return Err(format!("need ≥3 channels for an RGB rendering, got {k}"));
            }
            let (rows, bias) = build_coefs(img, &cmx, &vec![1f64; 3])?;
            let n = img.width * img.height;
            let mut rgb = vec![0i32; n * 3];
            for i in 0..n {
                let cam = [counts[i] as i64, counts[n + i] as i64, counts[2 * n + i] as i64];
                for o in 0..3 {
                    let acc = rows[0].coef[o] * cam[0] + rows[1].coef[o] * cam[1] + rows[2].coef[o] * cam[2] - bias[o];
                    rgb[i * 3 + o] = q_to_lin(acc);
                }
            }
            (img.width, img.height, rgb)
        }
    };

    Ok((out_w, out_h, rgb))
}
