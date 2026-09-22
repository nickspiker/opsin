//! Any bytes as an image — the fallback for a file no decoder claims (Nick 2026-09-15: "I do have a RAW fallback for anything that doesn't open, even text, programs, whatever … it uses stats to distinguish channel count, dims").
//! Ported from Dolphin's `read_headerless_raw` (also in oriel and shotover_pipeline), where it sits as the `.or_else` behind the DNG reader. Nothing about the file is trusted, so everything is inferred from the bytes' own statistics:
//! 1. **Sample format and channel count.** A few thousand positions are sampled; for each candidate layout (u8, u16 LE, u16 BE, f32 LE, f32 BE × one to four interleaved channels) the mean |sample − the same channel one pixel on|, normalised by the interquartile range, scores how image-like the bytes are under that reading. Real images vary slowly along a row in every channel; the true stride scores lowest. A float reading with a non-finite sample is discarded outright.
//! 2. **Width.** Every width within an aspect band of 8:1 either way of square is scored by the mean |luminance − the pixel one row up| at the sampled positions, times the aspect ratio as a penalty, so a headerless dump with a real width finds it (rows cohere sharply) and bytes with no image in them settle near square. Dolphin only tried exact divisors and panicked on a prime pixel count; here a trailing partial row is simply dropped.
//! 3. **Bayer.** A two-channel winner is also tried as a CFA mosaic (row pairs compared two rows apart); when that reads better the height halves and the 2×2 tiles demosaic to RGB, the channel order picked by which pair of tile positions correlates best.
//! 4. **Levels.** Black is the 256th-darkest value, white the brightest; the plane stores 16-bit between them. The profile is an identity `Assumed` entry: the bytes are taken as VSF RGB because nothing says otherwise, and `Assumed` says so.
//! The verdict rides in `make`/`model` ("headerless" / "16-bit LE, 2 ch, CFA RGGB"), which the viewer's frame-info HUD already prints. Sampling is deterministic (splitmix64 seeded by the length), so the same file guesses the same way every time.

use rayon::prelude::*;
use vsf::spectral_image::{ColourProfile, IdtClass, PlaneLayout, ProfileEntry, ProfileGrade, Provenance, SpectralChannel, SpectralImage, Transfer};
use vsf::BitPackedTensor;

use crate::convert::{rgb_channel_names, Decoded};

/// Positions sampled for the format and width votes.
const SAMPLES: usize = 1 << 12;
/// Interleaved channel counts tried.
const MAX_CHANNELS: usize = 4;
/// Widths considered: from `sqrt(n) / ASPECT_BAND` to `sqrt(n) × ASPECT_BAND`.
const ASPECT_BAND: f64 = 8.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    U8,
    U16Le,
    U16Be,
    F32Le,
    F32Be,
}

impl Format {
    const ALL: [Format; 5] = [Format::U8, Format::U16Le, Format::U16Be, Format::F32Le, Format::F32Be];

    fn bytes(self) -> usize {
        match self {
            Format::U8 => 1,
            Format::U16Le | Format::U16Be => 2,
            Format::F32Le | Format::F32Be => 4,
        }
    }

    /// One sample at byte offset `o` (the caller keeps `o` aligned and in bounds), scaled so integers land in 0..1.
    fn read(self, data: &[u8], o: usize) -> f32 {
        match self {
            Format::U8 => data[o] as f32 / 256.,
            Format::U16Le => u16::from_le_bytes([data[o], data[o + 1]]) as f32 / 65536.,
            Format::U16Be => u16::from_be_bytes([data[o], data[o + 1]]) as f32 / 65536.,
            Format::F32Le => f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
            Format::F32Be => f32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Format::U8 => "8-bit",
            Format::U16Le => "16-bit LE",
            Format::U16Be => "16-bit BE",
            Format::F32Le => "float LE",
            Format::F32Be => "float BE",
        }
    }
}

/// splitmix64: a deterministic sample-position stream seeded by the byte length.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `count` positions below `bound` (repeats allowed — a vote, not a census).
    fn positions(&mut self, bound: usize, count: usize) -> Vec<usize> {
        (0..count.min(bound)).map(|_| (self.next() % bound as u64) as usize).collect()
    }
}

/// Interquartile range of `v` plus one scale unit, the normaliser that lets formats of different value ranges compete.
fn iqr(v: &[f32]) -> f32 {
    let mut s: Vec<f32> = v.to_vec();
    s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    s[s.len() / 4 * 3] - s[s.len() / 4] + 1. / 65536.
}

