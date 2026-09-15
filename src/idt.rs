//! Copy / paste a DSR IDT — the magic-9 — between frames of the same camera.
//!
//! A VERICHROME Direct Scene Referred IDT in a DNG is nine numbers: `ColorMatrix1` (XYZ→camera, 9 SRATIONALs) with its `CalibrationIlluminant1`. A chameleon target scan writes them; an uncalibrated lumis frame carries the identity there. The IDT is a property of the SENSOR, so one scan characterizes every frame that sensor ever shot: **copy** lifts the matrix VERBATIM (exact rationals — no float round trip) together with the frame's identity fingerprint; **paste** refuses unless the target's fingerprint matches field for field, then overwrites the target's own `ColorMatrix1` slot in place. Same byte count, so the IFD is never rebuilt — nothing else in the file moves, and the sensor plane is never touched.
//!
//! The fingerprint is make, model, raw-plane dims, CFA tile, focal length — any mismatch refuses by default, in two tiers: SOFT (focal, dims — a lens change, a crop mode, lumis's 2×-height slitscan ring: the same sensor legitimately changes these) and HARD (make, model, CFA tile — a different sensor). `force` pastes through either, and the report records what was overridden. Focal length is not lens trivia here: a phone's main/ultrawide/tele are DIFFERENT SENSORS behind one Make/Model ("Android"/"Lumis"), and focal length is the field that tells them apart (the 005/006 sample folders are exactly that pair: 6.9 mm 4080×3072 vs 2.2 mm 4000×3000). A target that also carries `ColorMatrix2` gets the SAME matrix in both slots with matching illuminant codes: a DSR IDT is one matrix, and readers interpolate CM1↔CM2 by white balance — identical slots make that interpolation a no-op rather than a corruption. `ProfileName` is left alone (a different-length string would mean restructuring).
//!
//! The clip persists as readable text ([`IdtClip::to_text`]) at `$XDG_CONFIG_HOME/opsin/idt.clip`, so copy/paste crosses folders and launches, and the nine numbers can be kept, shared, and read by eye. Copy also works from a VSF-Image that carries a verbatim DNG matrix in its colour_profile (f32 there, so rationals are reconstructed exactly from the f32 — den 2^24); paste INTO a VSF is not wired yet: it would add a `unit`-grade colour_profile entry rather than patch a tag, and rewriting a container has provenance rules to settle first.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// A signed TIFF rational, verbatim: (numerator, denominator).
pub type SRational = (i32, i32);

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

/// One copied IDT: the nine verbatim rationals + illuminant, tagged with where it came from and the fingerprint it may be pasted onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdtClip {
    /// Path of the frame it was copied from — audit, not identity.
    pub source: String,
    pub fingerprint: Fingerprint,
    /// DNG CalibrationIlluminant1 — EXIF LightSource code (23 = D50, what chameleon writes).
    pub illuminant: u16,
    /// DNG ColorMatrix1, XYZ→camera, row-major, verbatim SRATIONALs.
    pub matrix: [SRational; 9],
}

/// What a paste changed, for the caller to surface. `replaced` is the target's previous ColorMatrix1 — printing it is the undo path (paste it back by hand from a clip).
#[derive(Debug)]
pub struct PasteReport {
    pub target: PathBuf,
    pub replaced: [SRational; 9],
    pub replaced_illuminant: u16,
    /// The target also carried ColorMatrix2 and it was set to the same matrix.
    pub cm2_also: bool,
    /// Fingerprint fields that did NOT match and were overridden by `force` (empty for a clean paste).
    pub forced_over: Vec<&'static str>,
}

impl std::fmt::Display for PasteReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ColorMatrix1{} replaced (was illuminant {} matrix", self.target.display(), if self.cm2_also { "+2" } else { "" }, self.replaced_illuminant)?;
        for (n, d) in self.replaced {
            write!(f, " {n}/{d}")?;
        }
        write!(f, ")")?;
        if !self.forced_over.is_empty() {
            write!(f, " — WARNING: forced over mismatched {}", self.forced_over.join(", "))?;
        }
        Ok(())
    }
}

use crate::tiff::{self, srational9, u16e, Entry, FrameMeta, TYPE_SHORT, TYPE_SRATIONAL};

fn write_matrix(f: &mut File, e: &Entry, be: bool, m: &[SRational; 9]) -> Result<(), String> {
    if e.ty != TYPE_SRATIONAL || e.count != 9 {
        return Err(format!("ColorMatrix entry is type {} count {} — expected SRATIONAL×9", e.ty, e.count));
    }
    let mut raw = Vec::with_capacity(72);
    for &(n, d) in m {
        tiff::put_i32(&mut raw, n, be);
        tiff::put_i32(&mut raw, d, be);
    }
    f.seek(SeekFrom::Start(tiff::u32e(&e.value, be) as u64)).map_err(|e| e.to_string())?;
    f.write_all(&raw).map_err(|e| e.to_string())
}

