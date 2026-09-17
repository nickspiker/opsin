//! Minimal TIFF/DNG IFD walker — the metadata that limbus's decoder doesn't surface (EXIF exposure fields, the DNG profile tags with their file positions) read straight from the headers, no pixel decode. Shared by [`crate::idt`] (which patches ColorMatrix1 in place and so needs entry POSITIONS, not just values) and the viewer's frame-info HUD. Byte order follows the header per TIFF; inline values sit left-justified in the 4-byte value field.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub fn u16e(b: &[u8], be: bool) -> u16 {
    let a: [u8; 2] = b[..2].try_into().unwrap();
    if be { u16::from_be_bytes(a) } else { u16::from_le_bytes(a) }
}
pub fn u32e(b: &[u8], be: bool) -> u32 {
    let a: [u8; 4] = b[..4].try_into().unwrap();
    if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) }
}
pub fn i32e(b: &[u8], be: bool) -> i32 {
    let a: [u8; 4] = b[..4].try_into().unwrap();
    if be { i32::from_be_bytes(a) } else { i32::from_le_bytes(a) }
}
pub fn put_i32(out: &mut Vec<u8>, v: i32, be: bool) {
    out.extend(if be { v.to_be_bytes() } else { v.to_le_bytes() });
}

/// One IFD entry as it sits in the file: where the 12-byte entry starts (for inline patches) and its decoded fields.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub pos: u64,
    pub ty: u16,
    pub count: u32,
    /// The 4-byte value field: an inline value or an offset, per TIFF.
    pub value: [u8; 4],
}

pub const TYPE_ASCII: u16 = 2;
pub const TYPE_SHORT: u16 = 3;
pub const TYPE_LONG: u16 = 4;
pub const TYPE_RATIONAL: u16 = 5;
pub const TYPE_SRATIONAL: u16 = 10;

pub const TAG_EXIF_IFD: u16 = 34665;
pub const TAG_EXPOSURE_TIME: u16 = 33434;
pub const TAG_F_NUMBER: u16 = 33437;
pub const TAG_ISO: u16 = 34855;
pub const TAG_DATETIME_ORIGINAL: u16 = 36867;
pub const TAG_FOCAL: u16 = 37386;
pub const TAG_CM1: u16 = 50721;
pub const TAG_CM2: u16 = 50722;
pub const TAG_BASELINE_EXPOSURE: u16 = 50730;
pub const TAG_ILL1: u16 = 50778;
pub const TAG_ILL2: u16 = 50779;
pub const TAG_PROFILE_NAME: u16 = 50936;
pub const TAG_ICC: u16 = 34675;
pub const TYPE_UNDEFINED: u16 = 7;

pub fn read_exact_at(f: &mut File, off: u64, buf: &mut [u8]) -> Result<(), String> {
    f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
    f.read_exact(buf).map_err(|e| format!("read at {off}: {e}"))
}

/// Header: byte order + IFD0 offset. Errors on anything that isn't a classic TIFF.
pub fn header(f: &mut File) -> Result<(bool, u64), String> {
    let mut hdr = [0u8; 8];
    read_exact_at(f, 0, &mut hdr)?;
    let be = match &hdr[0..4] {
        [b'I', b'I', 42, 0] => false,
        [b'M', b'M', 0, 42] => true,
        _ => return Err("not a TIFF/DNG".to_string()),
    };
    Ok((be, u32e(&hdr[4..8], be) as u64))
}

/// Walk one IFD, returning `(tag, entry)` for every entry whose tag is in `tags`, in file order.
pub fn walk_ifd(f: &mut File, ifd: u64, be: bool, tags: &[u16]) -> Result<Vec<(u16, Entry)>, String> {
    let mut n = [0u8; 2];
    read_exact_at(f, ifd, &mut n)?;
    let n = u16e(&n, be) as usize;
    let mut raw = vec![0u8; n * 12];
    read_exact_at(f, ifd + 2, &mut raw)?;
    let mut out = Vec::new();
    for i in 0..n {
        let e = &raw[i * 12..i * 12 + 12];
        let tag = u16e(&e[0..2], be);
        if tags.contains(&tag) {
            out.push((tag, Entry { pos: ifd + 2 + (i * 12) as u64, ty: u16e(&e[2..4], be), count: u32e(&e[4..8], be), value: e[8..12].try_into().unwrap() }));
        }
    }
    Ok(out)
}

/// A single RATIONAL (unsigned) — `None` if the entry isn't one.
pub fn rational(f: &mut File, e: &Entry, be: bool) -> Option<(u32, u32)> {
    if e.ty != TYPE_RATIONAL || e.count != 1 {
        return None;
    }
    let mut r = [0u8; 8];
    read_exact_at(f, u32e(&e.value, be) as u64, &mut r).ok()?;
    Some((u32e(&r[0..4], be), u32e(&r[4..8], be)))
}

/// A single SRATIONAL — `None` if the entry isn't one.
pub fn srational(f: &mut File, e: &Entry, be: bool) -> Option<(i32, i32)> {
    if e.ty != TYPE_SRATIONAL || e.count != 1 {
        return None;
    }
    let mut r = [0u8; 8];
    read_exact_at(f, u32e(&e.value, be) as u64, &mut r).ok()?;
    Some((i32e(&r[0..4], be), i32e(&r[4..8], be)))
}