/// The format and channel-count vote: lowest normalised neighbour difference wins.
fn guess_layout(data: &[u8], rng: &mut Rng) -> Option<(Format, usize)> {
    // Every candidate needs a sample and its neighbour MAX_CHANNELS samples on, in the widest format.
    let reach = MAX_CHANNELS * 4 + 4;
    if data.len() <= reach {
        return None;
    }
    let raw = rng.positions(data.len() - reach, SAMPLES);
    let mut best: Option<(f32, Format, usize)> = None;
    for fmt in Format::ALL {
        let b = fmt.bytes();
        let pos: Vec<usize> = raw.iter().map(|&p| p - p % b).collect();
        let samples: Vec<f32> = pos.iter().map(|&p| fmt.read(data, p)).collect();
        if samples.iter().any(|v| !v.is_finite()) {
            continue;
        }
        let range = iqr(&samples);
        for channels in 1..=MAX_CHANNELS {
            let mut total = 0f32;
            for (&p, &s) in pos.iter().zip(&samples) {
                let next = fmt.read(data, p + channels * b);
                if next.is_finite() {
                    total += (s - next).abs();
                }
            }
            let score = total / samples.len() as f32 / range;
            // A zero score is a constant file — no evidence of anything, skip it like Dolphin did.
            if score > 1e-7 && best.is_none_or(|(bs, _, _)| score < bs) {
                best = Some((score, fmt, channels));
            }
        }
    }
    best.map(|(_, f, c)| (f, c))
}

/// The width vote over the luminance plane: mean vertical difference × aspect penalty, lowest wins. Returns `(width, mosaic)`.
fn guess_width(lum: &[f32], pair_lum: Option<&[f32]>, rng: &mut Rng) -> Option<(usize, bool)> {
    let n = lum.len();
    if n < 4 {
        return None;
    }
    let root = (n as f64).sqrt();
    let lo = ((root / ASPECT_BAND).floor() as usize).max(1);
    let hi = ((root * ASPECT_BAND).ceil() as usize).min(n / 2).max(lo + 1);
    let idx = rng.positions(n, SAMPLES);
    let scored: Vec<(f32, usize, bool)> = (lo..=hi)
        .into_par_iter()
        .filter_map(|width| {
            let height = n / width;
            if height < 2 {
                return None;
            }
            let weight = height.max(width) as f32 / height.min(width) as f32;
            let mut total = 0f32;
            let mut count = 0usize;
            for &i in &idx {
                if i >= width {
                    let d = (lum[i] - lum[i - width]).abs();
                    if d.is_finite() {
                        total += d;
                        count += 1;
                    }
                }
            }
            let mut best = (count > 0 && total > 0.).then(|| (total / count as f32 * weight, width, false));
            // The Bayer reading of a two-channel plane: rows two apart share a colour phase, so they cohere when the plain rows do not.
            if let Some(pl) = pair_lum {
                let mut mt = 0f32;
                let mut mc = 0usize;
                for &i in &idx {
                    if i >= width * 2 {
                        let d = (pl[i] - pl[i - width * 2]).abs();
                        if d.is_finite() {
                            mt += d;
                            mc += 1;
                        }
                    }
                }
                if mc > 0 && mt > 0. {
                    let ms = mt / mc as f32 * weight;
                    if best.is_none_or(|(bs, _, _)| ms < bs) {
                        best = Some((ms, width, true));
                    }
                }
            }
            best
        })
        .collect();
    scored.into_iter().min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)).map(|(_, w, m)| (w, m))
}

/// The 256th-darkest value: a black point that ignores a handful of dead pixels.
fn black_point(planes: &[Vec<f32>]) -> f32 {
    let mut lows: Vec<f32> = Vec::with_capacity(257);
    for p in planes {
        for &v in p {
            if !v.is_finite() {
                continue;
            }
            if lows.len() < 256 {
                lows.push(v);
                if lows.len() == 256 {
                    lows.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                }
            } else if v < lows[255] {
                let at = lows.partition_point(|&x| x < v);
                lows.insert(at, v);
                lows.pop();
            }
        }
    }
    if lows.len() < 256 {
        lows.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    }
    lows.last().copied().unwrap_or(0.)
}

