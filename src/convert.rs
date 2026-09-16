//! Image loading + VSF conversion, used by the viewer. `load_any` decodes any supported source (VSF-Image passthrough, or camera RAW/DNG via limbus) into a [`Decoded`] — a `SpectralImage` carrying its own tiered colour_profile; `to_linear` renders it, deriving the display matrix from that profile and baking the debayer bin + matrix into one integer pass (signed output); `write_vsf` saves the image.
//!
//! Colour, per the VERICHROME IDT taxonomy: the DNG path is an **Absolute IDT** — camera → XYZ → linear **VSF RGB** by straight matrix inversion, illuminant cast preserved, no chromatic adaptation (that's a Creative IDT and not welcome here). The STORED reference is VSF RGB (spectral 703/523/462nm primaries, Illuminant E) — never Rec.2020 or XYZ, which are display/rendering targets resolved at read time. **Relative (DSR)** rendering comes from a chameleon magic-9 when one exists for the source — that entry just gets elected first. The monitor is assumed Rec.2020 primaries / gamma-2 — an assumption that lives at DISPLAY (concatenation `VSF_RGB2REC2020 × stored`), NOT in the stored file. Sources without any matrix render raw-camera.
//!
//! Matrix convention: opsin stays row-major throughout (row = output channel, `m[out*3 + in]`), matching the DNG ColorMatrix spec and this crate's `inv3`/`matmul3`/`build_coefs`. vsf::colour stores the SAME matrices column-major, so its numeric constants are pulled in via the `t3` transpose — one source of truth for the primaries, no convention clash.

use rayon::prelude::*;
use std::path::Path;
use vsf::spectral_image::{self, ColourProfile, IdtClass, PlaneLayout, ProfileEntry, ProfileGrade, Provenance, SpectralChannel, SpectralImage, Transfer, ViewOp, ViewTransform};
use vsf::{BitPackedTensor, Tensor};

/// Extensions the viewer will try to open + arrow-navigate. `vsf` is the native container; the RAW/TIFF family goes through limbus (50+ RAW formats — this is a representative common subset, not exhaustive); `jxl`/`jpg`/`webp` are the display-referred ingests (lumis exports and web files — JPEG and WebP are assumed sRGB, the format convention).
pub const SUPPORTED_EXTS: &[&str] = &[
    "vsf", "dng", "arw", "cr2", "cr3", "nef", "nrw", "raf", "rw2", "orf", "pef", "srw", "raw", "tif", "tiff", "jxl", "jpg", "jpeg", "webp",
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
/// The linear space a render lands in. `Rec2020` is the viewer's display space; `VsfRgb` keeps the buffer in VSF RGB for a host that converts at its own display step (photon, 2026-09-11: "vsf rgb as much as possible").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Rec2020,
    VsfRgb,
}

fn display_matrix(img: &SpectralImage, target: Target) -> Option<[f32; 9]> {
    let profile = img.profile.as_ref()?;
    if profile.target != "vsf_rgb" {
        return None;
    }
    let entry = profile.entries.first()?;
    if target == Target::VsfRgb {
        // Same exposure scalar, taken in VSF RGB: the illuminant's landing is (XYZ→VSF RGB)·wp.
        let mut disp = entry.matrix;
        let wp = illuminant_xyz(entry.illuminant);
        let m = &XYZ_TO_VSF_RGB;
        let lit = [
            m[0] * wp[0] + m[1] * wp[1] + m[2] * wp[2],
            m[3] * wp[0] + m[4] * wp[1] + m[5] * wp[2],
            m[6] * wp[0] + m[7] * wp[1] + m[8] * wp[2],
        ];
        let peak = lit[0].max(lit[1]).max(lit[2]);
        if peak <= 0. || !peak.is_finite() {
            return None;
        }
        for v in &mut disp {
            *v /= peak;
        }
        return Some(disp);
    }
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

/// Decode any supported source into a [`Decoded`]: VSF-Image files are read directly (no cmx yet); everything else is ingested through limbus (camera RAW / DNG) with a camera→Rec.2020 cmx when a ColorMatrix1 is present.
pub fn load_any(input: &Path) -> Result<Decoded, String> {
    match input.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("vsf") => {
            let bytes = std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?;
            let img = spectral_image::read(&bytes).map_err(|e| e.to_string())?;
            // The profile round-trips inside the file, so a reopened VSF renders colour-managed — no separate cmx to reconstruct.
            Ok(Decoded { img })
        }
        Some("jxl") => ingest_jxl(input),
        Some("jpg" | "jpeg") => ingest_jpeg(input),
        Some("webp") => ingest_webp(input),
        _ => ingest_image(input),
    }
}