/// SHORT or LONG scalar, inline.
pub fn scalar(e: &Entry, be: bool) -> Option<u32> {
    match (e.ty, e.count) {
        (TYPE_SHORT, 1) => Some(u16e(&e.value, be) as u32),
        (TYPE_LONG, 1) => Some(u32e(&e.value, be)),
        _ => None,
    }
}

/// ASCII, NUL-trimmed; inline when count ≤ 4, else at the offset.
pub fn ascii(f: &mut File, e: &Entry, be: bool) -> Option<String> {
    if e.ty != TYPE_ASCII || e.count == 0 {
        return None;
    }
    let bytes = if e.count <= 4 {
        e.value[..e.count as usize].to_vec()
    } else {
        let mut b = vec![0u8; e.count as usize];
        read_exact_at(f, u32e(&e.value, be) as u64, &mut b).ok()?;
        b
    };
    Some(String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string())
}

/// The nine SRATIONALs a ColorMatrix entry points at, verbatim.
pub fn srational9(f: &mut File, e: &Entry, be: bool) -> Result<[(i32, i32); 9], String> {
    if e.ty != TYPE_SRATIONAL || e.count != 9 {
        return Err(format!("ColorMatrix entry is type {} count {} — expected SRATIONAL×9", e.ty, e.count));
    }
    let mut raw = [0u8; 72];
    read_exact_at(f, u32e(&e.value, be) as u64, &mut raw)?;
    let mut m = [(0, 0); 9];
    for (i, r) in m.iter_mut().enumerate() {
        *r = (i32e(&raw[i * 8..i * 8 + 4], be), i32e(&raw[i * 8 + 4..i * 8 + 8], be));
    }
    Ok(m)
}

/// Everything the viewer's info HUD and the IDT clipboard read from a DNG's headers: EXIF exposure fields verbatim, the DNG profile tags with positions (so a paste can patch in place).
#[derive(Debug, Default, Clone)]
pub struct FrameMeta {
    pub be: bool,
    pub focal: Option<(u32, u32)>,
    pub f_number: Option<(u32, u32)>,
    pub exposure_s: Option<(u32, u32)>,
    pub iso: Option<u32>,
    pub datetime: Option<String>,
    pub profile_name: Option<String>,
    /// DNG BaselineExposure (stops) — lumis writes the on-screen display gain here so raw converters open at the same brightness. Informational only in opsin.
    pub baseline_exposure: Option<(i32, i32)>,
    /// Embedded ICC profile (34675) bytes, verbatim — an RGB TIFF's declared characterization.
    pub icc: Option<Vec<u8>>,
    pub cm1: Option<Entry>,
    pub cm2: Option<Entry>,
    pub ill1: Option<Entry>,
    pub ill2: Option<Entry>,
}

impl FrameMeta {
    pub fn read(f: &mut File) -> Result<FrameMeta, String> {
        let (be, ifd0) = header(f)?;
        let mut m = FrameMeta { be, ..Default::default() };
        let mut exif_ifd = None;
        for (tag, e) in walk_ifd(f, ifd0, be, &[TAG_EXIF_IFD, TAG_CM1, TAG_CM2, TAG_ILL1, TAG_ILL2, TAG_PROFILE_NAME, TAG_BASELINE_EXPOSURE, TAG_ICC])? {
            match tag {
                TAG_EXIF_IFD => exif_ifd = Some(u32e(&e.value, be) as u64),
                TAG_ICC if e.ty == TYPE_UNDEFINED && e.count > 4 => {
                    let mut b = vec![0u8; e.count as usize];
                    if read_exact_at(f, u32e(&e.value, be) as u64, &mut b).is_ok() {
                        m.icc = Some(b);
                    }
                }
                TAG_CM1 => m.cm1 = Some(e),
                TAG_CM2 => m.cm2 = Some(e),
                TAG_ILL1 => m.ill1 = Some(e),
                TAG_ILL2 => m.ill2 = Some(e),
                TAG_PROFILE_NAME => m.profile_name = ascii(f, &e, be),
                TAG_BASELINE_EXPOSURE => m.baseline_exposure = srational(f, &e, be),
                _ => {}
            }
        }
        if let Some(exif) = exif_ifd {
            for (tag, e) in walk_ifd(f, exif, be, &[TAG_FOCAL, TAG_F_NUMBER, TAG_EXPOSURE_TIME, TAG_ISO, TAG_DATETIME_ORIGINAL])? {
                match tag {
                    TAG_FOCAL => m.focal = rational(f, &e, be),
                    TAG_F_NUMBER => m.f_number = rational(f, &e, be),
                    TAG_EXPOSURE_TIME => m.exposure_s = rational(f, &e, be),
                    TAG_ISO => m.iso = scalar(&e, be),
                    TAG_DATETIME_ORIGINAL => m.datetime = ascii(f, &e, be),
                    _ => {}
                }
            }
        }
        Ok(m)
    }

    pub fn read_path(path: &Path) -> Result<FrameMeta, String> {
        let mut f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::read(&mut f)
    }

    pub fn illuminant1(&self) -> u16 {
        self.ill1.map(|e| u16e(&e.value, self.be)).unwrap_or(0)
    }
}
