//! Display encode of a linear RGB buffer: an exposure gain in stops, then a gamma-2 lookup into fluor's α + darkness pixel (`0xFF` ‖ `255 − channel`). The library twin of the viewer's own encode.

use rayon::prelude::*;

/// Linear i32 RGB (0 = black, 65535 = the profile's white) → packed pixels at `ev` stops of exposure. `clip_show` paints a blown channel black and a crushed one white, the raw-inversion convention.
pub fn encode_pixels(lin: &[i32], ev: f32, clip_show: bool) -> Vec<u32> {
    const GAIN_SHIFT: u32 = 1 << 4;
    static LUT: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let lut = LUT.get_or_init(|| (0..65536u32).map(|v| 255 - ((v as f32 / 65535.).sqrt() * 255.) as u32).collect());
    let gain = (2f64.powf(ev as f64) * (1u64 << GAIN_SHIFT) as f64).round() as i64;
    lin.par_chunks_exact(3)
        .map(|px| {
            let ch = |v: i32| {
                let g = (v as i64 * gain) >> GAIN_SHIFT;
                let idx = if clip_show {
                    if g >= 65535 { 0 } else if g < 0 { 65535 } else { g }
                } else {
                    g.clamp(0, 65535)
                };
                lut[idx as usize]
            };
            0xFF000000 | (ch(px[0]) << 16) | (ch(px[1]) << 8) | ch(px[2])
        })
        .collect()
}