/// Assemble a display-referred ingest into a [`Decoded`]: LINEAR planar u16 RGB (transfer already un-done by the caller) + a single `Assumed`-grade profile entry mapping the tagged/conventional display primaries → VSF RGB. Shared by the JXL and JPEG paths — the characterization is the format's word, not a measurement, and `Assumed` says so honestly.
fn display_referred(w: usize, h: usize, planar: Vec<u16>, cam_to_vsf: [f32; 9], source: &str) -> Decoded {
    let img = SpectralImage {
        width: w,
        height: h,
        channels: rgb_channel_names().into_iter().map(|name| SpectralChannel { name, curve: None }).collect(),
        layout: PlaneLayout::Planar,
        samples: BitPackedTensor::pack(16, vec![3, h, w], &planar),
        black: vec![0.; 3],
        white: vec![65535.; 3],
        make: String::new(),
        model: String::new(),
        provenance: Provenance::default(),
        profile: Some(ColourProfile {
            target: "vsf_rgb".to_string(),
            entries: vec![ProfileEntry {
                matrix: cam_to_vsf,
                source: source.to_string(),
                class: IdtClass::Absolute,
                grade: ProfileGrade::Assumed,
                illuminant: 21, // D65 — the white point of every accepted display space.
                transfer: Transfer::Linear,
            }],
            dng_colormatrix: [None, None],
            patches: None,
            cal: None,
        }),
        view: None,
    };
    Decoded { img }
}

/// Untagged-convention JPEG → [`Decoded`]: decoded RGB8 assumed sRGB (the web's defined default — same assumption every platform makes), sRGB EOTF un-done to linear via a 256-entry LUT, stored planar u16 with an sRGB→VSF-RGB `Assumed` entry. ICC profiles, if present, are ignored — the whole point of this path is the sRGB convention. EXIF orientation is not parsed yet (most posts are pre-rotated); greyscale JPEGs come back RGB from the decoder's requested output space.
#[allow(deprecated)]
fn ingest_jpeg(input: &Path) -> Result<Decoded, String> {
    let bytes = std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let options = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(zune_core::colorspace::ColorSpace::RGB);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(bytes), options);
    let rgb = dec.decode().map_err(|e| format!("{}: {e}", input.display()))?;
    let info = dec.info().ok_or_else(|| format!("{}: no dimensions", input.display()))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let n = w * h;
    if rgb.len() != n * 3 {
        return Err(format!("{}: decoded {} bytes for {w}×{h}×3", input.display(), rgb.len()));
    }
    let planar = srgb8_to_linear_planar(&rgb, 3, n);
    Ok(display_referred(w, h, planar, t3(vsf::colour::SRGB2VSF_RGB), "jpeg_assumed_srgb"))
}

/// Interleaved sRGB8 (`stride` bytes a pixel: 3 for RGB, 4 for RGBA) → LINEAR planar u16 RGB, `n` pixels. sRGB EOTF un-done via a 256-entry LUT built once per process. With a fourth byte the pixel is composited over black (the linear value scaled by alpha) — opsin has no alpha plane, and a transparent region carrying leftover colour would otherwise render as garbage; over black is what a viewer with no backdrop honestly shows.
fn srgb8_to_linear_planar(px: &[u8], stride: usize, n: usize) -> Vec<u16> {
    static LUT: std::sync::OnceLock<[u16; 256]> = std::sync::OnceLock::new();
    let lut = LUT.get_or_init(|| {
        let mut t = [0u16; 256];
        for (v, out) in t.iter_mut().enumerate() {
            *out = (vsf::colour::srgb_eotf(v as f32 / 255.) * 65535.).round() as u16;
        }
        t
    });
    let mut planar = vec![0u16; n * 3];
    let (rp, rest) = planar.split_at_mut(n);
    let (gp, bp) = rest.split_at_mut(n);
    rp.par_iter_mut().zip(gp.par_iter_mut()).zip(bp.par_iter_mut()).enumerate().for_each(|(i, ((r, g), b))| {
        let s = i * stride;
        let (lr, lg, lb) = (lut[px[s] as usize] as u32, lut[px[s + 1] as usize] as u32, lut[px[s + 2] as usize] as u32);
        if stride == 4 {
            let a = px[s + 3] as u32;
            *r = ((lr * a + 127) / 255) as u16;
            *g = ((lg * a + 127) / 255) as u16;
            *b = ((lb * a + 127) / 255) as u16;
        } else {
            *r = lr as u16;
            *g = lg as u16;
            *b = lb as u16;
        }
    });
    planar
}