/// Guess `bytes` as an image. `Err` only when there is too little to vote on.
pub fn guess(bytes: &[u8]) -> Result<Decoded, String> {
    let mut rng = Rng(bytes.len() as u64 ^ 0x6F70_7369_6E00_0000);
    let (fmt, channels) = guess_layout(bytes, &mut rng).ok_or_else(|| format!("{} bytes: too few to guess a layout", bytes.len()))?;
    let b = fmt.bytes();
    let n = bytes.len() / (b * channels);
    // Per-channel planes, unclamped (floats may hold anything; levels come later), plus the luminance vote plane.
    let planes: Vec<Vec<f32>> = (0..channels)
        .map(|c| (0..n).into_par_iter().map(|i| fmt.read(bytes, (i * channels + c) * b)).collect())
        .collect();
    let lum: Vec<f32> = (0..n).into_par_iter().map(|i| planes.iter().map(|p| p[i]).sum()).collect();
    let (width, mosaic) = guess_width(&lum, (channels == 2).then_some(lum.as_slice()), &mut rng).ok_or_else(|| format!("{n} pixels: no width to vote on"))?;
    let mut note = format!("{}, {channels} ch", fmt.label());

    // Three output planes, width × height.
    let (out_w, out_h, mut rgb): (usize, usize, [Vec<f32>; 3]) = if mosaic {
        let height = n / width / 2;
        // Tile positions: a = (even row, ch0), b = (even row, ch1), c = (odd row, ch0), d = (odd row, ch1). The pair that agrees best is the green pair; the map lays R, G, G, B over a..d.
        let idx = rng.positions(height * width, SAMPLES);
        let mut agree = [0f32; 6];
        for &i in &idx {
            let (y, x) = (i / width, i % width);
            let a = planes[0][(y * 2) * width + x];
            let bb = planes[1][(y * 2) * width + x];
            let c = planes[0][(y * 2 + 1) * width + x];
            let d = planes[1][(y * 2 + 1) * width + x];
            let rel = |p: f32, q: f32| if p + q != 0. { ((p - q) / (p + q)).abs() } else { 0. };
            agree[0] += rel(a, bb);
            agree[1] += rel(bb, c);
            agree[2] += rel(c, d);
            agree[3] += rel(a, c);
            agree[4] += rel(a, d);
            agree[5] += rel(bb, d);
        }
        let (pair, _) = agree.iter().enumerate().min_by(|x, y| x.1.partial_cmp(y.1).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or((5, &0.));
        // map[k] = which of a..d holds output slot k of [R, G1, G2, B]; the label is the tile read as a b / c d. Only the diagonal pairs are a real Bayer layout, and which of the other two is red is a coin the statistics cannot call, so red is the first of them.
        let (map, cfa) = match pair {
            0 => ([2, 0, 1, 3], "GGRB"),
            1 => ([0, 1, 2, 3], "RGGB"),
            2 => ([0, 3, 1, 2], "RBGG"),
            3 => ([1, 0, 2, 3], "GRGB"),
            4 => ([1, 0, 3, 2], "GRBG"),
            _ => ([0, 1, 3, 2], "RGBG"),
        };
        note.push_str(&format!(", CFA {cfa}"));
        let mut r = vec![0f32; width * height];
        let mut g = vec![0f32; width * height];
        let mut bl = vec![0f32; width * height];
        r.par_iter_mut().zip(g.par_iter_mut()).zip(bl.par_iter_mut()).enumerate().for_each(|(i, ((r, g), bl))| {
            let (y, x) = (i / width, i % width);
            let abcd = [planes[0][(y * 2) * width + x], planes[1][(y * 2) * width + x], planes[0][(y * 2 + 1) * width + x], planes[1][(y * 2 + 1) * width + x]];
            *r = abcd[map[0]];
            *g = (abcd[map[1]] + abcd[map[2]]) * 0.5;
            *bl = abcd[map[3]];
        });
        (width, height, [r, g, bl])
    } else {
        let height = n / width;
        let take = width * height;
        let pick = |c: usize| -> Vec<f32> { planes[c.min(channels - 1)][..take].to_vec() };
        // One channel replicates to grey; two or three fill in order; a fourth (alpha, most likely) is dropped.
        (width, height, [pick(0), pick(1), pick(2)])
    };

    // Levels: black = 256th-darkest, white = brightest finite value; store 16-bit between them.
    let black = black_point(&rgb);
    let white = rgb.iter().flat_map(|p| p.iter().copied()).filter(|v| v.is_finite()).fold(f32::MIN, f32::max);
    let scale = if white > black { 65535. / (white - black) } else { 1. };
    let mut planar = vec![0u16; out_w * out_h * 3];
    for (c, plane) in rgb.iter_mut().enumerate() {
        planar[c * out_w * out_h..(c + 1) * out_w * out_h].par_iter_mut().zip(plane.par_iter()).for_each(|(o, &v)| {
            *o = if v.is_finite() { ((v - black) * scale).round().clamp(0., 65535.) as u16 } else { 0 };
        });
    }
    let img = SpectralImage {
        width: out_w,
        height: out_h,
        channels: rgb_channel_names().into_iter().map(|name| SpectralChannel { name, curve: None }).collect(),
        layout: PlaneLayout::Planar,
        samples: BitPackedTensor::pack(16, vec![3, out_h, out_w], &planar),
        black: vec![0.; 3],
        white: vec![65535.; 3],
        make: "headerless".to_string(),
        model: note,
        provenance: Provenance::default(),
        profile: Some(ColourProfile {
            target: "vsf_rgb".to_string(),
            entries: vec![ProfileEntry {
                matrix: [1., 0., 0., 0., 1., 0., 0., 0., 1.],
                source: "headerless_assumed_vsf_rgb".to_string(),
                class: IdtClass::Absolute,
                grade: ProfileGrade::Assumed,
                illuminant: 0,
                transfer: Transfer::Linear,
            }],
            dng_colormatrix: [None, None],
            patches: None,
            cal: None,
        }),
        view: None,
    };
    Ok(Decoded { img, src_bits: (fmt.bytes() * 8) as u8 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth synthetic scene: a horizontal ramp plus a vertical ramp, distinct per channel, so rows cohere and columns cohere less.
    fn scene(w: usize, h: usize, c: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(w * h * c);
        for y in 0..h {
            for x in 0..w {
                for k in 0..c {
                    v.push(((x as f32 / w as f32) * 0.5 + (y as f32 / h as f32) * 0.3 + k as f32 * 0.1).min(0.999));
                }
            }
        }
        v
    }

    #[test]
    fn recovers_width_and_channels_of_an_rgb8_dump() {
        let (w, h) = (317, 211); // 317 is prime: Dolphin's divisor-only search would have panicked.
        let bytes: Vec<u8> = scene(w, h, 3).iter().map(|v| (v * 255.) as u8).collect();
        let dec = guess(&bytes).unwrap();
        assert_eq!((dec.img.width, dec.img.height), (w, h));
        assert_eq!(dec.img.make, "headerless");
        assert!(dec.img.model.starts_with("8-bit, 3 ch"), "{}", dec.img.model);
    }

    #[test]
    fn recovers_a_u16_le_mono_dump() {
        let (w, h) = (640, 480);
        let bytes: Vec<u8> = scene(w, h, 1).iter().flat_map(|v| ((v * 65535.) as u16).to_le_bytes()).collect();
        let dec = guess(&bytes).unwrap();
        assert_eq!((dec.img.width, dec.img.height), (w, h));
        assert!(dec.img.model.starts_with("16-bit LE, 1 ch"), "{}", dec.img.model);
        // Grey replicates: the three planes agree.
        let all = dec.img.samples.unpack_u16();
        let n = w * h;
        assert_eq!(all[n / 2], all[n + n / 2]);
        assert_eq!(all[n / 2], all[2 * n + n / 2]);
    }

    #[test]
    fn recovers_a_float_be_rgb_dump() {
        let (w, h) = (200, 150);
        let bytes: Vec<u8> = scene(w, h, 3).iter().flat_map(|v| v.to_be_bytes()).collect();
        let dec = guess(&bytes).unwrap();
        assert_eq!((dec.img.width, dec.img.height), (w, h));
        assert!(dec.img.model.starts_with("float BE, 3 ch"), "{}", dec.img.model);
    }

    #[test]
    fn anything_at_all_becomes_a_near_square_picture() {
        // Pseudo-random bytes: no width is right, so the aspect penalty settles it near square, and nothing panics.
        let mut r = Rng(7);
        let bytes: Vec<u8> = (0..100_003).map(|_| (r.next() & 0xFF) as u8).collect();
        let dec = guess(&bytes).unwrap();
        let (w, h) = (dec.img.width as f64, dec.img.height as f64);
        assert!(w > 0. && h > 0.);
        assert!((w / h).max(h / w) <= ASPECT_BAND, "{w}×{h}");
        // And a file too short to vote on says so instead of panicking.
        assert!(guess(&[1, 2, 3]).is_err());
    }

    #[test]
    fn levels_stretch_black_to_white() {
        let (w, h) = (64, 64);
        let bytes: Vec<u8> = scene(w, h, 3).iter().map(|v| (v * 100. + 50.) as u8).collect();
        let dec = guess(&bytes).unwrap();
        let all = dec.img.samples.unpack_u16();
        assert_eq!(*all.iter().max().unwrap(), 65535);
        assert!(all.iter().filter(|&&v| v == 0).count() >= 1);
    }
}
