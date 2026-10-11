//! Copy / paste a DSR IDT — the magic-9 — between frames of the same camera, and carry it in a DNG the way DNG can hold it.
//!
//! A VERICHROME Direct Scene Referred IDT is nine numbers: an XYZ→camera matrix solved from a target scan, with its illuminant. A chameleon scan produces one; an uncalibrated lumis frame carries the identity in its `ColorMatrix1`. The IDT is a property of the SENSOR, so one scan characterizes every frame that sensor ever shot: **copy** lifts the matrix VERBATIM (exact rationals — no float round trip) together with the frame's identity fingerprint; **paste** refuses unless the target's fingerprint matches field for field, then writes the IDT into the target.
//!
//! **How a paste writes (2026-10-08, Nick: "I'm down with your recommendation").** DNG has no slot for an IDT's class, but it has one for a per-unit correction: `CameraCalibration1/2`, which every DNG reader multiplies onto the ColorMatrix — `XYZ→camera = CameraCalibration × ColorMatrix`. So the paste leaves the file's own `ColorMatrix1/2` untouched and writes `CC_i = M × CM_i⁻¹` into each calibration slot, which makes the product come out as the IDT exactly at each slot's illuminant; readers that blend the slots by white balance get the IDT at both ends and a near-identity blend between. The factory matrix survives, and so does the reader's own behaviour: the pairing strings `CameraCalibrationSignature` / `ProfileCalibrationSignature` are set equal (`VERICHROME …`), so a reader using the file's embedded profile applies the calibration, and one that swaps in its own profile (Lightroom with Adobe Standard) sees no match, ignores it, and falls back to its own per-model matrix — never our correction multiplied onto someone else's base. The class, tier, observer the solve targeted, target serial, timestamps and the nine exact rationals ride in an XMP packet (tag 700) under the `verichrome:` namespace — the only place a DNG can say *relative* from *absolute*, or that the solve was for CVRL 2012 and not CIE 1931; nearly every tool preserves XMP.
//!
//! **No original byte moves.** New tags need a bigger IFD0, so the paste APPENDS a copy of IFD0 with the added entries (and their data) at the end of the file and repoints the header's one 4-byte IFD0 offset at it. Every entry the paste doesn't understand is carried verbatim — their offsets still point where they always did. The original IFD0 and length are recorded in the XMP, so [`IdtClip::unpaste`] restores the file byte for byte: one 4-byte write and a truncate. A second paste carries the FIRST paste's originals forward, so undo always lands on the untouched file.
//!
//! The fingerprint is make, model, raw-plane dims, CFA tile, focal length. The gate (Nick 2026-10-10): make and model match → paste, no warning — dims, CFA phase and focal are what one sensor legitimately changes across crop modes, lenses and lumis's slitscan ring, and the IDT is a property of the sensor; make or model differ → refuse unless forced (`Ctrl+Shift+V`, `--force`), and the report then carries a WARNING naming what differed; a different channel count (a Bayer IDT onto a monochrome sensor) → refuse always. Caveat kept on record: a phone's main/ultrawide/tele are different sensors behind one Make/Model ("Android"/"Lumis") and only focal length tells them apart — that paste goes through silently under this rule.
//!
//! The clip persists as readable text ([`IdtClip::to_text`]) at `$XDG_CONFIG_HOME/opsin/idt.clip`, so copy/paste crosses folders and launches, and the nine numbers can be kept, shared, and read by eye. Copy also works from a VSF-Image that carries a verbatim DNG matrix in its colour_profile (f32 there, so rationals are reconstructed exactly from the f32 — den 2^24); paste INTO a VSF is not wired yet: it would add a `unit`-tier colour_profile entry rather than patch a tag, and rewriting a container has provenance rules to settle first.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A signed TIFF rational, verbatim: (numerator, denominator).
pub type SRational = (i32, i32);

/// The XMP namespace the IDT's class and provenance are written under.
pub const VERICHROME_NS: &str = "https://verichrome.cc/ns/idt/1.0/";

/// The identity a frame declares — what has to agree before an IDT may move between two files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub make: String,
    pub model: String,
    /// Raw sensor plane dims (the mosaic, not any preview).
    pub width: usize,
    pub height: usize,
    /// CFA tile dims + channel index per cell (row-major).
    pub cfa_w: u16,
    pub cfa_h: u16,
    pub cfa: Vec<u8>,
    /// EXIF FocalLength as an unsigned rational, verbatim; `None` when the file carries none.
    pub focal: Option<(u32, u32)>,
}

impl Fingerprint {
    /// How many colour channels the CFA tile names (3 for a Bayer tile, 1 for a monochrome sensor, 4 for RGBW) — the one thing an IDT can never be pasted across.
    pub fn channels(&self) -> usize {
        let mut seen: Vec<u8> = self.cfa.clone();
        seen.sort_unstable();
        seen.dedup();
        seen.len().max(1)
    }

    /// Field-by-field comparison; the names of every field that differs (empty ⇒ match). Focal compares by cross-multiplication so 69/10 and 6900/1000 agree; a focal on one side only is a mismatch (an unknown is not a match).
    pub fn diff(&self, other: &Fingerprint) -> Vec<&'static str> {
        let mut d = Vec::new();
        if self.make != other.make {
            d.push("make");
        }
        if self.model != other.model {
            d.push("model");
        }
        if (self.width, self.height) != (other.width, other.height) {
            d.push("sensor");
        }
        if (self.cfa_w, self.cfa_h, &self.cfa) != (other.cfa_w, other.cfa_h, &other.cfa) {
            d.push("cfa");
        }
        let focal_eq = match (self.focal, other.focal) {
            (Some((an, ad)), Some((bn, bd))) => an as u64 * bd as u64 == bn as u64 * ad as u64,
            (None, None) => true,
            _ => false,
        };
        if !focal_eq {
            d.push("focal");
        }
        d
    }

    fn focal_text(&self) -> String {
        match self.focal {
            Some((n, d)) => format!("{n}/{d}"),
            None => "-".to_string(),
        }
    }
}

/// What the IDT claims and where it came from — the VERICHROME class and tier (vsf's closed vocabularies, as strings here because a DNG is an open world), the observer the solve targeted, and the target scan behind it. A clip copied from a factory matrix is `absolute` / `model` with the rest empty: pasting it is a Model-tier transplant between frames of one camera, and the file says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdtProvenance {
    /// `relative` (a DSR solve) or `absolute` (a straight inversion).
    pub class: String,
    /// `unit` (measured on this camera) or `model`.
    pub tier: String,
    /// The observer / colour matching functions the solve targeted, as the settings describe them (chameleon: "CIE/CVRL 2006 2°"); empty when unknown.
    pub observer: String,
    /// The VERICHROME target that produced a `unit` IDT; 0 when none.
    pub target_type: u32,
    pub target_serial: u64,
    /// The target's own calibration timestamp, as chameleon prints it; empty when none.
    pub target_calibrated: String,
    /// When the scan was made (RFC 3339, UTC); empty when unknown.
    pub scanned: String,
}

impl IdtProvenance {
    /// A chameleon scan made now: `relative` / `unit`, the observer the settings name, and the target that produced it (`cal` = type, serial, the target's calibration timestamp).
    pub fn of_scan(observer: &str, cal: Option<(u32, u64, String)>) -> IdtProvenance {
        let (target_type, target_serial, target_calibrated) = cal.unwrap_or((0, 0, String::new()));
        IdtProvenance { observer: observer.to_string(), target_type, target_serial, target_calibrated, scanned: now_rfc3339(), ..Self::chameleon() }
    }
    pub fn factory() -> IdtProvenance {
        IdtProvenance { class: "absolute".into(), tier: "model".into(), observer: String::new(), target_type: 0, target_serial: 0, target_calibrated: String::new(), scanned: String::new() }
    }
    pub fn chameleon() -> IdtProvenance {
        IdtProvenance { class: "relative".into(), tier: "unit".into(), ..Self::factory() }
    }
}

/// The camera's spectral response under the calibration light — the Relative IDT's stored half (`vsf/idt/relative.md`, "The representation"); the nine numbers are its cache. Carried in the clip, the `verichrome:` XMP of a DNG, and a VSF's `characterization`. One common scale across channels; linear band grid.
#[derive(Debug, Clone, PartialEq)]
pub struct IdtResponse {
    pub start_nm: f32,
    pub step_nm: f32,
    pub channels: Vec<Vec<f32>>,
    /// How the curves were regularised (`shortest`).
    pub prior: String,
    /// Identity of the calibration light (BLAKE3 of the scan), so two responses can be compared and a shot's illuminant ratio knows its reference.
    pub light: Option<[u8; 32]>,
}