/// Untagged-convention WebP → [`Decoded`]: the JPEG path's twin. Lossy and lossless both decode to RGB8 (RGBA8 when the extended header carries alpha — composited over black in linear), assumed sRGB like every web file, linearized and stored planar u16 with the same sRGB→VSF-RGB `Assumed` entry. An animated file yields its first frame. The ICC chunk is ignored on purpose (the sRGB convention IS this path) and EXIF orientation is not parsed, matching JPEG.
fn ingest_webp(input: &Path) -> Result<Decoded, String> {
    let bytes = std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| format!("{}: {e}", input.display()))?;
    let (w, h) = dec.dimensions();
    let (w, h) = (w as usize, h as usize);
    let n = w * h;
    let stride = if dec.has_alpha() { 4 } else { 3 };
    let len = dec.output_buffer_size().ok_or_else(|| format!("{}: {w}×{h} does not fit in memory", input.display()))?;
    let mut px = vec![0u8; len];
    dec.read_image(&mut px).map_err(|e| format!("{}: {e}", input.display()))?;
    if px.len() != n * stride {
        return Err(format!("{}: decoded {} bytes for {w}×{h}×{stride}", input.display(), px.len()));
    }
    let planar = srgb8_to_linear_planar(&px, stride, n);
    Ok(display_referred(w, h, planar, t3(vsf::colour::SRGB2VSF_RGB), "webp_assumed_srgb"))
}

/// Display-referred JXL → [`Decoded`]. The inverse concession to [`export_srgb_jpeg`]'s forward one: a JXL carries finished display colour (lumis exports are Rec.2020 primaries + gamma; web files are sRGB), so ingest un-does the transfer (EOTF → linear) and stores the result as a 16-bit planar plane whose profile entry maps that display space → VSF RGB — `Assumed` grade, because the characterization is the format tag, not a measurement. The decoder applies the codestream orientation itself (JXL's own display contract — decoders MUST honour it, unlike EXIF's advisory tag), so no orientation view op is recorded. ICC-profiled and HDR (PQ/HLG) streams are rejected rather than guessed at.
fn ingest_jxl(input: &Path) -> Result<Decoded, String> {
    use jxl_oxide::color::{ColourEncoding, Primaries, TransferFunction};
    let image = jxl_oxide::JxlImage::builder().open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let ColourEncoding::Enum(enc) = &image.image_header().metadata.colour_encoding else {
        return Err(format!("{}: ICC-profiled JXL not supported (enum colour encodings only)", input.display()));
    };
    // Display space → linear VSF RGB, from the tagged primaries (white D65 for both). This is the profile entry's matrix — the stored plane is linear in the TAGGED primaries; VSF RGB is reached at read time like every other source.
    let cam_to_vsf: [f32; 9] = match enc.primaries {
        Primaries::Srgb => t3(vsf::colour::SRGB2VSF_RGB),
        Primaries::Bt2100 => inv3(&VSF_RGB_TO_REC2020).ok_or("Rec.2020 primaries matrix singular")?,
        other => return Err(format!("{}: unsupported JXL primaries {other:?}", input.display())),
    };
    // EOTF exponent/curve to LINEARIZE the decoded samples. jxl gamma signalling is the OETF gamma (lumis writes 0.5 for its sqrt encode), parsed with `inverted: true` ⇒ EOTF exponent = 1e7/g.
    enum Eotf {
        Linear,
        Srgb,
        Pow(f32),
    }
    let eotf = match enc.tf {
        TransferFunction::Linear => Eotf::Linear,
        TransferFunction::Srgb => Eotf::Srgb,
        TransferFunction::Gamma { g, inverted } if g > 0 => Eotf::Pow(if inverted { 1e7 / g as f32 } else { g as f32 / 1e7 }),
        other => return Err(format!("{}: unsupported JXL transfer function {other:?}", input.display())),
    };

    let render = image.render_frame(0).map_err(|e| format!("{}: {e}", input.display()))?;
    let fb = render.image_all_channels();
    let (w, h, ch) = (fb.width(), fb.height(), fb.channels());
    if ch < 3 {
        return Err(format!("{}: {ch}-channel JXL — RGB only", input.display()));
    }
    let n = w * h;
    let buf = fb.buf();
    // Interleaved f32 [h,w,ch] → linear planar u16 [3,h,w], alpha (ch 4) dropped. Rows are independent — rayon over the three planes' output rows via one pass per plane keeps it simple: iterate output rows of the planar buffer jointly instead.
    let mut planar = vec![0u16; n * 3];
    let (rp, rest) = planar.split_at_mut(n);
    let (gp, bp) = rest.split_at_mut(n);
    let lin_of = |e: f32| -> u16 {
        let e = e.clamp(0., 1.);
        #[allow(deprecated)]
        let l = match eotf {
            Eotf::Linear => e,
            Eotf::Srgb => vsf::colour::srgb_eotf(e),
            Eotf::Pow(p) => e.powf(p),
        };
        (l * 65535.).round() as u16
    };
    rp.par_iter_mut().zip(gp.par_iter_mut()).zip(bp.par_iter_mut()).enumerate().for_each(|(i, ((r, g), b))| {
        let s = i * ch;
        *r = lin_of(buf[s]);
        *g = lin_of(buf[s + 1]);
        *b = lin_of(buf[s + 2]);
    });

    Ok(display_referred(w, h, planar, cam_to_vsf, "jxl_colour_encoding"))
}

