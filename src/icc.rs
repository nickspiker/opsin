//! Embedded ICC profiles on display-referred ingests (JPEG APP2, WebP ICCP, JXL). A matrix/TRC profile is a declared characterization of the file's RGB — colorant columns (rXYZ/gXYZ/bXYZ, adapted to the D50 PCS per the spec) and a per-channel transfer curve — and honouring it is what makes a file look the way its author saw it: the Esme timeline JPEGs (2026-09-15) carry a "Rec. 2020 F" profile whose red and green colorants are SWAPPED relative to Rec.2020, compensating pixel data that is stored channel-swapped; assume sRGB and you get both the swap and the wrong primaries. LUT-based profiles (A2B0 pipelines, CMYK, Lab PCS) are declined, not guessed at — the caller falls back to its convention and says so.
//!
//! This is the CIE 1931 path by construction: the PCS is 1931 XYZ, so a matrix profile lands in vsf's legacy space before the spectral primaries — the same bridge the DNG matrices already cross. The PCS white is D50 by ICC bookkeeping (the colorants were Bradford-adapted to it when the profile was made), so the matrix is un-adapted back to the profile's own media white — D65 for every sRGB/Rec.2020/P3 profile — before XYZ → VSF RGB. That undoes the spec's accounting, not the scene: no white balance happens here.

/// A parsed matrix/TRC profile.
#[derive(Debug, Clone)]
pub struct MatrixTrc {
    /// Linear RGB → CIE 1931 XYZ, row-major (row = X/Y/Z), with the PCS D50 adaptation UNDONE: RGB(1,1,1) lands on `white`.
    pub rgb_to_xyz: [f32; 9],
    /// The media white the colorants are expressed against (XYZ, Y = 1).
    pub white: [f32; 3],
    /// Per-channel decode curve (encoded → linear).
    pub trc: [Trc; 3],
    /// The profile's description tag, for the HUD ("Rec. 2020 F").
    pub description: String,
}

/// A transfer curve as ICC encodes it: a power, a sampled table, or the parametric families of v4 `para`.
#[derive(Debug, Clone)]
pub enum Trc {
    Gamma(f32),
    /// Sampled, uniformly spaced over [0, 1], values in [0, 1].
    Table(Vec<f32>),
    /// v4 parametric: `function_type` 0..=4 with its parameters (g, a, b, c, d, e, f) as ICC lays them out.
    Para(u16, [f32; 7]),
}