pub fn response_to_vsf(r: &IdtResponse) -> vsf::visual::SpectralResponse {
    vsf::visual::SpectralResponse { start_nm: r.start_nm, step_nm: r.step_nm, channels: r.channels.clone(), prior: r.prior.clone(), light: r.light }
}

pub fn response_of_vsf(r: &vsf::visual::SpectralResponse) -> IdtResponse {
    IdtResponse { start_nm: r.start_nm, step_nm: r.step_nm, channels: r.channels.clone(), prior: r.prior.clone(), light: r.light }
}

fn hex32(h: &[u8; 32]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl IdtResponse {
    /// The XMP attributes a paste writes: `ResponseGrid="start step count"`, `Response="v v …;v v …"` (channels `;`-separated, channel-major), `ResponsePrior`, `ResponseLight` (hex).
    fn xmp_attrs(&self) -> String {
        let n = self.channels.first().map_or(0, Vec::len);
        let values: Vec<String> = self.channels.iter().map(|c| c.iter().map(|v| format!("{v:.6e}")).collect::<Vec<_>>().join(" ")).collect();
        format!(
            "\n   verichrome:ResponseGrid=\"{} {} {}\"\n   verichrome:ResponsePrior=\"{}\"\n   verichrome:ResponseLight=\"{}\"\n   verichrome:Response=\"{}\"",
            self.start_nm,
            self.step_nm,
            n,
            xml_escape(&self.prior),
            self.light.map(|l| hex32(&l)).unwrap_or_default(),
            values.join(";")
        )
    }

    /// From a `verichrome:` packet; `None` when it carries no response or a malformed one.
    pub fn from_xmp(xmp: &str) -> Option<IdtResponse> {
        let grid = xmp_attr(xmp, "ResponseGrid")?;
        let g: Vec<&str> = grid.split_whitespace().collect();
        if g.len() != 3 {
            return None;
        }
        let (start_nm, step_nm, n): (f32, f32, usize) = (g[0].parse().ok()?, g[1].parse().ok()?, g[2].parse().ok()?);
        let channels: Vec<Vec<f32>> = xmp_attr(xmp, "Response")?.split(';').map(|c| c.split_whitespace().map(|v| v.parse::<f32>()).collect::<Result<Vec<_>, _>>()).collect::<Result<_, _>>().ok()?;
        if n == 0 || channels.is_empty() || channels.iter().any(|c| c.len() != n) {
            return None;
        }
        Some(IdtResponse { start_nm, step_nm, channels, prior: xmp_attr(xmp, "ResponsePrior").unwrap_or_default(), light: xmp_attr(xmp, "ResponseLight").and_then(|h| unhex32(&h)) })
    }
}

/// One copied IDT: the nine verbatim rationals + illuminant, what it claims, where it came from, the fingerprint it may be pasted onto, and — when the scan produced one — the spectral response the nine numbers were derived from.
#[derive(Debug, Clone, PartialEq)]
pub struct IdtClip {
    /// Path of the frame it was copied from — audit, not identity.
    pub source: String,
    pub fingerprint: Fingerprint,
    /// EXIF LightSource code of the solve's illuminant (23 = D50, what chameleon writes).
    pub illuminant: u16,
    /// XYZ→camera, row-major, verbatim SRATIONALs.
    pub matrix: [SRational; 9],
    pub provenance: IdtProvenance,
    pub response: Option<IdtResponse>,
}

/// What a paste did, for the caller to surface. Undo is [`IdtClip::unpaste`]: the original IFD0 offset and file length are in the file's XMP.
#[derive(Debug)]
pub struct PasteReport {
    pub target: PathBuf,
    /// The pairing string written to both signature tags.
    pub signature: String,
    pub ifd0_was: u32,
    pub ifd0_now: u32,
    /// Bytes appended (the new IFD0 and its data).
    pub appended: u64,
    /// The target also carried ColorMatrix2 and got a CameraCalibration2 to match.
    pub cm2_also: bool,
    /// Fingerprint fields that did NOT match and were overridden by `force` (empty for a clean paste).
    pub forced_over: Vec<&'static str>,
    /// Set when the target was a VSF visual: the paste was appended as this generation (undo = `--unpaste-idt`, which truncates it).
    pub vsf_generation: Option<u32>,
}

impl std::fmt::Display for PasteReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(g) = self.vsf_generation {
            write!(f, "{}: IDT pasted as a characterization in generation {g} (\"{}\", +{} bytes appended; opsin --unpaste-idt truncates it)", self.target.display(), self.signature, self.appended)?;
            if !self.forced_over.is_empty() {
                write!(f, " — WARNING: forced over mismatched {}", self.forced_over.join(", "))?;
            }
            return Ok(());
        }
        write!(
            f,
            "{}: IDT pasted as CameraCalibration1{} (\"{}\"), IFD0 {} → {} (+{} bytes appended; the original is intact — opsin --unpaste-idt restores it)",
            self.target.display(),
            if self.cm2_also { "+2" } else { "" },
            self.signature,
            self.ifd0_was,
            self.ifd0_now,
            self.appended
        )?;
        if !self.forced_over.is_empty() {
            write!(f, " — WARNING: forced over mismatched {}", self.forced_over.join(", "))?;
        }
        Ok(())
    }
}

use crate::tiff::{self, srational9, u16e, FrameMeta, TAG_CC1, TAG_CC2, TAG_CC_SIGNATURE, TAG_PROFILE_CAL_SIGNATURE, TAG_XMP, TYPE_ASCII, TYPE_BYTE, TYPE_SRATIONAL};

/// The fingerprint a VSF visual declares: its capture's make/model and focal, its primary plane's dims and CFA tile.
fn fingerprint_of_visual(v: &vsf::visual::Visual) -> Result<Fingerprint, String> {
    let p = v.primary_plane().ok_or("visual has no plane")?;
    let (cfa_h, cfa_w, cfa) = match &p.layout {
        vsf::spectral_image::PlaneLayout::Mosaic { cfa } => (cfa.shape[0] as u16, cfa.shape[1] as u16, cfa.data.clone()),
        vsf::spectral_image::PlaneLayout::Planar => (0, 0, Vec::new()),
    };
    let cap = v.capture.clone().unwrap_or_default();
    // EXIF focal as an unsigned rational: a VSF carries it as f32, so 1000ths — the cross-multiplying compare makes 69/10 and 6900/1000 agree.
    let focal = cap.focal_mm.map(|f| ((f * 1000.0).round() as u32, 1000u32));
    Ok(Fingerprint { make: cap.make.clone(), model: cap.model.clone(), width: p.width, height: p.height, cfa_w, cfa_h, cfa, focal })
}

/// The fingerprint gate shared by every paste (Nick 2026-10-10). Make and model match → paste, no warning: dims, CFA phase and focal length are what one sensor legitimately changes across crop modes, lenses and slitscan rings, and the response is a property of the sensor. Make or model differ → refuse, unless `force` (`Ctrl+Shift+V`, `--force`), and then the report carries the warning. Channel count differs (a Bayer IDT onto a Leica Monochrom, a seven-channel camera) → refuse always: an IDT for K channels has no meaning on K′, and scaling anything off a single patch without the spectral maths would be doing it wrong. Returns the fields overridden by force (empty for a clean paste).
fn gate(clip: &Fingerprint, target: &Path, fp: &Fingerprint, force: bool) -> Result<Vec<&'static str>, String> {
    let (kc, kt) = (clip.channels(), fp.channels());
    if kc != kt {
        return Err(format!("{}: the copied IDT is for a {kc}-channel camera and this frame has {kt} channels (target cfa {}×{} {:?}; clip cfa {}×{} {:?}) — no IDT crosses a channel count", target.display(), fp.cfa_w, fp.cfa_h, fp.cfa, clip.cfa_w, clip.cfa_h, clip.cfa));
    }
    let diff = fp.diff(clip);
    let camera = diff.iter().any(|f| *f == "make" || *f == "model");
    if !camera {
        return Ok(Vec::new());
    }
    if !force {
        return Err(format!(
            "{}: a different camera from the copied IDT — differs in {} (target {}/{} {}×{} cfa {}×{} {:?} focal {}; clip {}/{} {}×{} cfa {}×{} {:?} focal {}). Pass --force (Ctrl+Shift+V) to paste anyway; the report will say so",
            target.display(),
            diff.join(", "),
            fp.make, fp.model, fp.width, fp.height, fp.cfa_w, fp.cfa_h, fp.cfa, fp.focal_text(),
            clip.make, clip.model, clip.width, clip.height, clip.cfa_w, clip.cfa_h, clip.cfa, clip.focal_text(),
        ));
    }
    Ok(diff)
}