/// Camera RAW / DNG → [`Decoded`], in memory (no file written). Decodes via limbus, packs the sensor plane as a `BitPackedTensor` at native bit depth, records the CFA as a channel-index tile, and derives the camera→Rec.2020 cmx from the DNG ColorMatrix1 when present (3-channel sources only).
pub fn ingest_image(input: &Path) -> Result<Decoded, String> {
    let (info, pixels) = limbus::read_dng(input).ok_or_else(|| format!("{}: unable to decode", input.display()))?;

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
    // An IDENTITY ColorMatrix1 with no ColorMatrix2 is lumis's explicit "uncalibrated" sentinel (chameleon hasn't scanned this camera yet) — not a characterization. Treating it as one would push raw camera counts through XYZ→VSF-RGB as if they were XYZ: the green, desaturated render. No profile ⇒ honest raw-camera rendering, and the HUD says so.
    let identity = |m: &[f32; 9]| m.iter().zip(&[1f32, 0., 0., 0., 1., 0., 0., 0., 1.]).all(|(a, b)| (a - b).abs() < 1e-6);
    let uncalibrated = info.colourmatrix1.as_ref().is_some_and(identity) && info.colourmatrix2.is_none();
    // A "Verichrome scene-relative IDT" profile name means the matrix came from a chameleon target scan of THIS camera: `unit` grade, `relative` (DSR) class — elected first over any factory matrix. Header read only; a non-TIFF or missing tag just leaves the factory grading.
    let verichrome = crate::tiff::FrameMeta::read_path(input).ok().and_then(|m| m.profile_name).is_some_and(|n| n.to_ascii_lowercase().contains("verichrome"));
    let profile = if k == 3 && !uncalibrated {
        let daylight = |code: u16| matches!(code, 0 | 1 | 9 | 10 | 20 | 21 | 22 | 23);
        let grade = |mut e: ProfileEntry| {
            if verichrome {
                e.grade = ProfileGrade::Unit;
                e.class = IdtClass::Relative;
            }
            e
        };
        let e1 = info.colourmatrix1.and_then(|m| derive_profile(m, info.calibrationilluminant1, "dng_colormatrix1")).map(grade);
        let e2 = info.colourmatrix2.and_then(|m| derive_profile(m, info.calibrationilluminant2, "dng_colormatrix2")).map(grade);
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

    // EXIF Orientation (tag 274) enters the translateration log verbatim — the camera's display-time claim, never applied to the sensor plane. Codes 2..=8 are real transforms; 1 (normal) and limbus's absent-sentinel 9 record nothing.
    let view = (2..=8).contains(&info.orientation).then(|| ViewTransform {
        space: "vsf_rgb_linear".to_string(),
        ops: vec![ViewOp {
            name: "orientation".to_string(),
            class: IdtClass::Technical,
            params: vec![info.orientation as f32],
        }],
    });

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
        view,
    };

    Ok(Decoded { img })
}

/// The HDR highlight rolloff — Photon's audio wire shaper (`call/qgain.rs::cubic_rail`) on the u16 display domain: `y = (3x − (x³ >> 32)) >> 1`, i.e. `(3x − x³)/2` with the rail at 65535. Integer, branchless, one multiply chain; `f(0) = 0`, `f(rail) = rail`, slope 3/2 at black, slope 0 exactly at the rail — a soft shoulder that reaches display white tangentially, so the clamp lands where the curve is already flat and no edge shows; its only distortion product is 3rd-order. Brightens the low end (+0.58 stop) and compresses the top; pull exposure down ~3× and the top ~1.5 stops that used to clip now roll off. The input clamp is load-bearing: past the rail the cubic FOLDS BACK, so overs must pin to the rail first (the encode boundary's clamp already does). Per channel, in linear, at the ONE encode boundary — viewer LUT and JPEG export call this same function, so they are bit-identical. A Creative op: recorded, never silent. Oriel's `sin(πx/2)` rolloff is the same shape within 0.023.
#[inline]
pub fn hdr_rail(x: i64) -> i64 {
    let x = x.clamp(0, 65535);
    ((3 * x - ((x * x * x) >> 32)) >> 1).clamp(0, 65535)
}