impl Trc {
    /// Encoded [0, 1] → linear [0, 1].
    pub fn linear(&self, x: f32) -> f32 {
        let x = x.clamp(0., 1.);
        match self {
            Trc::Gamma(g) => x.powf(*g),
            Trc::Table(t) => {
                if t.len() < 2 {
                    return x;
                }
                let p = x * (t.len() - 1) as f32;
                let i = (p as usize).min(t.len() - 2);
                let f = p - i as f32;
                t[i] + (t[i + 1] - t[i]) * f
            }
            Trc::Para(kind, p) => {
                let [g, a, b, c, d, e, f] = *p;
                match kind {
                    0 => x.powf(g),
                    1 => if x >= -b / a { (a * x + b).powf(g) } else { 0. },
                    2 => if x >= -b / a { (a * x + b).powf(g) + c } else { c },
                    3 => if x >= d { (a * x + b).powf(g) } else { c * x },
                    _ => if x >= d { (a * x + b).powf(g) + e } else { c * x + f },
                }
            }
        }
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}
fn s15f16(b: &[u8]) -> f32 {
    i32::from_be_bytes(b[..4].try_into().unwrap()) as f32 / 65536.
}

/// The Bradford chromatic-adaptation matrix taking XYZ under white `from` to white `to` — used ONLY to undo the PCS's D50 bookkeeping.
fn bradford(from: [f32; 3], to: [f32; 3]) -> [f32; 9] {
    const B: [f32; 9] = [0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296];
    const BI: [f32; 9] = [0.9869929, -0.1470543, 0.1599627, 0.4323053, 0.5183603, 0.0492912, -0.0085287, 0.0400428, 0.9684867];
    let cone = |w: [f32; 3]| [B[0] * w[0] + B[1] * w[1] + B[2] * w[2], B[3] * w[0] + B[4] * w[1] + B[5] * w[2], B[6] * w[0] + B[7] * w[1] + B[8] * w[2]];
    let (cf, ct) = (cone(from), cone(to));
    let d = [ct[0] / cf[0], ct[1] / cf[1], ct[2] / cf[2]];
    // BI · diag(d) · B
    let mut m = [0f32; 9];
    for r in 0..3 {
        for c in 0..3 {
            m[r * 3 + c] = (0..3).map(|k| BI[r * 3 + k] * d[k] * B[k * 3 + c]).sum();
        }
    }
    m
}

const D50: [f32; 3] = [0.9642, 1., 0.8249];

/// Parse a matrix/TRC profile. `None` when the bytes aren't an ICC profile or it isn't the matrix/TRC kind (LUT pipelines, non-RGB device spaces, non-XYZ PCS).
pub fn parse(bytes: &[u8]) -> Option<MatrixTrc> {
    if bytes.len() < 132 || &bytes[36..40] != b"acsp" || &bytes[16..20] != b"RGB " || &bytes[20..24] != b"XYZ " {
        return None;
    }
    let n = be32(&bytes[128..132]) as usize;
    let table = bytes.get(132..132 + n * 12)?;
    let tag = |sig: &[u8; 4]| -> Option<&[u8]> {
        (0..n).find(|&i| &table[i * 12..i * 12 + 4] == sig).and_then(|i| {
            let off = be32(&table[i * 12 + 4..]) as usize;
            let len = be32(&table[i * 12 + 8..]) as usize;
            bytes.get(off..off + len)
        })
    };
    let xyz = |sig: &[u8; 4]| -> Option<[f32; 3]> {
        let t = tag(sig)?;
        (&t[0..4] == b"XYZ " && t.len() >= 20).then(|| [s15f16(&t[8..]), s15f16(&t[12..]), s15f16(&t[16..])])
    };
    let (r, g, b) = (xyz(b"rXYZ")?, xyz(b"gXYZ")?, xyz(b"bXYZ")?);
    let trc = |sig: &[u8; 4]| -> Option<Trc> {
        let t = tag(sig)?;
        match &t[0..4] {
            b"curv" => {
                let count = be32(&t[8..]) as usize;
                Some(match count {
                    0 => Trc::Gamma(1.),
                    1 => Trc::Gamma(u16::from_be_bytes([t[12], t[13]]) as f32 / 256.),
                    _ => Trc::Table((0..count).map(|i| u16::from_be_bytes([t[12 + i * 2], t[13 + i * 2]]) as f32 / 65535.).collect()),
                })
            }
            b"para" => {
                let kind = u16::from_be_bytes([t[8], t[9]]);
                let np = [1, 3, 4, 5, 7].get(kind as usize).copied()?;
                let mut p = [0f32; 7];
                for (i, v) in p.iter_mut().enumerate().take(np) {
                    *v = s15f16(&t[12 + i * 4..]);
                }
                Some(Trc::Para(kind, p))
            }
            _ => None,
        }
    };
    let trc = [trc(b"rTRC")?, trc(b"gTRC")?, trc(b"bTRC")?];
    // The media white: v2 profiles put the real one in wtpt; v4 pins wtpt to D50 and hides the real one in chad. Either way the colorants sum to the PCS white (D50), and the profile's own white is what RGB(1,1,1) should land on.
    let wtpt = xyz(b"wtpt").unwrap_or(D50);
    let white = if (wtpt[0] - D50[0]).abs() < 1e-3 && (wtpt[2] - D50[2]).abs() < 1e-3 {
        // v4 (or a genuinely D50 profile): recover the source white from chad if present, else stay at D50.
        tag(b"chad").filter(|t| &t[0..4] == b"sf32" && t.len() >= 44).map(|t| {
            let m: Vec<f32> = (0..9).map(|i| s15f16(&t[8 + i * 4..])).collect();
            // chad maps source white → D50; invert on D50.
            let inv = crate::convert::inv3(&m.try_into().unwrap()).unwrap_or([1., 0., 0., 0., 1., 0., 0., 0., 1.]);
            [inv[0] * D50[0] + inv[1] * D50[1] + inv[2] * D50[2], inv[3] * D50[0] + inv[4] * D50[1] + inv[5] * D50[2], inv[6] * D50[0] + inv[7] * D50[1] + inv[8] * D50[2]]
        }).unwrap_or(D50)
    } else {
        wtpt
    };
    // Colorant columns → row-major RGB→XYZ(D50), then undo the D50 adaptation.
    let m50 = [r[0], g[0], b[0], r[1], g[1], b[1], r[2], g[2], b[2]];
    let rgb_to_xyz = crate::convert::matmul3(&bradford(D50, white), &m50);
    let description = tag(b"desc").map(|t| match &t[0..4] {
        b"desc" => {
            let len = be32(&t[8..]) as usize;
            String::from_utf8_lossy(&t[12..(12 + len).min(t.len())]).trim_end_matches('\0').to_string()
        }
        b"mluc" => {
            let (len, off) = (be32(&t[20..]) as usize, be32(&t[24..]) as usize);
            let u: Vec<u16> = t.get(off..off + len).map(|s| s.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect()).unwrap_or_default();
            String::from_utf16_lossy(&u)
        }
        _ => String::new(),
    }).filter(|s| !s.is_empty()).unwrap_or_else(|| "icc".to_string());
    Some(MatrixTrc { rgb_to_xyz, white, trc, description })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REC2020_F: &[u8] = include_bytes!("../assets/test/rec2020_f_rg_swapped.icc");

    #[test]
    fn parses_the_swapped_rec2020_profile() {
        let p = parse(REC2020_F).expect("matrix/TRC profile");
        assert_eq!(p.description, "Rec. 2020 F");
        assert!(matches!(p.trc[0], Trc::Gamma(g) if (g - 2.398).abs() < 0.01));
        // Media white is D65 (v2 wtpt), and after un-adapting, RGB(1,1,1) lands there.
        assert!((p.white[0] - 0.9505).abs() < 1e-3 && (p.white[2] - 1.089).abs() < 1e-3);
        let m = p.rgb_to_xyz;
        let w = [m[0] + m[1] + m[2], m[3] + m[4] + m[5], m[6] + m[7] + m[8]];
        assert!((w[0] - 0.9505).abs() < 2e-3 && (w[1] - 1.).abs() < 2e-3 && (w[2] - 1.089).abs() < 2e-3, "{w:?}");
        // The R and G columns are Rec.2020's G and R: un-adapted column 0 ≈ Rec.2020 green (0.1446, 0.6780, 0.0281), column 1 ≈ Rec.2020 red (0.6370, 0.2627, 0.0000).
        assert!((m[0] - 0.1446).abs() < 5e-3 && (m[3] - 0.6780).abs() < 5e-3, "col0 = {} {} {}", m[0], m[3], m[6]);
        assert!((m[1] - 0.6370).abs() < 5e-3 && (m[4] - 0.2627).abs() < 5e-3, "col1 = {} {} {}", m[1], m[4], m[7]);
    }

    #[test]
    fn trc_families_decode() {
        assert!((Trc::Gamma(2.2).linear(0.5) - 0.5f32.powf(2.2)).abs() < 1e-6);
        assert!((Trc::Table(vec![0., 0.25, 1.]).linear(0.75) - 0.625).abs() < 1e-6);
        // sRGB as a type-3 para: g=2.4 a=1/1.055 b=0.055/1.055 c=1/12.92 d=0.04045.
        let srgb = Trc::Para(3, [2.4, 1. / 1.055, 0.055 / 1.055, 1. / 12.92, 0.04045, 0., 0.]);
        #[allow(deprecated)]
        let want = vsf::colour::srgb_eotf(0.5);
        assert!((srgb.linear(0.5) - want).abs() < 1e-4);
    }

    #[test]
    fn declines_what_it_cannot_honour() {
        assert!(parse(b"not a profile").is_none());
        let mut cmyk = REC2020_F.to_vec();
        cmyk[16..20].copy_from_slice(b"CMYK");
        assert!(parse(&cmyk).is_none());
    }
}