fn fingerprint_of_dng(path: &Path, focal: Option<(u32, u32)>) -> Result<Fingerprint, String> {
    let info = limbus::read_metadata(path).ok_or_else(|| format!("{}: unable to read DNG metadata", path.display()))?;
    Ok(Fingerprint {
        make: info.make.trim_end_matches('\0').trim().to_string(),
        model: info.model.trim_end_matches('\0').trim().to_string(),
        width: info.width,
        height: info.height,
        cfa_w: info.cfaw,
        cfa_h: info.cfah,
        cfa: info.cfa,
        focal,
    })
}

/// f64 → SRATIONAL over 2^24: 24 bits of fraction, exact for every f32 and |v| < 128 — every colour matrix coefficient by a wide margin.
fn rational_of_f64(v: f64) -> SRational {
    const DEN: i32 = 1 << 24;
    ((v * DEN as f64).round() as i32, DEN)
}

fn f64_of(m: &[SRational; 9]) -> [f64; 9] {
    let mut o = [0f64; 9];
    for (o, &(n, d)) in o.iter_mut().zip(m) {
        *o = if d == 0 { 0. } else { n as f64 / d as f64 };
    }
    o
}

fn mul3(a: &[f64; 9], b: &[f64; 9]) -> [f64; 9] {
    let mut o = [0f64; 9];
    for r in 0..3 {
        for c in 0..3 {
            o[r * 3 + c] = a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
        }
    }
    o
}

fn inv3(m: &[f64; 9]) -> Option<[f64; 9]> {
    let c = [
        m[4] * m[8] - m[5] * m[7],
        m[5] * m[6] - m[3] * m[8],
        m[3] * m[7] - m[4] * m[6],
        m[2] * m[7] - m[1] * m[8],
        m[0] * m[8] - m[2] * m[6],
        m[1] * m[6] - m[0] * m[7],
        m[1] * m[5] - m[2] * m[4],
        m[2] * m[3] - m[0] * m[5],
        m[0] * m[4] - m[1] * m[3],
    ];
    let det = m[0] * c[0] + m[1] * c[1] + m[2] * c[2];
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    Some([c[0] / det, c[3] / det, c[6] / det, c[1] / det, c[4] / det, c[7] / det, c[2] / det, c[5] / det, c[8] / det])
}

/// The CameraCalibration that makes `CC × CM` come out as `idt`: `idt × CM⁻¹`.
fn calibration_for(idt: &[f64; 9], cm: &[f64; 9]) -> Result<[SRational; 9], String> {
    let inv = inv3(cm).ok_or("the file's ColorMatrix is singular — nothing can be multiplied onto it")?;
    let cc = mul3(idt, &inv);
    let mut out = [(0, 0); 9];
    for (o, &v) in out.iter_mut().zip(&cc) {
        if !v.is_finite() || v.abs() >= 127. {
            return Err(format!("CameraCalibration coefficient {v} out of range — the file's ColorMatrix is degenerate"));
        }
        *o = rational_of_f64(v);
    }
    Ok(out)
}

/// Now, as RFC 3339 UTC to the second — no dependency, civil-from-days (Howard Hinnant's algorithm).
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `verichrome:Name="…"` out of an XMP packet, unescaped; `None` when absent.
pub fn xmp_attr(xmp: &str, name: &str) -> Option<String> {
    let key = format!("verichrome:{name}=\"");
    let i = xmp.find(&key)? + key.len();
    let j = xmp[i..].find('"')? + i;
    Some(xml_unescape(&xmp[i..j]))
}

/// The `rdf:Description` a paste contributes: the IDT's claim, provenance, the nine exact rationals, and the undo coordinates.
fn description(clip: &IdtClip, signature: &str, orig_ifd0: u32, orig_len: u64, pasted: &str) -> String {
    let p = &clip.provenance;
    let matrix: Vec<String> = clip.matrix.iter().map(|(n, d)| format!("{n}/{d}")).collect();
    format!(
        "<rdf:Description rdf:about=\"\" xmlns:verichrome=\"{VERICHROME_NS}\"\n   verichrome:IdtClass=\"{}\"\n   verichrome:IdtTier=\"{}\"\n   verichrome:Observer=\"{}\"\n   verichrome:Illuminant=\"{}\"\n   verichrome:Matrix=\"{}\"\n   verichrome:TargetType=\"{}\"\n   verichrome:TargetSerial=\"{}\"\n   verichrome:TargetCalibrated=\"{}\"\n   verichrome:Scanned=\"{}\"\n   verichrome:Source=\"{}\"\n   verichrome:Signature=\"{}\"\n   verichrome:Pasted=\"{}\"\n   verichrome:OriginalIFD0=\"{}\"\n   verichrome:OriginalLength=\"{}\"",
        xml_escape(&p.class),
        xml_escape(&p.tier),
        xml_escape(&p.observer),
        clip.illuminant,
        matrix.join(" "),
        p.target_type,
        p.target_serial,
        xml_escape(&p.target_calibrated),
        xml_escape(&p.scanned),
        xml_escape(&clip.source),
        xml_escape(signature),
        pasted,
        orig_ifd0,
        orig_len
    ) + &clip.response.as_ref().map(|r| r.xmp_attrs()).unwrap_or_default()
        + "/>"
}

/// The description placed in a packet: into the file's existing XMP before `</rdf:RDF>` (replacing an earlier verichrome description if there is one — everything else byte for byte), else a fresh packet. Trailing whitespace padding is the XMP convention that lets later editors rewrite in place.
fn xmp_with(existing: Option<&[u8]>, desc: &str) -> Vec<u8> {
    const PAD: usize = 2048;
    if let Some(bytes) = existing {
        if let Ok(text) = std::str::from_utf8(bytes) {
            let mut out = text.to_string();
            // Replace an earlier verichrome description (self-closing, as this module writes it) in place.
            if let Some(i) = out.find(&format!("xmlns:verichrome=\"{VERICHROME_NS}\"")) {
                if let (Some(start), Some(end)) = (out[..i].rfind("<rdf:Description"), out[i..].find("/>").map(|e| i + e + 2)) {
                    out.replace_range(start..end, desc);
                    return out.into_bytes();
                }
            }
            if let Some(i) = out.find("</rdf:RDF>") {
                out.insert_str(i, &format!("{desc}\n"));
                return out.into_bytes();
            }
        }
    }
    let mut s = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  {desc}\n </rdf:RDF>\n</x:xmpmeta>\n"
    );
    s.extend(std::iter::repeat(' ').take(PAD));
    s.push_str("\n<?xpacket end=\"w\"?>");
    s.into_bytes()
}

/// chameleon's own signature (DNGs it writes named "Verichrome scene-relative IDT"), or an in-place paste from before the XMP era: one matrix in every ColorMatrix slot at D50 — a factory pair never measures the same matrix under two illuminants.
fn looks_like_chameleon(tags: &FrameMeta, cm1: &[SRational; 9], cm2: Option<&[SRational; 9]>) -> bool {
    tags.profile_name.as_deref().is_some_and(|n| n.to_ascii_lowercase().contains("verichrome"))
        || match cm2 {
            Some(b) => cm1 == b,
            None => tags.illuminant1() == 23 && cm1.iter().any(|&(n, d)| n != d && n != 0),
        }
}

impl IdtClip {
    /// Lift the IDT out of `path`: a DNG — the nine exact rationals and provenance from a VERICHROME XMP packet when the file carries one, else `ColorMatrix1` verbatim — or a VSF-Image carrying a verbatim DNG matrix in its colour_profile.
    pub fn copy_from(path: &Path) -> Result<IdtClip, String> {
        if crate::sniff::sniff_path(path) == Some(crate::sniff::Kind::Vsf) {
            return Self::copy_from_vsf(path);
        }
        let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let tags = FrameMeta::read(&mut f)?;
        let fingerprint = fingerprint_of_dng(path, tags.focal)?;
        let source = path.display().to_string();
        if let Some(xmp) = tags.xmp.as_deref().and_then(|b| std::str::from_utf8(b).ok()) {
            if let Some(m) = xmp_attr(xmp, "Matrix") {
                let words: Vec<&str> = m.split_whitespace().collect();
                if words.len() == 9 {
                    let mut matrix = [(0, 0); 9];
                    for (r, w) in matrix.iter_mut().zip(&words) {
                        let (n, d) = w.split_once('/').ok_or_else(|| format!("{}: bad verichrome:Matrix '{m}'", path.display()))?;
                        *r = (n.parse().map_err(|_| "bad matrix numerator")?, d.parse().map_err(|_| "bad matrix denominator")?);
                    }
                    let get = |k: &str| xmp_attr(xmp, k).unwrap_or_default();
                    let provenance = IdtProvenance {
                        class: get("IdtClass"),
                        tier: get("IdtTier"),
                        observer: get("Observer"),
                        target_type: get("TargetType").parse().unwrap_or(0),
                        target_serial: get("TargetSerial").parse().unwrap_or(0),
                        target_calibrated: get("TargetCalibrated"),
                        scanned: get("Scanned"),
                    };
                    let illuminant = get("Illuminant").parse().unwrap_or(23);
                    return Ok(IdtClip { source, fingerprint, illuminant, matrix, provenance, response: IdtResponse::from_xmp(xmp) });
                }
            }
        }
        let cm1 = tags.cm1.ok_or_else(|| format!("{}: no ColorMatrix1", path.display()))?;
        let matrix = srational9(&mut f, &cm1, tags.be)?;
        let cm2 = tags.cm2.and_then(|e| srational9(&mut f, &e, tags.be).ok());
        let provenance = if looks_like_chameleon(&tags, &matrix, cm2.as_ref()) { IdtProvenance::chameleon() } else { IdtProvenance::factory() };
        Ok(IdtClip { source, fingerprint, illuminant: tags.illuminant1(), matrix, provenance, response: None })
    }