/// `dr_curve` view-op params for [`hdr_rail`]: polynomial coefficients [c0, c1, c2, c3] of f(x) = Σ cᵢxⁱ.
pub const HDR_CURVE_COEFS: [f32; 4] = [0., 1.5, 0., -0.5];

/// Export the rendered view as an sRGB JPEG — the ONE legacy-space concession, for posts on platforms that assume sRGB and strip everything else. Input is [`to_linear`]'s output (signed linear Rec.2020, white = 65535, orientation already applied); the live exposure is baked as a linear gain, then Rec.2020 → sRGB (thru vsf's deprecated legacy constants — deliberately: sRGB IS the legacy), clamp to gamut (the single display clamp, same boundary rule as the viewer), the HDR rolloff when on (same [`hdr_rail`] the viewer encodes with, so the JPEG is the screen), sRGB OETF, quality-95 JPEG. Untagged on purpose — an untagged JPEG is defined-sRGB everywhere that matters, which is exactly the consistency being bought. Nothing is written back to the source: the sensor plane stays as captured, the JPEG is a rendering.
#[allow(deprecated)]
pub fn export_srgb_jpeg(lin: &[i32], w: usize, h: usize, ev: f32, hdr: bool, out: &Path) -> Result<(), String> {
    if lin.len() != w * h * 3 {
        return Err(format!("linear buffer {} != {w}×{h}×3", lin.len()));
    }
    if w > u16::MAX as usize || h > u16::MAX as usize {
        return Err(format!("{w}×{h} exceeds JPEG's 65535 dimension limit"));
    }
    const SRGB_FROM_VSF_RGB: [f32; 9] = t3(vsf::colour::VSF_RGB2SRGB);
    let m = matmul3(&SRGB_FROM_VSF_RGB, &inv3(&VSF_RGB_TO_REC2020).ok_or("Rec.2020 matrix singular")?);
    let gain = (2f32).powf(ev) / 65535.;
    let mut rgb8 = vec![0u8; w * h * 3];
    rgb8.par_chunks_mut(w * 3).zip(lin.par_chunks(w * 3)).for_each(|(orow, irow)| {
        for x in 0..w {
            let c = [irow[x * 3] as f32 * gain, irow[x * 3 + 1] as f32 * gain, irow[x * 3 + 2] as f32 * gain];
            for o in 0..3 {
                let v = (m[o * 3] * c[0] + m[o * 3 + 1] * c[1] + m[o * 3 + 2] * c[2]).clamp(0., 1.);
                // Through the integer rail so the export's shoulder is bit-identical to the viewer's LUT.
                let v = if hdr { hdr_rail((v * 65535.).round() as i64) as f32 / 65535. } else { v };
                orow[x * 3 + o] = (vsf::colour::srgb_oetf(v) * 255.).round() as u8;
            }
        }
    });
    let encoder = jpeg_encoder::Encoder::new_file(out, 95).map_err(|e| format!("{}: {e}", out.display()))?;
    encoder
        .encode(&rgb8, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .map_err(|e| format!("{}: {e}", out.display()))
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

/// Inverse (gather) mapping of an EXIF orientation: DISPLAY pixel (dx, dy) → source pixel in the PRE-orientation `w × h` buffer. Identity for codes outside 2..=8. Shared by the display permute and the histogram's raw-sample lookup, so they can never disagree about which sensor tile a screen pixel shows.
pub fn orientation_src(code: u16, w: usize, h: usize, dx: usize, dy: usize) -> (usize, usize) {
    match code {
        2 => (w - 1 - dx, dy),
        3 => (w - 1 - dx, h - 1 - dy),
        4 => (dx, h - 1 - dy),
        5 => (dy, dx),
        6 => (dy, h - 1 - dx),
        7 => (w - 1 - dy, h - 1 - dx),
        8 => (w - 1 - dy, dx),
        _ => (dx, dy),
    }
}

/// Compose an EXIF orientation code with a further 90° display rotation: the code `r` such that `display(r) = rot(display(code))`. The eight codes are the dihedral group of the frame; each is a signed 2×2 map from source axes to display axes (x right, y down; 6 = rotate 90 CW ⇒ x' = h−1−y, y' = x ⇒ [[0,−1],[1,0]] — the same convention [`orientation_src`] inverts), so composition is a 2×2 integer product and a table lookup. `cw` false ⇒ counter-clockwise. Applying CW four times from any code returns it.
pub fn rotate_code(code: u16, cw: bool) -> u16 {
    const M: [[i8; 4]; 8] = [
        [1, 0, 0, 1],   // 1 normal
        [-1, 0, 0, 1],  // 2 mirror H
        [-1, 0, 0, -1], // 3 rotate 180
        [1, 0, 0, -1],  // 4 mirror V
        [0, 1, 1, 0],   // 5 transpose
        [0, -1, 1, 0],  // 6 rotate 90 CW
        [0, -1, -1, 0], // 7 transverse
        [0, 1, -1, 0],  // 8 rotate 90 CCW
    ];
    let m = M[(code.clamp(1, 8) - 1) as usize];
    let r: [i8; 4] = if cw { M[5] } else { M[7] };
    // r · m
    let p = [r[0] * m[0] + r[1] * m[2], r[0] * m[1] + r[1] * m[3], r[2] * m[0] + r[3] * m[2], r[2] * m[1] + r[3] * m[3]];
    M.iter().position(|c| *c == p).map(|i| i as u16 + 1).unwrap_or(1)
}

/// The EXIF orientation code from the view log: the `orientation` op's first param when present and a real transform (2..=8), else 1 (display as stored).
pub fn orientation_code(img: &SpectralImage) -> u16 {
    img.view
        .as_ref()
        .and_then(|v| v.ops.iter().find(|op| op.name == "orientation"))
        .and_then(|op| op.params.first())
        .map(|&p| p as u16)
        .filter(|c| (2..=8).contains(c))
        .unwrap_or(1)
}

/// Apply an EXIF orientation code to an interleaved 3-channel buffer — display-time only, the stored sensor plane is never touched. Codes 5..=8 swap the output dims; anything outside 2..=8 passes through unmoved. Mapping is (source → display): 2 mirror-H, 3 rotate 180, 4 mirror-V, 5 transpose, 6 rotate 90 CW, 7 transverse, 8 rotate 90 CCW — implemented as the INVERSE gather (each destination pixel fetches its source) so destination rows are independent and split across the rayon pool.
fn apply_orientation(w: usize, h: usize, rgb: Vec<i32>, code: u16) -> (usize, usize, Vec<i32>) {
    if !(2..=8).contains(&code) {
        return (w, h, rgb);
    }
    let (ow, oh) = if code >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0i32; rgb.len()];
    out.par_chunks_mut(ow * 3).enumerate().for_each(|(dy, out_row)| {
        for dx in 0..ow {
            let (sx, sy) = orientation_src(code, w, h, dx, dy);
            let src = (sy * w + sx) * 3;
            out_row[dx * 3..dx * 3 + 3].copy_from_slice(&rgb[src..src + 3]);
        }
    });
    (ow, oh, out)
}

/// Render to linear SIGNED interleaved RGB, white at 65535 — values outside 0..65535 are preserved (negative = read noise below black / out-of-Rec.2020-gamut; above = speculars past the illuminant peak), so exposure can recover them; the single display clamp happens at the encode boundary, and so does the clip indicator (lumis `preview_sub` semantics after the display matrix + EV gain — see `encode_pixels`), so this buffer stays clean for both re-encoding and JPEG export. Mosaic: each CFA tile → one output pixel (2:1 for a 2×2 Bayer). The debayer bin, the black/white normalisation, and the camera→Rec.2020 cmx are **baked into integer Q24 constants** — the per-pixel work is one `i64` multiply-accumulate per (sample × output) and a shift; no float touches a pixel (see [`build_coefs`]). Without a cmx the constants encode a plain channel-averaged bin (raw camera space). Planar sources take the first three channels. Last, the view log's `orientation` op (EXIF tag 274, recorded at ingest) permutes the OUTPUT buffer — display honours the camera's claim while the stored plane stays exactly as captured.
pub fn to_linear(dec: &Decoded) -> Result<(usize, usize, Vec<i32>), String> {
    to_linear_in(dec, Target::Rec2020)
}

/// [`to_linear`] with the landing space chosen: the same integer pipeline, only the matrix differs.
pub fn to_linear_in(dec: &Decoded, target: Target) -> Result<(usize, usize, Vec<i32>), String> {
    let img = &dec.img;
    // Display matrix derived fresh from the stored VSF-RGB profile: VSF_RGB2REC2020 × elected entry, illuminant-normalized. None ⇒ raw-camera bin.
    let cmx = display_matrix(img, target);
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

            // Output rows are independent (each reads only its own tile rows), so the pass splits across the rayon pool.
            let mut rgb = vec![0i32; ow * oh * 3];
            rgb.par_chunks_mut(ow * 3).enumerate().for_each(|(by, out_row)| {
                for bx in 0..ow {
                    let mut acc = [-bias[0], -bias[1], -bias[2]];
                    for ty in 0..th {
                        let row = (by * th + ty) * img.width + bx * tw;
                        for tx in 0..tw {
                            let ch = cfa.data[ty * tw + tx] as usize;
                            let c = &rows[ch].coef;
                            let v = counts[row + tx] as i64;
                            acc[0] += c[0] * v;
                            acc[1] += c[1] * v;
                            acc[2] += c[2] * v;
                        }
                    }
                    out_row[bx * 3] = q_to_lin(acc[0]);
                    out_row[bx * 3 + 1] = q_to_lin(acc[1]);
                    out_row[bx * 3 + 2] = q_to_lin(acc[2]);
                }
            });
            (ow, oh, rgb)
        }
        PlaneLayout::Planar => {
            let k = img.channel_count();
            if k < 3 {
                return Err(format!("need ≥3 channels for an RGB rendering, got {k}"));
            }
            let (rows, bias) = build_coefs(img, &cmx, &vec![1f64; 3])?;
            let n = img.width * img.height;
            let w = img.width;
            let mut rgb = vec![0i32; n * 3];
            rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, out_row)| {
                for x in 0..w {
                    let i = y * w + x;
                    let cam = [counts[i] as i64, counts[n + i] as i64, counts[2 * n + i] as i64];
                    for o in 0..3 {
                        let acc = rows[0].coef[o] * cam[0] + rows[1].coef[o] * cam[1] + rows[2].coef[o] * cam[2] - bias[o];
                        out_row[x * 3 + o] = q_to_lin(acc);
                    }
                }
            });
            (img.width, img.height, rgb)
        }
    };

    Ok(apply_orientation(out_w, out_h, rgb, orientation_code(img)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×1 buffer: pixel A left, pixel B right, distinct per-channel values.
    fn two_px() -> Vec<i32> {
        vec![1, 2, 3, 4, 5, 6]
    }

    #[test]
    fn orientation_identity_and_invalid_pass_through() {
        for code in [0, 1, 9, 42] {
            let (w, h, out) = apply_orientation(2, 1, two_px(), code);
            assert_eq!((w, h), (2, 1));
            assert_eq!(out, two_px());
        }
    }

    #[test]
    fn orientation_transforms_map_corners_correctly() {
        // (code, expected dims, expected buffer) for the 2×1 [A B] source.
        let a = [1, 2, 3];
        let b = [4, 5, 6];
        let cat = |first: &[i32; 3], second: &[i32; 3]| [first.as_slice(), second.as_slice()].concat();
        let cases = [
            (2, (2, 1), cat(&b, &a)),  // mirror-H: [B A]
            (3, (2, 1), cat(&b, &a)),  // rotate 180 of a single row = mirror-H
            (4, (2, 1), cat(&a, &b)),  // mirror-V of a single row = unchanged
            (5, (1, 2), cat(&a, &b)),  // transpose: column [A; B]
            (6, (1, 2), cat(&a, &b)),  // rotate 90 CW: column [A; B] (h=1 so no flip)
            (7, (1, 2), cat(&b, &a)),  // transverse: column [B; A]
            (8, (1, 2), cat(&b, &a)),  // rotate 90 CCW: column [B; A]
        ];
        for (code, dims, expect) in cases {
            let (w, h, out) = apply_orientation(2, 1, two_px(), code);
            assert_eq!((w, h), dims, "dims for code {code}");
            assert_eq!(out, expect, "buffer for code {code}");
        }
    }

    /// A lossless 2×2 WebP with alpha thru the ingest: the fully opaque sRGB white and mid-grey pixels land at their linear u16 values, the transparent one composites to black, the half-alpha one to half its linear value.
    #[test]
    fn webp_ingests_srgb_and_composites_alpha_over_black() {
        // Pixels row-major: white α255, sRGB 128 α255, white α0, white α128.
        let px: [u8; 16] = [255, 255, 255, 255, 128, 128, 128, 255, 255, 255, 255, 0, 255, 255, 255, 128];
        let mut bytes = Vec::new();
        image_webp::WebPEncoder::new(&mut bytes).encode(&px, 2, 2, image_webp::ColorType::Rgba8).unwrap();
        let path = std::env::temp_dir().join(format!("opsin-webp-test-{}.webp", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        let dec = load_any(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!((dec.img.width, dec.img.height), (2, 2));
        assert_eq!(dec.img.profile.as_ref().unwrap().entries[0].source, "webp_assumed_srgb");
        // Planar [3, h, w]: the red plane is the first four samples.
        let all = dec.img.samples.unpack_u16();
        let r = &all[..4];
        let grey = (vsf::colour::srgb_eotf(128. / 255.) * 65535.).round() as u16;
        assert_eq!(r[0], 65535);
        assert_eq!(r[1], grey);
        assert_eq!(r[2], 0);
        assert_eq!(r[3], ((65535u32 * 128 + 127) / 255) as u16);
        // The green and blue planes agree with red for the neutral pixels.
        assert_eq!(all[4 + 1], grey);
        assert_eq!(all[8 + 3], ((65535u32 * 128 + 127) / 255) as u16);
    }

    /// The crate's own hero image is a lossy VP8 WebP: it opens, at its known size, thru the same path.
    #[test]
    fn webp_lossy_hero_opens() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/opsin.webp"));
        let dec = load_any(path).unwrap();
        assert_eq!((dec.img.width, dec.img.height), (1024, 1024));
        assert!(is_supported(path));
    }

    #[test]
    fn to_linear_honours_view_orientation() {
        // 2×1 planar RGB, uncharacterized (identity render), tagged rotate-90-CCW (code 8): display comes back 1×2 with the right pixel on top. The stored plane is untouched — only the render output moves.
        let img = SpectralImage {
            width: 2,
            height: 1,
            channels: rgb_channel_names().into_iter().map(|name| SpectralChannel { name, curve: None }).collect(),
            layout: PlaneLayout::Planar,
            samples: BitPackedTensor::pack(16, vec![3, 1, 2], &[10u16, 20, 30, 40, 50, 60]),
            black: vec![0.; 3],
            white: vec![65535.; 3],
            make: String::new(),
            model: String::new(),
            provenance: Provenance::default(),
            profile: None,
            view: Some(ViewTransform {
                space: "vsf_rgb_linear".to_string(),
                ops: vec![ViewOp { name: "orientation".to_string(), class: IdtClass::Technical, params: vec![8.] }],
            }),
        };
        let (w, h, lin) = to_linear(&Decoded { img }).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(lin, vec![20, 40, 60, 10, 30, 50]);
    }

    #[test]
    fn rotate_code_is_the_dihedral_group() {
        // Four CW turns is the identity from every code; CW then CCW is the identity; the pure rotations cycle 1→6→3→8→1.
        for c in 1..=8u16 {
            let mut x = c;
            for _ in 0..4 {
                x = rotate_code(x, true);
            }
            assert_eq!(x, c, "4×CW from {c}");
            assert_eq!(rotate_code(rotate_code(c, true), false), c, "CW·CCW from {c}");
        }
        assert_eq!(rotate_code(1, true), 6);
        assert_eq!(rotate_code(6, true), 3);
        assert_eq!(rotate_code(3, true), 8);
        assert_eq!(rotate_code(8, true), 1);
        // And the composed code renders as the composed transform: rotating a 6-oriented render once more CW must equal a 3 (180°) render of the same plane.
        let src: Vec<i32> = (0..12).collect();
        let (w6, h6, r6) = apply_orientation(2, 2, src.clone(), 6);
        let (_, _, r6_then_cw) = apply_orientation(w6, h6, r6, 6);
        let (_, _, r3) = apply_orientation(2, 2, src, rotate_code(6, true));
        assert_eq!(r6_then_cw, r3);
    }

    #[test]
    fn rotate90_cw_puts_top_right_first() {
        // 2×2 source [A B; C D], code 6 (rotate 90 CW) → [C A; D B].
        let src: Vec<i32> = (0..12).collect(); // A=0.., B=3.., C=6.., D=9..
        let (w, h, out) = apply_orientation(2, 2, src, 6);
        assert_eq!((w, h), (2, 2));
        let px = |i: usize| &out[i * 3..i * 3 + 3];
        assert_eq!(px(0), &[6, 7, 8]); // C
        assert_eq!(px(1), &[0, 1, 2]); // A
        assert_eq!(px(2), &[9, 10, 11]); // D
        assert_eq!(px(3), &[3, 4, 5]); // B
    }
}

