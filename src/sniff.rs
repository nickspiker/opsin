//! What a file IS, from its bytes — never from its name (Nick 2026-09-15: "why do we even care what the extension is?").
//! Every format opsin opens announces itself in its first bytes; the one family that shares a signature (TIFF and the camera RAWs built on it) resolves itself further in by its own tags, and limbus already does that resolution by content.
//! The only thing a name could tell us that bytes cannot is a headerless sensor dump, and a name cannot tell us that either — it has no dimensions, bit depth or CFA order to give. Those come from the statistics of the bytes themselves (see `headerless`).

/// The byte-recognised family of a file. Everything except `Unknown` has a decoder; `Unknown` goes to the headerless guesser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `RÅ<` — a VSF container (only a `spectral_image` document renders; another VSF document falls thru to the guesser, which shows its bytes).
    Vsf,
    /// `FF D8 FF`.
    Jpeg,
    /// Bare codestream `FF 0A` or the ISOBMFF box `00 00 00 0C 'JXL '`.
    Jxl,
    /// `RIFF …. WEBP`.
    WebP,
    /// The TIFF family: `II*\0` / `MM\0*`, Olympus `IIRO`/`IIRS`/`MMOR`, Panasonic `IIU\0`. DNG, NEF, CR2, ARW, PEF, SRW, ORF, RW2 and plain TIFF all live here; limbus tells them apart by their tags.
    Tiff,
    /// Canon CR3: ISOBMFF with `ftypcrx ` at offset 4.
    Cr3,
    /// Fujifilm `FUJIFILMCCD-RAW`.
    Raf,
    /// Canon CRW: `II\x1a\0\0\0HEAPCCDR`.
    Crw,
    /// No signature we know.
    Unknown,
}

impl Kind {
    /// Lower-case label for messages and the HUD.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Vsf => "vsf",
            Kind::Jpeg => "jpeg",
            Kind::Jxl => "jxl",
            Kind::WebP => "webp",
            Kind::Tiff => "tiff",
            Kind::Cr3 => "cr3",
            Kind::Raf => "raf",
            Kind::Crw => "crw",
            Kind::Unknown => "unknown",
        }
    }
}

/// Bytes the sniff needs: the longest signature ends at offset 16.
pub const HEAD_LEN: usize = 16;

/// Recognise `head` (the file's first bytes; fewer than [`HEAD_LEN`] is fine, short files just match less).
pub fn sniff(head: &[u8]) -> Kind {
    let starts = |m: &[u8]| head.len() >= m.len() && &head[..m.len()] == m;
    let at = |o: usize, m: &[u8]| head.len() >= o + m.len() && &head[o..o + m.len()] == m;
    if starts("RÅ<".as_bytes()) {
        Kind::Vsf
    } else if starts(&[0xFF, 0xD8, 0xFF]) {
        Kind::Jpeg
    } else if starts(&[0xFF, 0x0A]) || starts(b"\0\0\0\x0cJXL ") {
        Kind::Jxl
    } else if starts(b"RIFF") && at(8, b"WEBP") {
        Kind::WebP
    } else if starts(b"FUJIFILMCCD-RAW") {
        Kind::Raf
    } else if starts(b"II\x1a\0\0\0HEAPCCDR") {
        Kind::Crw
    } else if at(4, b"ftypcrx ") {
        Kind::Cr3
    } else if starts(b"II*\0") || starts(b"MM\0*") || starts(b"IIRO") || starts(b"IIRS") || starts(b"MMOR") || starts(b"IIU\0") {
        Kind::Tiff
    } else {
        Kind::Unknown
    }
}

/// Sniff a file by reading its head. `None` when it cannot be opened or is not a regular file.
pub fn sniff_path(path: &std::path::Path) -> Option<Kind> {
    use std::io::Read;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; HEAD_LEN];
    let mut got = 0;
    while got < HEAD_LEN {
        match f.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    Some(sniff(&head[..got]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_resolve_by_bytes_alone() {
        assert_eq!(sniff("RÅ<zzz".as_bytes()), Kind::Vsf);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE1]), Kind::Jpeg);
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00]), Kind::Jxl);
        assert_eq!(sniff(b"\0\0\0\x0cJXL \r\n\x87\n"), Kind::Jxl);
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Kind::WebP);
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), Kind::Unknown);
        assert_eq!(sniff(b"II*\0\x08\0\0\0"), Kind::Tiff);
        assert_eq!(sniff(b"MM\0*\0\0\0\x08"), Kind::Tiff);
        assert_eq!(sniff(b"IIU\0\x18\0\0\0"), Kind::Tiff);
        assert_eq!(sniff(b"\0\0\0\x18ftypcrx \0\0\0\x01"), Kind::Cr3);
        assert_eq!(sniff(b"FUJIFILMCCD-RAW 0201"), Kind::Raf);
        assert_eq!(sniff(b"II\x1a\0\0\0HEAPCCDR"), Kind::Crw);
        assert_eq!(sniff(b"hello, world"), Kind::Unknown);
        assert_eq!(sniff(b""), Kind::Unknown);
        assert_eq!(sniff(b"R"), Kind::Unknown);
    }
}