    fn copy_from_vsf(path: &Path) -> Result<IdtClip, String> {
        let v = vsf::visual::Visual::read_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let profile = v.characterization.as_ref().ok_or_else(|| format!("{}: no characterization", path.display()))?;
        let (m, illuminant) = profile.dng_colormatrix[0].ok_or_else(|| format!("{}: characterization carries no verbatim XYZ→camera matrix", path.display()))?;
        let fingerprint = fingerprint_of_visual(&v)?;
        let mut matrix = [(0, 0); 9];
        for (r, &x) in matrix.iter_mut().zip(&m) {
            *r = rational_of_f64(x as f64);
        }
        // The entry's own class and tier, as the file states them; a pasted IDT's provenance rides in `cal`.
        let provenance = match profile.entries.first() {
            Some(e) => IdtProvenance {
                class: e.class.as_str().to_string(),
                tier: e.tier.as_str().to_string(),
                observer: v.observer.clone(),
                target_type: profile.cal.as_ref().map_or(0, |c| c.target_type),
                target_serial: profile.cal.as_ref().map_or(0, |c| c.target_serial),
                target_calibrated: profile.cal.as_ref().map_or(String::new(), |c| c.timestamp.clone()),
                scanned: String::new(),
            },
            None => IdtProvenance::factory(),
        };
        Ok(IdtClip { source: path.display().to_string(), fingerprint, illuminant, matrix, provenance, response: v.response.as_ref().map(response_of_vsf) })
    }

    /// Paste into a VSF visual: a generation appended to the file carrying a `characterization` with this IDT as its first entry (the file's own entries follow), the IDT's XYZ→camera matrix in the verbatim slot so a copy out is exact, the target provenance in `cal`, and the pairing string in `foreign`. Nothing original moves; undo is truncation.
    fn paste_into_vsf(&self, target: &Path, force: bool) -> Result<PasteReport, String> {
        let f = File::open(target).map_err(|e| format!("{}: {e}", target.display()))?;
        let doc = vsf::container::Document::open(f).map_err(|e| e.to_string())?;
        let vis = vsf::visual::Visual::read_document(&doc).map_err(|e| format!("{}: {e}", target.display()))?;
        let diff = gate(&self.fingerprint, target, &fingerprint_of_visual(&vis)?, force)?;
        let p = &self.provenance;
        let m: [f32; 9] = f64_of(&self.matrix).map(|x| x as f32);
        let mut entry = crate::convert::derive_profile(m, self.illuminant, "verichrome_paste").ok_or("the IDT matrix is singular")?;
        entry.class = if p.class == "absolute" { vsf::spectral_image::IdtClass::Absolute } else { vsf::spectral_image::IdtClass::Relative };
        entry.tier = if p.tier == "model" { vsf::spectral_image::ProfileTier::Model } else { vsf::spectral_image::ProfileTier::Unit };
        let mut profile = vis.characterization.clone().unwrap_or(vsf::spectral_image::ColourProfile { target: "vsf_rgb".to_string(), entries: Vec::new(), dng_colormatrix: [None, None], patches: None, cal: None });
        profile.entries.retain(|e| e.source != "verichrome_paste");
        profile.entries.insert(0, entry);
        profile.dng_colormatrix[0] = Some((m, self.illuminant));
        if p.target_serial != 0 || p.target_type != 0 {
            profile.cal = Some(vsf::spectral_image::CalProvenance { target_type: p.target_type, target_serial: p.target_serial, timestamp: p.target_calibrated.clone() });
        }
        let observer = if p.observer.is_empty() { vis.observer.clone() } else { p.observer.clone() };
        let signature = format!("VERICHROME {} {} target#{} {}", p.class, p.tier, p.target_serial, if p.scanned.is_empty() { "-" } else { p.scanned.as_str() });
        let mut foreign = vis.foreign.clone();
        foreign.calibration_signature = signature.clone();
        let add = vsf::container::Appender::new("opsin", "paste idt")
            .add_section("characterization", vsf::visual::characterization_fields(&profile, &observer, self.response.as_ref().map(response_to_vsf).as_ref()))
            .add_section("foreign", foreign.fields())
            .supersede("colour_profile")
            .build(&doc)
            .map_err(|e| e.to_string())?;
        let generation = doc.generation() + 1;
        drop(doc);
        vsf::container::append_to_file(target, &add).map_err(|e| e.to_string())?;
        Ok(PasteReport { target: target.to_path_buf(), signature, ifd0_was: 0, ifd0_now: 0, appended: add.len() as u64, cm2_also: false, forced_over: diff, vsf_generation: Some(generation) })
    }