/// CalibrationIlluminant is a SHORT inline in the entry's value field (left-justified, so bytes 8..10 of the entry).
fn write_illuminant(f: &mut File, e: &Entry, be: bool, code: u16) -> Result<(), String> {
    if e.ty != TYPE_SHORT || e.count != 1 {
        return Err(format!("CalibrationIlluminant entry is type {} count {} — expected SHORT×1", e.ty, e.count));
    }
    f.seek(SeekFrom::Start(e.pos + 8)).map_err(|e| e.to_string())?;
    f.write_all(&if be { code.to_be_bytes() } else { code.to_le_bytes() }).map_err(|e| e.to_string())
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

/// f32 → exact SRATIONAL over 2^24 (a 24-bit mantissa is representable exactly for |v| < 128 — every colour matrix coefficient by a wide margin).
fn rational_of_f32(v: f32) -> SRational {
    const DEN: i32 = 1 << 24;
    ((v as f64 * DEN as f64).round() as i32, DEN)
}

impl IdtClip {
    /// Lift the IDT out of `path`: a DNG (verbatim rationals from ColorMatrix1) or a VSF-Image carrying a verbatim DNG matrix in its colour_profile.
    pub fn copy_from(path: &Path) -> Result<IdtClip, String> {
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        if ext.as_deref() == Some("vsf") {
            return Self::copy_from_vsf(path);
        }
        let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let tags = FrameMeta::read(&mut f)?;
        let cm1 = tags.cm1.ok_or_else(|| format!("{}: no ColorMatrix1", path.display()))?;
        let matrix = srational9(&mut f, &cm1, tags.be)?;
        let illuminant = tags.illuminant1();
        let fingerprint = fingerprint_of_dng(path, tags.focal)?;
        Ok(IdtClip { source: path.display().to_string(), fingerprint, illuminant, matrix })
    }

    fn copy_from_vsf(path: &Path) -> Result<IdtClip, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let img = vsf::spectral_image::read(&bytes).map_err(|e| e.to_string())?;
        let profile = img.profile.as_ref().ok_or_else(|| format!("{}: no colour_profile", path.display()))?;
        let (m, illuminant) = profile.dng_colormatrix[0].ok_or_else(|| format!("{}: colour_profile carries no verbatim DNG matrix", path.display()))?;
        let (cfa_h, cfa_w, cfa) = match &img.layout {
            vsf::spectral_image::PlaneLayout::Mosaic { cfa } => (cfa.shape[0] as u16, cfa.shape[1] as u16, cfa.data.clone()),
            vsf::spectral_image::PlaneLayout::Planar => (0, 0, Vec::new()),
        };
        let mut matrix = [(0, 0); 9];
        for (r, &v) in matrix.iter_mut().zip(&m) {
            *r = rational_of_f32(v);
        }
        Ok(IdtClip {
            source: path.display().to_string(),
            fingerprint: Fingerprint { make: img.make.clone(), model: img.model.clone(), width: img.width, height: img.height, cfa_w, cfa_h, cfa, focal: None },
            illuminant,
            matrix,
        })
    }

    /// Patch this IDT into `target` (a DNG) in place — after the fingerprint gate. Without `force`, ANY differing field refuses, naming the fields and what each side declares; the message distinguishes the two tiers so the user knows what they'd be overriding. **Soft** = focal length alone: on an interchangeable-lens body (a Sony) the sensor — and so the IDT — survives a lens change, so this is the case `force` exists for; on a phone the same mismatch means a different camera module. **Hard** = make/model/sensor dims/CFA: almost certainly a different sensor. `force` pastes through either tier — user's call — and the report carries the overridden fields as a warning so the decision is on record. Refuses regardless if the target's tags aren't the shape an in-place patch needs.
    pub fn paste_into(&self, target: &Path, force: bool) -> Result<PasteReport, String> {
        let mut f = File::options().read(true).write(true).open(target).map_err(|e| format!("{}: {e}", target.display()))?;
        let tags = FrameMeta::read(&mut f)?;
        let fp = fingerprint_of_dng(target, tags.focal)?;
        let diff = fp.diff(&self.fingerprint);
        // Soft = fields the SAME sensor can legitimately change: focal (a lens change on an interchangeable-lens body) and raw dims (a crop mode; lumis's slitscan ring at 2× height). Hard = make/model/CFA — a different tile or a different camera name is a different sensor.
        let soft_only = diff.iter().all(|f| *f == "focal" || *f == "sensor");
        if !diff.is_empty() && !force {
            let tier = if soft_only {
                "focal length / raw dims differ — a lens change, a crop mode, or a slitscan ring keeps the sensor (IDT still valid); on a phone a focal change means a different camera module. Pass --force (Ctrl+Shift+V) to paste anyway"
            } else {
                "different CFA tile or camera name — almost certainly a different sensor. Pass --force (Ctrl+Shift+V) only if you know better"
            };
            return Err(format!(
                "{}: camera doesn't match the copied IDT — differs in {}: {} (target {}/{} {}×{} cfa {}×{} {:?} focal {}; clip {}/{} {}×{} cfa {}×{} {:?} focal {})",
                target.display(),
                diff.join(", "),
                tier,
                fp.make, fp.model, fp.width, fp.height, fp.cfa_w, fp.cfa_h, fp.cfa, fp.focal_text(),
                self.fingerprint.make, self.fingerprint.model, self.fingerprint.width, self.fingerprint.height, self.fingerprint.cfa_w, self.fingerprint.cfa_h, self.fingerprint.cfa, self.fingerprint.focal_text(),
            ));
        }
        let cm1 = tags.cm1.ok_or_else(|| format!("{}: no ColorMatrix1 slot to patch", target.display()))?;
        let ill1 = tags.ill1.ok_or_else(|| format!("{}: no CalibrationIlluminant1 slot to patch", target.display()))?;
        // Read back what's there BEFORE writing anything — the report is the undo path.
        let replaced = srational9(&mut f, &cm1, tags.be)?;
        let replaced_illuminant = u16e(&ill1.value, tags.be);
        // Validate every slot first so a shape problem can't leave the file half-patched.
        if let Some(cm2) = &tags.cm2 {
            srational9(&mut f, cm2, tags.be)?;
        }
        write_matrix(&mut f, &cm1, tags.be, &self.matrix)?;
        write_illuminant(&mut f, &ill1, tags.be, self.illuminant)?;
        let mut cm2_also = false;
        if let Some(cm2) = &tags.cm2 {
            write_matrix(&mut f, cm2, tags.be, &self.matrix)?;
            if let Some(ill2) = &tags.ill2 {
                write_illuminant(&mut f, ill2, tags.be, self.illuminant)?;
            }
            cm2_also = true;
        }
        f.flush().map_err(|e| e.to_string())?;
        Ok(PasteReport { target: target.to_path_buf(), replaced, replaced_illuminant, cm2_also, forced_over: diff })
    }

    /// The readable clip form — one field per line, exact rationals, no floats.
    pub fn to_text(&self) -> String {
        let fp = &self.fingerprint;
        let mut s = String::new();
        s.push_str("opsin-idt 1\n");
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
        s.push_str("matrix");
        for (n, d) in self.matrix {
            s.push_str(&format!(" {n}/{d}"));
        }
        s.push('\n');
        s
    }

    pub fn from_text(text: &str) -> Result<IdtClip, String> {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("opsin-idt 1") {
            return Err("not an opsin-idt 1 clip".to_string());
        }
        let mut source = String::new();
        let mut fp = Fingerprint { make: String::new(), model: String::new(), width: 0, height: 0, cfa_w: 0, cfa_h: 0, cfa: Vec::new(), focal: None };
        let mut illuminant = 0u16;
        let mut matrix: Option<[SRational; 9]> = None;
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
        Ok(IdtClip { source, fingerprint: fp, illuminant, matrix })
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

    /// A minimal little-endian DNG-shaped TIFF: IFD0 with the raw-plane tags limbus needs (NewSubFileType 0, dims, bits, strips, make/model, CFA), ColorMatrix1 (+ optional CM2), CalibrationIlluminant1 (+2), and an EXIF IFD with FocalLength. Returns the bytes.
    fn synth_dng(make: &str, model: &str, w: u32, h: u32, cfa: [u8; 4], focal: (u32, u32), cm1: [SRational; 9], cm2: Option<[SRational; 9]>) -> Vec<u8> {
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
        entries.push((33421, 3, 2, [le16(2), le16(2)].concat()));
        entries.push((33422, 1, 4, cfa.to_vec()));
        entries.push((34665, 4, 1, le(0))); // patched to the EXIF IFD offset
        entries.push((50721, 10, 9, rat9(&cm1)));
        if let Some(m) = &cm2 {
            entries.push((50722, 10, 9, rat9(m)));
        }
        entries.push((50778, 3, 1, le16(23)));
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

    #[test]
    fn copy_reads_verbatim_and_text_round_trips() {
        let src = tmp("src.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None));
        let clip = IdtClip::copy_from(&src).unwrap();
        assert_eq!(clip.matrix, MAGIC);
        assert_eq!(clip.illuminant, 23);
        assert_eq!(clip.fingerprint.focal, Some((69, 10)));
        assert_eq!(clip.fingerprint.cfa, vec![1, 2, 0, 1]);
        assert_eq!((clip.fingerprint.width, clip.fingerprint.height), (4080, 3072));
        let back = IdtClip::from_text(&clip.to_text()).unwrap();
        assert_eq!(back, clip);
    }

    #[test]
    fn paste_patches_matching_frame_in_place_and_only_there() {
        let src = tmp("a.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None));
        let before = synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (6900, 1000), IDENT, None);
        let dst = tmp("b.dng", &before);
        let clip = IdtClip::copy_from(&src).unwrap();
        let report = clip.paste_into(&dst, false).unwrap();
        assert_eq!(report.replaced, IDENT);
        assert!(!report.cm2_also);
        // The target now reads back the magic-9, and NOTHING else in the file moved: byte-diff is confined to the 72-byte matrix block.
        assert_eq!(IdtClip::copy_from(&dst).unwrap().matrix, MAGIC);
        let after = std::fs::read(&dst).unwrap();
        assert_eq!(before.len(), after.len());
        let changed: Vec<usize> = before.iter().zip(&after).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
        assert!(!changed.is_empty());
        assert!(changed.last().unwrap() - changed.first().unwrap() < 72, "diff span {:?}", (changed.first(), changed.last()));
    }

    #[test]
    fn paste_fills_both_matrix_slots_when_target_has_two() {
        let src = tmp("c.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None));
        let dst = tmp("d.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), IDENT, Some(IDENT)));
        let report = IdtClip::copy_from(&src).unwrap().paste_into(&dst, false).unwrap();
        assert!(report.cm2_also);
        let mut f = File::open(&dst).unwrap();
        let tags = FrameMeta::read(&mut f).unwrap();
        assert_eq!(srational9(&mut f, &tags.cm2.unwrap(), false).unwrap(), MAGIC);
        assert_eq!(u16e(&tags.ill2.unwrap().value, false), 23); // illuminant2 patched from the stale 21 to the clip's 23
    }

    #[test]
    fn paste_refuses_a_different_sensor_and_names_the_fields() {
        // The 005/006 case: same Make/Model, but the ultrawide — dims, CFA phase, and focal all differ. Nothing is written.
        let src = tmp("e.dng", &synth_dng("Android", "Lumis", 4080, 3072, [1, 2, 0, 1], (69, 10), MAGIC, None));
        let before = synth_dng("Android", "Lumis", 4000, 3000, [2, 1, 1, 0], (22, 10), IDENT, None);
        let dst = tmp("f.dng", &before);
        let err = IdtClip::copy_from(&src).unwrap().paste_into(&dst, false).unwrap_err();
        assert!(err.contains("sensor") && err.contains("cfa") && err.contains("focal"), "{err}");
        assert!(!err.contains("make"), "{err}");
        assert_eq!(std::fs::read(&dst).unwrap(), before, "a refused paste must not touch the file");
    }

    #[test]
    fn focal_only_mismatch_is_the_soft_tier_and_force_pastes_with_a_warning() {
        // A lens change on an interchangeable-lens body: same sensor, different focal. Refused by default with the lens-change hint; --force pastes and the report records what was overridden.
        let src = tmp("g.dng", &synth_dng("SONY", "ILCE-7M4", 7008, 4672, [0, 1, 1, 2], (50, 1), MAGIC, None));
        let dst = tmp("h.dng", &synth_dng("SONY", "ILCE-7M4", 7008, 4672, [0, 1, 1, 2], (85, 1), IDENT, None));
        let clip = IdtClip::copy_from(&src).unwrap();
        let err = clip.paste_into(&dst, false).unwrap_err();
        assert!(err.contains("differs in focal:") && err.contains("lens change"), "{err}");
        let report = clip.paste_into(&dst, true).unwrap();
        assert_eq!(report.forced_over, vec!["focal"]);
        assert!(report.to_string().contains("WARNING: forced over mismatched focal"));
        assert_eq!(IdtClip::copy_from(&dst).unwrap().matrix, MAGIC);
        // Hard tier reads differently — and force still goes through (user's call).
        let dst2 = tmp("i.dng", &synth_dng("SONY", "ILCE-7M4", 7008, 4672, [1, 0, 2, 1], (50, 1), IDENT, None));
        let err = clip.paste_into(&dst2, false).unwrap_err();
        assert!(err.contains("different CFA tile or camera name"), "{err}");
        assert_eq!(clip.paste_into(&dst2, true).unwrap().forced_over, vec!["cfa"]);
    }
}