    /// Write this IDT into `target` (a DNG) — after the fingerprint gate. Without `force`, ANY differing field refuses, naming the fields and what each side declares; the message distinguishes the two tiers so the user knows what they'd be overriding. **Soft** = focal length alone: on an interchangeable-lens body (a Sony) the sensor — and so the IDT — survives a lens change, so this is the case `force` exists for; on a phone the same mismatch means a different camera module. **Hard** = make/model/sensor dims/CFA: almost certainly a different sensor. `force` pastes through either tier — user's call — and the report carries the overridden fields as a warning so the decision is on record.
    ///
    /// The write is the appended-IFD0 scheme in the module doc: `CameraCalibration1/2` = `IDT × ColorMatrix_i⁻¹`, both signature tags, and the `verichrome:` XMP — in a new IFD0 at the end of the file, the header repointed, nothing else touched.
    pub fn paste_into(&self, target: &Path, force: bool) -> Result<PasteReport, String> {
        if crate::sniff::sniff_path(target) == Some(crate::sniff::Kind::Vsf) {
            return self.paste_into_vsf(target, force);
        }
        let mut f = File::options().read(true).write(true).open(target).map_err(|e| format!("{}: {e}", target.display()))?;
        let tags = FrameMeta::read(&mut f)?;
        let fp = fingerprint_of_dng(target, tags.focal)?;
        // Soft = fields the SAME sensor can legitimately change: focal (a lens change on an interchangeable-lens body) and raw dims (a crop mode; lumis's slitscan ring at 2× height). Hard = make/model/CFA — a different tile or a different camera name is a different sensor.
        let diff = gate(&self.fingerprint, target, &fp, force)?;
        let be = tags.be;
        let cm1 = tags.cm1.ok_or_else(|| format!("{}: no ColorMatrix1 to calibrate onto", target.display()))?;
        let idt = f64_of(&self.matrix);
        let cc1 = calibration_for(&idt, &f64_of(&srational9(&mut f, &cm1, be)?))?;
        let cc2 = match &tags.cm2 {
            Some(e) => Some(calibration_for(&idt, &f64_of(&srational9(&mut f, e, be)?))?),
            None => None,
        };
        let p = &self.provenance;
        let signature = format!("VERICHROME {} {} target#{} {}", p.class, p.tier, p.target_serial, if p.scanned.is_empty() { "-" } else { p.scanned.as_str() });
        // Undo coordinates: a re-paste carries the FIRST paste's originals forward, so unpaste always lands on the untouched file.
        let file_len = f.metadata().map_err(|e| e.to_string())?.len();
        let existing = tags.xmp.as_deref().and_then(|b| std::str::from_utf8(b).ok());
        let (orig_ifd0, orig_len) = match existing.and_then(|x| Some((xmp_attr(x, "OriginalIFD0")?.parse::<u32>().ok()?, xmp_attr(x, "OriginalLength")?.parse::<u64>().ok()?))) {
            Some(o) => o,
            None => (tags.ifd0 as u32, file_len),
        };
        let desc = description(self, &signature, orig_ifd0, orig_len, &now_rfc3339());
        let xmp = xmp_with(tags.xmp.as_deref(), &desc);

        // The new IFD0: every entry the file had, minus the ones this paste owns, plus ours — sorted by tag as TIFF requires. Out-of-line data for the new entries follows the IFD.
        let (raw, next) = tiff::read_ifd_raw(&mut f, tags.ifd0, be)?;
        let ours = [TAG_CC1, TAG_CC2, TAG_CC_SIGNATURE, TAG_PROFILE_CAL_SIGNATURE, TAG_XMP];
        let mut entries: Vec<(u16, [u8; 12])> = raw.into_iter().map(|e| (u16e(&e[0..2], be), e)).filter(|(t, _)| !ours.contains(t)).collect();
        let mut blobs: Vec<(usize, Vec<u8>)> = Vec::new(); // (index into entries, data) — offsets filled once the IFD's size is known
        let rat9 = |m: &[SRational; 9]| {
            let mut v = Vec::with_capacity(72);
            for &(n, d) in m {
                tiff::put_i32(&mut v, n, be);
                tiff::put_i32(&mut v, d, be);
            }
            v
        };
        let mut sig = signature.clone().into_bytes();
        sig.push(0);
        let mut add = |tag: u16, ty: u16, data: Vec<u8>| {
            let mut e = [0u8; 12];
            e[0..2].copy_from_slice(&if be { tag.to_be_bytes() } else { tag.to_le_bytes() });
            e[2..4].copy_from_slice(&if be { ty.to_be_bytes() } else { ty.to_le_bytes() });
            let count = if ty == TYPE_SRATIONAL { 9u32 } else { data.len() as u32 };
            e[4..8].copy_from_slice(&if be { count.to_be_bytes() } else { count.to_le_bytes() });
            entries.push((tag, e));
            if data.len() <= 4 {
                let i = entries.len() - 1;
                entries[i].1[8..8 + data.len()].copy_from_slice(&data);
            } else {
                blobs.push((entries.len() - 1, data));
            }
        };
        add(TAG_CC1, TYPE_SRATIONAL, rat9(&cc1));
        if let Some(cc2) = &cc2 {
            add(TAG_CC2, TYPE_SRATIONAL, rat9(cc2));
        }
        add(TAG_CC_SIGNATURE, TYPE_ASCII, sig.clone());
        add(TAG_PROFILE_CAL_SIGNATURE, TYPE_ASCII, sig);
        add(TAG_XMP, TYPE_BYTE, xmp);
        // Sort by tag; blobs index entries by position, so remap thru the sort.
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by_key(|&i| entries[i].0);
        let pos_of: Vec<usize> = { let mut p = vec![0; order.len()]; for (new, &old) in order.iter().enumerate() { p[old] = new; } p };
        let mut sorted: Vec<[u8; 12]> = order.iter().map(|&i| entries[i].1).collect();
        let n = sorted.len();
        let ifd_off = (file_len + 1) & !1; // IFDs sit on an even offset
        if ifd_off + 2 + 12 * n as u64 + 4 > u32::MAX as u64 {
            return Err(format!("{}: file too large for a classic-TIFF IFD offset", target.display()));
        }
        let mut data = Vec::new();
        for (i, blob) in blobs {
            let off = ifd_off + 2 + 12 * n as u64 + 4 + data.len() as u64;
            sorted[pos_of[i]][8..12].copy_from_slice(&if be { (off as u32).to_be_bytes() } else { (off as u32).to_le_bytes() });
            data.extend(blob);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
        let mut out = Vec::with_capacity(2 + 12 * n + 4 + data.len());
        out.extend(if be { (n as u16).to_be_bytes() } else { (n as u16).to_le_bytes() });
        for e in &sorted {
            out.extend_from_slice(e);
        }
        out.extend(if be { next.to_be_bytes() } else { next.to_le_bytes() });
        out.extend(data);
        // Append, then repoint the header — in that order, so a failure mid-append leaves a file whose header still names the old, intact IFD0.
        f.seek(SeekFrom::Start(file_len)).map_err(|e| e.to_string())?;
        if ifd_off > file_len {
            f.write_all(&[0]).map_err(|e| e.to_string())?;
        }
        f.write_all(&out).map_err(|e| e.to_string())?;
        f.seek(SeekFrom::Start(4)).map_err(|e| e.to_string())?;
        f.write_all(&if be { (ifd_off as u32).to_be_bytes() } else { (ifd_off as u32).to_le_bytes() }).map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())?;
        Ok(PasteReport { target: target.to_path_buf(), signature, ifd0_was: tags.ifd0 as u32, ifd0_now: ifd_off as u32, appended: ifd_off + out.len() as u64 - file_len, cm2_also: cc2.is_some(), forced_over: diff, vsf_generation: None })
    }

    /// Undo every VERICHROME paste on `target`: point the header back at the original IFD0 and truncate to the original length, both recorded in the paste's XMP. Byte for byte the file it was.
    pub fn unpaste(target: &Path) -> Result<String, String> {
        if crate::sniff::sniff_path(target) == Some(crate::sniff::Kind::Vsf) {
            let f = File::open(target).map_err(|e| format!("{}: {e}", target.display()))?;
            let doc = vsf::container::Document::open(f).map_err(|e| e.to_string())?;
            let gens = doc.generations().map_err(|e| e.to_string())?;
            let last = gens.last().ok_or("empty history")?;
            if last.index == 0 {
                return Err(format!("{}: no paste to undo (generation 0)", target.display()));
            }
            if last.action != "paste idt" {
                return Err(format!("{}: the latest generation is '{}' by {}, not a paste — undo it with opsin's history, not --unpaste-idt", target.display(), last.action, last.tool));
            }
            let keep = gens[gens.len() - 2].end;
            let dropped = doc.end() - keep;
            drop(doc);
            vsf::container::truncate_file(target, keep).map_err(|e| e.to_string())?;
            return Ok(format!("{}: paste undone — generation {} dropped ({dropped} bytes), back to generation {}", target.display(), last.index, last.index - 1));
        }
        let mut f = File::options().read(true).write(true).open(target).map_err(|e| format!("{}: {e}", target.display()))?;
        let tags = FrameMeta::read(&mut f)?;
        let xmp = tags.xmp.as_deref().and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| format!("{}: no VERICHROME paste to undo (no XMP)", target.display()))?;
        let orig_ifd0: u32 = xmp_attr(xmp, "OriginalIFD0").and_then(|v| v.parse().ok()).ok_or_else(|| format!("{}: no VERICHROME paste to undo", target.display()))?;
        let orig_len: u64 = xmp_attr(xmp, "OriginalLength").and_then(|v| v.parse().ok()).ok_or_else(|| format!("{}: paste record carries no original length", target.display()))?;
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        if orig_len > len || orig_ifd0 as u64 >= orig_len {
            return Err(format!("{}: paste record is inconsistent with the file (original {orig_len} bytes, IFD0 at {orig_ifd0}; file is {len})", target.display()));
        }
        f.seek(SeekFrom::Start(4)).map_err(|e| e.to_string())?;
        f.write_all(&if tags.be { orig_ifd0.to_be_bytes() } else { orig_ifd0.to_le_bytes() }).map_err(|e| e.to_string())?;
        f.set_len(orig_len).map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())?;
        Ok(format!("{}: paste undone — IFD0 back at {orig_ifd0}, {} appended bytes dropped", target.display(), len - orig_len))
    }

    /// The readable clip form — one field per line, exact rationals, no floats.
    pub fn to_text(&self) -> String {
        let fp = &self.fingerprint;
        let p = &self.provenance;
        let mut s = String::new();
        s.push_str("opsin-idt 3\n");
        s.push_str(&format!("source {}\n", self.source));
        s.push_str(&format!("make {}\n", fp.make));
        s.push_str(&format!("model {}\n", fp.model));
        s.push_str(&format!("sensor {} {}\n", fp.width, fp.height));
        s.push_str(&format!("cfa {} {}", fp.cfa_w, fp.cfa_h));
        for c in &fp.cfa {
            s.push_str(&format!(" {c}"));
        }
        s.push('\n');
        s.push_str(&format!("focal {}\n", fp.focal_text()));
        s.push_str(&format!("illuminant {}\n", self.illuminant));
        s.push_str(&format!("class {}\n", p.class));
        s.push_str(&format!("tier {}\n", p.tier));
        s.push_str(&format!("observer {}\n", p.observer));
        s.push_str(&format!("target {} {}\n", p.target_type, p.target_serial));
        s.push_str(&format!("calibrated {}\n", p.target_calibrated));
        s.push_str(&format!("scanned {}\n", p.scanned));
        s.push_str("matrix");
        for (n, d) in self.matrix {
            s.push_str(&format!(" {n}/{d}"));
        }
        s.push('\n');
        if let Some(r) = &self.response {
            // The response: grid, prior, light, then one line per camera channel.
            s.push_str(&format!("response_grid {} {} {}\n", r.start_nm, r.step_nm, r.channels.first().map_or(0, Vec::len)));
            s.push_str(&format!("response_prior {}\n", r.prior));
            s.push_str(&format!("response_light {}\n", r.light.map(|l| hex32(&l)).unwrap_or_else(|| "-".into())));
            for c in &r.channels {
                s.push_str("response");
                for v in c {
                    s.push_str(&format!(" {v:.6e}"));
                }
                s.push('\n');
            }
        }
        s
    }

    /// Parse a clip. `opsin-idt 1` (before provenance) reads as a factory `absolute`/`model` IDT.
    pub fn from_text(text: &str) -> Result<IdtClip, String> {
        let mut lines = text.lines();
        let version = match lines.next().map(str::trim) {
            Some("opsin-idt 1") => 1,
            Some("opsin-idt 2") => 2,
            Some("opsin-idt 3") => 3,
            _ => return Err("not an opsin-idt clip".to_string()),
        };
        let mut source = String::new();
        let mut fp = Fingerprint { make: String::new(), model: String::new(), width: 0, height: 0, cfa_w: 0, cfa_h: 0, cfa: Vec::new(), focal: None };
        let mut illuminant = 0u16;
        let mut matrix: Option<[SRational; 9]> = None;
        let mut p = IdtProvenance::factory();
        let _ = version;
        let mut grid: Option<(f32, f32, usize)> = None;
        let mut channels: Vec<Vec<f32>> = Vec::new();
        let mut response_prior = String::new();
        let mut response_light: Option<[u8; 32]> = None;
        let rat = |s: &str| -> Result<(i64, i64), String> {
            let (n, d) = s.split_once('/').ok_or_else(|| format!("bad rational '{s}'"))?;
            Ok((n.parse().map_err(|_| format!("bad rational '{s}'"))?, d.parse().map_err(|_| format!("bad rational '{s}'"))?))
        };
        for line in lines {
            let (key, rest) = match line.trim_end().split_once(' ') {
                Some(kv) => kv,
                None => (line.trim_end(), ""),
            };
            let words: Vec<&str> = rest.split_whitespace().collect();
            match key {
                "source" => source = rest.to_string(),
                "make" => fp.make = rest.to_string(),
                "model" => fp.model = rest.to_string(),
                "sensor" => {
                    if words.len() != 2 {
                        return Err("sensor needs width height".to_string());
                    }
                    fp.width = words[0].parse().map_err(|_| "bad sensor width")?;
                    fp.height = words[1].parse().map_err(|_| "bad sensor height")?;
                }
                "cfa" => {
                    if words.len() < 2 {
                        return Err("cfa needs w h cells...".to_string());
                    }
                    fp.cfa_w = words[0].parse().map_err(|_| "bad cfa w")?;
                    fp.cfa_h = words[1].parse().map_err(|_| "bad cfa h")?;
                    fp.cfa = words[2..].iter().map(|w| w.parse().map_err(|_| "bad cfa cell".to_string())).collect::<Result<_, _>>()?;
                }
                "focal" => {
                    fp.focal = if rest.trim() == "-" {
                        None
                    } else {
                        let (n, d) = rat(rest.trim())?;
                        Some((n as u32, d as u32))
                    };
                }
                "illuminant" => illuminant = rest.trim().parse().map_err(|_| "bad illuminant")?,
                "class" => p.class = rest.trim().to_string(),
                "tier" => p.tier = rest.trim().to_string(),
                "observer" => p.observer = rest.trim().to_string(),
                "target" => {
                    if words.len() != 2 {
                        return Err("target needs type serial".to_string());
                    }
                    p.target_type = words[0].parse().map_err(|_| "bad target type")?;
                    p.target_serial = words[1].parse().map_err(|_| "bad target serial")?;
                }
                "calibrated" => p.target_calibrated = rest.trim().to_string(),
                "scanned" => p.scanned = rest.trim().to_string(),
                "response_grid" => {
                    if words.len() != 3 {
                        return Err("response_grid needs start step count".to_string());
                    }
                    grid = Some((words[0].parse().map_err(|_| "bad response start")?, words[1].parse().map_err(|_| "bad response step")?, words[2].parse().map_err(|_| "bad response count")?));
                }
                "response_prior" => response_prior = rest.trim().to_string(),
                "response_light" => response_light = unhex32(rest),
                "response" => channels.push(words.iter().map(|w| w.parse::<f32>().map_err(|_| format!("bad response value '{w}'"))).collect::<Result<_, _>>()?),
                "matrix" => {
                    if words.len() != 9 {
                        return Err(format!("matrix needs 9 rationals, got {}", words.len()));
                    }
                    let mut m = [(0, 0); 9];
                    for (r, w) in m.iter_mut().zip(&words) {
                        let (n, d) = rat(w)?;
                        *r = (n as i32, d as i32);
                    }
                    matrix = Some(m);
                }
                "" => {}
                other => return Err(format!("unknown clip field '{other}'")),
            }
        }
        let matrix = matrix.ok_or("clip has no matrix")?;
        if fp.cfa.len() != fp.cfa_w as usize * fp.cfa_h as usize {
            return Err("cfa cells don't match cfa dims".to_string());
        }
        let response = match (grid, channels.is_empty()) {
            (Some((start_nm, step_nm, n)), false) => {
                if channels.iter().any(|c| c.len() != n) {
                    return Err("response channels do not match response_grid count".to_string());
                }
                Some(IdtResponse { start_nm, step_nm, channels, prior: response_prior, light: response_light })
            }
            _ => None,
        };
        Ok(IdtClip { source, fingerprint: fp, illuminant, matrix, provenance: p, response })
    }

    /// Where the clip persists: `$XDG_CONFIG_HOME/opsin/idt.clip`, else `~/.config/opsin/idt.clip`.
    pub fn clip_path() -> Result<PathBuf, String> {
        let base = match std::env::var_os("XDG_CONFIG_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x),
            _ => PathBuf::from(std::env::var_os("HOME").ok_or("no HOME")?).join(".config"),
        };
        Ok(base.join("opsin").join("idt.clip"))
    }

    pub fn save(&self) -> Result<PathBuf, String> {
        let p = Self::clip_path()?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&p, self.to_text()).map_err(|e| format!("{}: {e}", p.display()))?;
        Ok(p)
    }

    pub fn load() -> Result<IdtClip, String> {
        let p = Self::clip_path()?;
        let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e} (copy an IDT first)", p.display()))?;
        Self::from_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal little-endian DNG-shaped TIFF: IFD0 with the raw-plane tags limbus needs (NewSubFileType 0, dims, bits, strips, make/model, CFA), ColorMatrix1 (+ optional CM2), CalibrationIlluminant1 (+2), an optional XMP packet, and an EXIF IFD with FocalLength. Returns the bytes.
    fn synth_dng(make: &str, model: &str, w: u32, h: u32, cfa: [u8; 4], focal: (u32, u32), cm1: [SRational; 9], cm2: Option<[SRational; 9]>, xmp: Option<&str>) -> Vec<u8> {
        let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = Vec::new(); // (tag, type, count, value-or-data). Data > 4 bytes goes out of line.
        let asciiz = |s: &str| {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            v
        };
        let le = |v: u32| v.to_le_bytes().to_vec();
        let le16 = |v: u16| v.to_le_bytes().to_vec();
        let rat9 = |m: &[SRational; 9]| {
            let mut v = Vec::new();
            for &(n, d) in m {
                v.extend(n.to_le_bytes());
                v.extend(d.to_le_bytes());
            }
            v
        };
        entries.push((254, 4, 1, le(0)));
        entries.push((256, 4, 1, le(w)));
        entries.push((257, 4, 1, le(h)));
        entries.push((258, 3, 1, le16(16)));
        entries.push((271, 2, make.len() as u32 + 1, asciiz(make)));
        entries.push((272, 2, model.len() as u32 + 1, asciiz(model)));
        entries.push((273, 4, 1, le(0)));
        if let Some(x) = xmp {
            entries.push((700, 1, x.len() as u32, x.as_bytes().to_vec()));
        }
        entries.push((33421, 3, 2, [le16(2), le16(2)].concat()));
        entries.push((33422, 1, 4, cfa.to_vec()));
        entries.push((34665, 4, 1, le(0))); // patched to the EXIF IFD offset
        entries.push((50721, 10, 9, rat9(&cm1)));
        if let Some(m) = &cm2 {
            entries.push((50722, 10, 9, rat9(m)));
        }
        entries.push((50778, 3, 1, le16(if cm2.is_some() { 17 } else { 23 })));
        if cm2.is_some() {
            entries.push((50779, 3, 1, le16(21)));
        }
        // Layout: header(8) | IFD0 (2 + 12n + 4) | out-of-line data | EXIF IFD | focal rational.
        let n = entries.len();
        let ifd0 = 8u32;
        let data_off = ifd0 + 2 + 12 * n as u32 + 4;
        let mut data = Vec::new();
        let mut ifd = Vec::new();
        ifd.extend((n as u16).to_le_bytes());
        let mut exif_ptr_pos = 0usize;
        for (tag, ty, count, val) in &entries {
            ifd.extend(tag.to_le_bytes());
            ifd.extend(ty.to_le_bytes());
            ifd.extend(count.to_le_bytes());
            if *tag == 34665 {
                exif_ptr_pos = ifd.len();
            }
            if val.len() <= 4 {
                let mut v = val.clone();
                v.resize(4, 0);
                ifd.extend(v);
            } else {
                ifd.extend((data_off + data.len() as u32).to_le_bytes());
                data.extend(val);
                if data.len() % 2 == 1 {
                    data.push(0);
                }
            }
        }
        ifd.extend(0u32.to_le_bytes());
        // EXIF IFD after the data block.
        let exif_off = data_off + data.len() as u32;
        let focal_off = exif_off + 2 + 12 + 4;
        ifd[exif_ptr_pos..exif_ptr_pos + 4].copy_from_slice(&exif_off.to_le_bytes());
        let mut exif = Vec::new();
        exif.extend(1u16.to_le_bytes());
        exif.extend(37386u16.to_le_bytes());
        exif.extend(5u16.to_le_bytes());
        exif.extend(1u32.to_le_bytes());
        exif.extend(focal_off.to_le_bytes());
        exif.extend(0u32.to_le_bytes());
        exif.extend(focal.0.to_le_bytes());
        exif.extend(focal.1.to_le_bytes());
        let mut out = vec![b'I', b'I', 42, 0];
        out.extend(ifd0.to_le_bytes());
        out.extend(ifd);
        out.extend(data);
        out.extend(exif);
        out
    }

    fn tmp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("opsin-idt-test-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    const IDENT: [SRational; 9] = [(1, 1), (0, 1), (0, 1), (0, 1), (1, 1), (0, 1), (0, 1), (0, 1), (1, 1)];
    const MAGIC: [SRational; 9] = [(1216135144, 1000000000), (-353578776, 1000000000), (-75642496, 1000000000), (-323607296, 1000000000), (1535650611, 1000000000), (135206401, 1000000000), (-116805404, 1000000000), (427233905, 1000000000), (555407941, 1000000000)];
    /// A Sigma-fp-shaped factory pair.
    const FACTORY_A: [SRational; 9] = [(15311, 10000), (-7263, 10000), (-2355, 10000), (-11733, 10000), (22710, 10000), (503, 10000), (854, 10000), (-3131, 10000), (16227, 10000)];
    const FACTORY_D65: [SRational; 9] = [(15708, 10000), (-7451, 10000), (-2416, 10000), (-9692, 10000), (18760, 10000), (415, 10000), (353, 10000), (-1295, 10000), (6710, 10000)];

    /// A three-channel response on a 5-band grid, one common scale, as a scan would hand over.
    fn sample_response() -> IdtResponse {
        IdtResponse { start_nm: 400.0, step_nm: 80.0, channels: vec![vec![0.0, 0.1, 0.4, 1.0, 0.3], vec![0.1, 0.7, 1.3, 0.5, 0.0], vec![0.9, 1.1, 0.2, 0.0, 0.0]], prior: "shortest".into(), light: Some([7u8; 32]) }
    }

    /// The response rides the DNG paste in the `verichrome:` XMP and comes back on copy, exact (`{:.6e}` is more than f32 carries).
    #[test]
    fn a_pasted_response_rides_the_xmp_and_copies_back() {
        let src = tmp("resp.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), None));
        let mut clip = chameleon_clip(&src);
        clip.response = Some(sample_response());
        clip.paste_into(&src, false).unwrap();
        let back = IdtClip::copy_from(&src).unwrap();
        assert_eq!(back.response, clip.response);
        assert_eq!(back.matrix, MAGIC);
        // A clip without one writes none, and a copy sees none.
        let plain = tmp("plain.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), None));
        chameleon_clip(&plain).paste_into(&plain, false).unwrap();
        assert_eq!(IdtClip::copy_from(&plain).unwrap().response, None);
    }

    fn chameleon_clip(src: &Path) -> IdtClip {
        let mut clip = IdtClip::copy_from(src).unwrap();
        clip.matrix = MAGIC;
        clip.illuminant = 23;
        clip.provenance = IdtProvenance { observer: "CIE/CVRL 2006 2°".into(), target_type: 1, target_serial: 13, target_calibrated: "2026-07-01 UTC".into(), scanned: "2026-10-08T17:00:00Z".into(), ..IdtProvenance::chameleon() };
        clip
    }

    /// `CC × CM` from the file, as f64 — what every DNG reader computes.
    fn effective(path: &Path, slot: u8) -> [f64; 9] {
        let mut f = File::open(path).unwrap();
        let t = FrameMeta::read(&mut f).unwrap();
        let (cc, cm) = if slot == 1 { (t.cc1.unwrap(), t.cm1.unwrap()) } else { (t.cc2.unwrap(), t.cm2.unwrap()) };
        mul3(&f64_of(&srational9(&mut f, &cc, t.be).unwrap()), &f64_of(&srational9(&mut f, &cm, t.be).unwrap()))
    }

    fn close(a: &[f64; 9], b: &[f64; 9]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
    }

    #[test]
    fn copy_reads_verbatim_and_text_round_trips() {
        let src = tmp("src.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None, None));
        let clip = IdtClip::copy_from(&src).unwrap();
        assert_eq!(clip.matrix, MAGIC);
        assert_eq!(clip.illuminant, 23);
        assert_eq!(clip.fingerprint.focal, Some((69, 10)));
        assert_eq!(clip.fingerprint.cfa, vec![1, 2, 0, 1]);
        assert_eq!((clip.fingerprint.width, clip.fingerprint.height), (4080, 3072));
        // A lone non-identity D50 matrix is the old in-place paste's signature: chameleon's.
        assert_eq!(clip.provenance, IdtProvenance::chameleon());
        let back = IdtClip::from_text(&clip.to_text()).unwrap();
        assert_eq!(back, clip);
        // With a response: the curves, prior and light ride the text form exactly.
        let mut with = clip.clone();
        with.response = Some(sample_response());
        let back = IdtClip::from_text(&with.to_text()).unwrap();
        assert_eq!(back, with);
        // A factory pair copies as what it is.
        let fac = tmp("fac.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), None));
        assert_eq!(IdtClip::copy_from(&fac).unwrap().provenance, IdtProvenance::factory());
        // A v1 clip still reads.
        let v1 = clip.to_text().replace("opsin-idt 3", "opsin-idt 1").lines().filter(|l| !l.starts_with("class") && !l.starts_with("tier") && !l.starts_with("observer") && !l.starts_with("target") && !l.starts_with("calibrated") && !l.starts_with("scanned")).collect::<Vec<_>>().join("\n");
        assert_eq!(IdtClip::from_text(&v1).unwrap().matrix, MAGIC);
    }

    /// The paste leaves every original byte in place but the header's IFD0 pointer, appends a sorted IFD0 carrying the calibration, signatures and XMP, and the product CC × CM a reader computes IS the IDT — at both slots.
    #[test]
    fn paste_appends_a_calibration_that_multiplies_onto_the_factory_matrix() {
        let src = tmp("a.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), MAGIC, None, None));
        let before = synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), None);
        let dst = tmp("b.dng", &before);
        let report = chameleon_clip(&src).paste_into(&dst, false).unwrap();
        assert!(report.cm2_also);
        assert!(report.signature.starts_with("VERICHROME relative unit target#13 "), "{}", report.signature);
        let after = std::fs::read(&dst).unwrap();
        // Original bytes intact but the header's pointer; everything new is appended.
        assert!(after.len() > before.len());
        assert_eq!(&after[8..before.len()], &before[8..]);
        assert_eq!(&after[..4], &before[..4]);
        assert_eq!(u32::from_le_bytes(after[4..8].try_into().unwrap()), report.ifd0_now);
        assert_eq!(report.ifd0_was, 8);
        // The appended IFD0: sorted tags, factory matrices untouched, calibration present, signatures equal.
        let mut f = File::open(&dst).unwrap();
        let t = FrameMeta::read(&mut f).unwrap();
        let (raw, _) = tiff::read_ifd_raw(&mut f, t.ifd0, false).unwrap();
        let tags: Vec<u16> = raw.iter().map(|e| u16::from_le_bytes([e[0], e[1]])).collect();
        assert!(tags.windows(2).all(|w| w[0] < w[1]), "IFD0 tags not ascending: {tags:?}");
        assert_eq!(srational9(&mut f, &t.cm1.unwrap(), false).unwrap(), FACTORY_A);
        assert_eq!(srational9(&mut f, &t.cm2.unwrap(), false).unwrap(), FACTORY_D65);
        assert_eq!(t.cc_signature.as_deref(), Some(report.signature.as_str()));
        assert_eq!(t.profile_cal_signature, t.cc_signature);
        assert!(close(&effective(&dst, 1), &f64_of(&MAGIC)), "CC1 × CM1 != IDT");
        assert!(close(&effective(&dst, 2), &f64_of(&MAGIC)), "CC2 × CM2 != IDT");
        // XMP: class, tier, observer, the exact rationals, and the undo coordinates.
        let xmp = String::from_utf8(t.xmp.unwrap()).unwrap();
        assert_eq!(xmp_attr(&xmp, "IdtClass").as_deref(), Some("relative"));
        assert_eq!(xmp_attr(&xmp, "IdtTier").as_deref(), Some("unit"));
        assert_eq!(xmp_attr(&xmp, "Observer").as_deref(), Some("CIE/CVRL 2006 2°"));
        assert_eq!(xmp_attr(&xmp, "TargetSerial").as_deref(), Some("13"));
        assert_eq!(xmp_attr(&xmp, "OriginalIFD0").as_deref(), Some("8"));
        assert_eq!(xmp_attr(&xmp, "OriginalLength").unwrap(), before.len().to_string());
        // Copy out of the pasted file: the IDT comes back EXACT (from the XMP), not CC × CM rounded.
        let back = IdtClip::copy_from(&dst).unwrap();
        assert_eq!(back.matrix, MAGIC);
        assert_eq!(back.provenance.class, "relative");
        assert_eq!(back.provenance.target_serial, 13);
        // limbus sees the calibration too.
        let info = limbus::read_metadata(&dst).unwrap();
        assert!(info.cameracalibration1.is_some() && info.cameracalibration2.is_some());
        assert!(info.calibration_signature.starts_with("VERICHROME"));
    }

    /// An identity factory matrix (an uncalibrated lumis frame): the calibration IS the IDT.
    #[test]
    fn paste_onto_identity_gives_the_idt_itself() {
        let src = tmp("c.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None, None));
        let dst = tmp("d.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), IDENT, None, None));
        let report = IdtClip::copy_from(&src).unwrap().paste_into(&dst, false).unwrap();
        assert!(!report.cm2_also);
        let mut f = File::open(&dst).unwrap();
        let t = FrameMeta::read(&mut f).unwrap();
        let cc = f64_of(&srational9(&mut f, &t.cc1.unwrap(), false).unwrap());
        assert!(close(&cc, &f64_of(&MAGIC)));
        assert!(t.cc2.is_none());
    }

    /// Unpaste restores the file byte for byte, and a second paste carries the FIRST paste's originals, so undo after two pastes still lands on the untouched file.
    #[test]
    fn unpaste_restores_the_original_even_after_two_pastes() {
        let src = tmp("e.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), MAGIC, None, None));
        let before = synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), None);
        let dst = tmp("f.dng", &before);
        let clip = chameleon_clip(&src);
        let r1 = clip.paste_into(&dst, false).unwrap();
        let mut clip2 = clip.clone();
        clip2.matrix[0] = (1, 1);
        let r2 = clip2.paste_into(&dst, false).unwrap();
        assert_eq!(r2.ifd0_was, r1.ifd0_now);
        // The second paste's XMP still names the true original.
        let mut f = File::open(&dst).unwrap();
        let t = FrameMeta::read(&mut f).unwrap();
        let xmp = String::from_utf8(t.xmp.unwrap()).unwrap();
        assert_eq!(xmp_attr(&xmp, "OriginalIFD0").as_deref(), Some("8"));
        assert_eq!(xmp.matches("xmlns:verichrome").count(), 1, "re-paste must replace the description, not stack them");
        assert_eq!(IdtClip::copy_from(&dst).unwrap().matrix[0], (1, 1));
        IdtClip::unpaste(&dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), before);
        assert!(IdtClip::unpaste(&dst).unwrap_err().contains("no VERICHROME paste"));
    }

    /// A file that already carries XMP keeps every byte of it; the verichrome description is added beside the rest.
    #[test]
    fn paste_merges_into_existing_xmp() {
        let other = "<?xpacket begin=\"\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?><x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" crs:Exposure2012=\"+0.50\"/></rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>";
        let src = tmp("g.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), MAGIC, None, None));
        let dst = tmp("h.dng", &synth_dng("SIGMA", "SIGMA fp", 6064, 4042, [0, 1, 1, 2], (45, 1), FACTORY_A, Some(FACTORY_D65), Some(other)));
        chameleon_clip(&src).paste_into(&dst, false).unwrap();
        let mut f = File::open(&dst).unwrap();
        let xmp = String::from_utf8(FrameMeta::read(&mut f).unwrap().xmp.unwrap()).unwrap();
        assert!(xmp.contains("crs:Exposure2012=\"+0.50\""), "existing XMP content lost");
        assert_eq!(xmp_attr(&xmp, "IdtClass").as_deref(), Some("relative"));
        assert!(xmp.starts_with("<?xpacket begin"));
    }

    #[test]
    fn same_make_and_model_pastes_silently_whatever_else_differs() {
        // The 005/006 case: same Make/Model, but dims, CFA phase and focal all differ (a crop mode, another module). Nick's rule: let them, no warning.
        let src = tmp("i.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None, None));
        let dst = tmp("j.dng", &synth_dng("Android", "Lumis", 4000, 3000, [2, 1, 1, 0], (22, 10), IDENT, None, None));
        let report = IdtClip::copy_from(&src).unwrap().paste_into(&dst, false).unwrap();
        assert!(report.forced_over.is_empty());
        assert!(!report.to_string().contains("WARNING"));
        assert_eq!(IdtClip::copy_from(&dst).unwrap().matrix, MAGIC);
    }

    #[test]
    fn a_different_camera_refuses_and_force_pastes_with_a_warning() {
        let src = tmp("k.dng", &synth_dng("SONY", "ILCE-7M4", 7008, 4672, [0, 1, 1, 2], (50, 1), MAGIC, None, None));
        let before = synth_dng("SONY", "ILCE-7RM5", 9504, 6336, [0, 1, 1, 2], (50, 1), IDENT, None, None);
        let dst = tmp("l.dng", &before);
        let clip = IdtClip::copy_from(&src).unwrap();
        let err = clip.paste_into(&dst, false).unwrap_err();
        assert!(err.contains("different camera") && err.contains("model") && err.contains("sensor"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), before, "a refused paste must not touch the file");
        let report = clip.paste_into(&dst, true).unwrap();
        assert_eq!(report.forced_over, vec!["model", "sensor"]);
        assert!(report.to_string().contains("WARNING: forced over mismatched model, sensor"));
        assert_eq!(IdtClip::copy_from(&dst).unwrap().matrix, MAGIC);
    }

    #[test]
    fn a_channel_count_mismatch_refuses_even_when_forced() {
        // A Bayer IDT onto a monochrome sensor (one CFA channel): no IDT crosses a channel count.
        let src = tmp("n.dng", &synth_dng("Leica", "M11", 9528, 6328, [0, 1, 1, 2], (50, 1), MAGIC, None, None));
        let before = synth_dng("Leica", "M11", 9528, 6328, [0, 0, 0, 0], (50, 1), IDENT, None, None);
        let dst = tmp("o.dng", &before);
        let clip = IdtClip::copy_from(&src).unwrap();
        for force in [false, true] {
            let err = clip.paste_into(&dst, force).unwrap_err();
            assert!(err.contains("3-channel") && err.contains("1 channels"), "{err}");
        }
        assert_eq!(std::fs::read(&dst).unwrap(), before);
    }

    #[test]
    fn rfc3339_is_well_formed() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20, "{t}");
        assert!(t.starts_with("20") && t.ends_with('Z') && &t[10..11] == "T", "{t}");
    }
}
