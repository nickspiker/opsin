//! RCD (Ratio Corrected Demosaicing), release 2.3 @ 171125 — a Rust port of Luis Sanz Rodríguez's
//! implementation, vendored verbatim from lumis (`rust/src/debayer/rcd.rs`), where it is the live
//! demosaicer behind every full-resolution export. opsin's viewer bins each CFA tile to one pixel —
//! fast, and all a fit view needs — but an exported JPEG is a deliverable at the sensor's own
//! resolution, so the export path runs this instead (Nick 2026-10-05: "for the jpeg export, it's
//! pretty small, are we debayering or just RGGB binning?").
//!
//! (There is an AMaZE port sitting in both lumis and oriel, but it is dead source in each: neither
//! declares the module, and it references host types — `FilterPattern`, `ImageBuffer`, `ImageMode` —
//! that exist in neither crate, so it has never been compiled, let alone verified. RCD is the one
//! that actually runs.)
/// Frame dimensions — all the demosaicer needs of the source (lumis keeps the same split so this
/// module stays dependency-free; vendored into opsin whole, 2026-10-05).
pub struct RawImage {
    pub width: usize,
    pub height: usize,
}

impl RawImage {
    pub fn new(width: usize, height: usize) -> Self {
        RawImage { width, height }
    }
}

/// Represents demosaic data for RCD processing
pub struct RcdData {
    /// Image data stored as [height][width][3] where 3 is RGB channels
    pub data: Vec<Vec<[u16; 3]>>,
    /// CFA pattern function - returns colour index (0=R, 1=G, 2=B) at given position
    pub fc: Box<dyn Fn(usize, usize) -> usize>,
}

impl RcdData {
    /// Create a new RCD data structure with given dimensions
    pub fn new(width: usize, height: usize, fc: Box<dyn Fn(usize, usize) -> usize>) -> Self {
        Self {
            data: vec![vec![[0u16; 3]; width]; height],
            fc,
        }
    }
    /// Perform RCD demosaicing on the raw image
    pub fn rcd_demosaic(&mut self, raw_image: &RawImage) {
        // Constants
        const EPS: f32 = 1e-5;
        const EPSSQ: f32 = 1e-10;

        // Width constants for row offsets
        let w1 = raw_image.width;
        let w2 = 2 * raw_image.width;
        let w3 = 3 * raw_image.width;
        let w4 = 4 * raw_image.width;

        // Convert CFA to floating point buffers
        let mut cfa = vec![0.0f32; raw_image.width * raw_image.height];
        let mut rgb = vec![[0.0f32; 3]; raw_image.width * raw_image.height];

        // Initialize CFA buffer from raw data
        for row in 0..raw_image.height {
            for col in 0..raw_image.width {
                let indx = row * raw_image.width + col;
                let fc = (self.fc)(row, col);
                cfa[indx] = self.data[row][col][fc] as f32 / 65535.0;
                rgb[indx][fc] = cfa[indx];
            }
        }

        // STEP 1: Find vertical and horizontal interpolation directions
        let mut vh_dir = vec![0.0f32; raw_image.width * raw_image.height];

        // Step 1.1: Calculate vertical and horizontal local discrimination
        for row in 4..raw_image.height.saturating_sub(4) {
            for col in 4..raw_image.width.saturating_sub(4) {
                let indx = row * raw_image.width + col;

                // Safe bounds checking for array access
                let v_stat = self.calculate_v_stat(&cfa, indx, w1, w2, w3, w4).max(EPSSQ);
                let h_stat = self.calculate_h_stat(&cfa, indx).max(EPSSQ);

                vh_dir[indx] = v_stat / (v_stat + h_stat);
            }
        }

        // STEP 2: Calculate the low pass filter
        let mut lpf = vec![0.0f32; raw_image.width * raw_image.height];

        for row in 2..raw_image.height.saturating_sub(2) {
            let start_col = 2 + ((self.fc)(row, 0) & 1);
            for col in (start_col..raw_image.width.saturating_sub(2)).step_by(2) {
                let indx = row * raw_image.width + col;

                lpf[indx] = 0.25 * cfa[indx]
                    + 0.125 * (cfa[indx - w1] + cfa[indx + w1] + cfa[indx - 1] + cfa[indx + 1])
                    + 0.0625
                        * (cfa[indx - w1 - 1]
                            + cfa[indx - w1 + 1]
                            + cfa[indx + w1 - 1]
                            + cfa[indx + w1 + 1]);
            }
        }

        // STEP 3: Populate the green channel
        for row in 4..raw_image.height.saturating_sub(4) {
            let start_col = 4 + ((self.fc)(row, 0) & 1);
            for col in (start_col..raw_image.width.saturating_sub(4)).step_by(2) {
                let indx = row * raw_image.width + col;

                // Refined vertical and horizontal local discrimination
                let vh_central = vh_dir[indx];
                let vh_neighbourhood = 0.25
                    * (vh_dir[indx - w1 - 1]
                        + vh_dir[indx - w1 + 1]
                        + vh_dir[indx + w1 - 1]
                        + vh_dir[indx + w1 + 1]);

                let vh_disc = if (0.5 - vh_central).abs() < (0.5 - vh_neighbourhood).abs() {
                    vh_neighbourhood
                } else {
                    vh_central
                };

                // Cardinal gradients
                let n_grad = EPS
                    + (cfa[indx - w1] - cfa[indx + w1]).abs()
                    + (cfa[indx] - cfa[indx - w2]).abs()
                    + (cfa[indx - w1] - cfa[indx - w3]).abs()
                    + (cfa[indx - w2] - cfa[indx - w4]).abs();

                let s_grad = EPS
                    + (cfa[indx + w1] - cfa[indx - w1]).abs()
                    + (cfa[indx] - cfa[indx + w2]).abs()
                    + (cfa[indx + w1] - cfa[indx + w3]).abs()
                    + (cfa[indx + w2] - cfa[indx + w4]).abs();

                let w_grad = EPS
                    + (cfa[indx - 1] - cfa[indx + 1]).abs()
                    + (cfa[indx] - cfa[indx - 2]).abs()
                    + (cfa[indx - 1] - cfa[indx - 3]).abs()
                    + (cfa[indx - 2] - cfa[indx - 4]).abs();

                let e_grad = EPS
                    + (cfa[indx + 1] - cfa[indx - 1]).abs()
                    + (cfa[indx] - cfa[indx + 2]).abs()
                    + (cfa[indx + 1] - cfa[indx + 3]).abs()
                    + (cfa[indx + 2] - cfa[indx + 4]).abs();

                // Cardinal pixel estimations
                let n_est = cfa[indx - w1]
                    * (1.0 + (lpf[indx] - lpf[indx - w2]) / (EPS + lpf[indx] + lpf[indx - w2]));
                let s_est = cfa[indx + w1]
                    * (1.0 + (lpf[indx] - lpf[indx + w2]) / (EPS + lpf[indx] + lpf[indx + w2]));
                let w_est = cfa[indx - 1]
                    * (1.0 + (lpf[indx] - lpf[indx - 2]) / (EPS + lpf[indx] + lpf[indx - 2]));
                let e_est = cfa[indx + 1]
                    * (1.0 + (lpf[indx] - lpf[indx + 2]) / (EPS + lpf[indx] + lpf[indx + 2]));

                // Vertical and horizontal estimations
                let v_est = (s_grad * n_est + n_grad * s_est) / (n_grad + s_grad);
                let h_est = (w_grad * e_est + e_grad * w_est) / (e_grad + w_grad);

                // G@B and G@R interpolation
                rgb[indx][1] = (vh_disc * h_est + (1.0 - vh_disc) * v_est).clamp(0.0, 1.0);
            }
        }

        // STEP 4: Populate the red and blue channels

        // Step 4.1: Calculate P/Q diagonal local discrimination
        let mut pq_dir = vec![0.0f32; raw_image.width * raw_image.height];

        for row in 4..raw_image.height.saturating_sub(4) {
            let start_col = 4 + ((self.fc)(row, 0) & 1);
            for col in (start_col..raw_image.width.saturating_sub(4)).step_by(2) {
                let indx = row * raw_image.width + col;

                let p_stat = self.calculate_p_stat(&cfa, indx, w1, w2, w3, w4).max(EPSSQ);
                let q_stat = self.calculate_q_stat(&cfa, indx, w1, w2, w3, w4).max(EPSSQ);

                pq_dir[indx] = p_stat / (p_stat + q_stat);
            }
        }

        // Step 4.2: Populate red and blue channels at blue and red CFA positions
        for row in 4..raw_image.height.saturating_sub(4) {
            let start_col = 4 + ((self.fc)(row, 0) & 1);
            for col in (start_col..raw_image.width.saturating_sub(4)).step_by(2) {
                let indx = row * raw_image.width + col;
                let c = 2 - (self.fc)(row, col);

                // Refined P/Q diagonal local discrimination
                let pq_central = pq_dir[indx];
                let pq_neighbourhood = 0.25
                    * (pq_dir[indx - w1 - 1]
                        + pq_dir[indx - w1 + 1]
                        + pq_dir[indx + w1 - 1]
                        + pq_dir[indx + w1 + 1]);

                let pq_disc = if (0.5 - pq_central).abs() < (0.5 - pq_neighbourhood).abs() {
                    pq_neighbourhood
                } else {
                    pq_central
                };

                // Diagonal gradients
                let nw_grad = EPS
                    + (rgb[indx - w1 - 1][c] - rgb[indx + w1 + 1][c]).abs()
                    + (rgb[indx - w1 - 1][c] - rgb[indx - w3 - 3][c]).abs()
                    + (rgb[indx][1] - rgb[indx - w2 - 2][1]).abs();

                let ne_grad = EPS
                    + (rgb[indx - w1 + 1][c] - rgb[indx + w1 - 1][c]).abs()
                    + (rgb[indx - w1 + 1][c] - rgb[indx - w3 + 3][c]).abs()
                    + (rgb[indx][1] - rgb[indx - w2 + 2][1]).abs();

                let sw_grad = EPS
                    + (rgb[indx + w1 - 1][c] - rgb[indx - w1 + 1][c]).abs()
                    + (rgb[indx + w1 - 1][c] - rgb[indx + w3 - 3][c]).abs()
                    + (rgb[indx][1] - rgb[indx + w2 - 2][1]).abs();

                let se_grad = EPS
                    + (rgb[indx + w1 + 1][c] - rgb[indx - w1 - 1][c]).abs()
                    + (rgb[indx + w1 + 1][c] - rgb[indx + w3 + 3][c]).abs()
                    + (rgb[indx][1] - rgb[indx + w2 + 2][1]).abs();

                // Diagonal colour differences
                let nw_est = rgb[indx - w1 - 1][c] - rgb[indx - w1 - 1][1];
                let ne_est = rgb[indx - w1 + 1][c] - rgb[indx - w1 + 1][1];
                let sw_est = rgb[indx + w1 - 1][c] - rgb[indx + w1 - 1][1];
                let se_est = rgb[indx + w1 + 1][c] - rgb[indx + w1 + 1][1];

                // P/Q estimations
                let p_est = (nw_grad * se_est + se_grad * nw_est) / (nw_grad + se_grad);
                let q_est = (ne_grad * sw_est + sw_grad * ne_est) / (ne_grad + sw_grad);

                // R@B and B@R interpolation
                rgb[indx][c] =
                    (rgb[indx][1] + (1.0 - pq_disc) * p_est + pq_disc * q_est).clamp(0.0, 1.0);
            }
        }

        // Step 4.3: Populate red and blue channels at green CFA positions
        for row in 4..raw_image.height.saturating_sub(4) {
            let start_col = 4 + ((self.fc)(row, 1) & 1);
            for col in (start_col..raw_image.width.saturating_sub(4)).step_by(2) {
                let indx = row * raw_image.width + col;

                // Refined vertical and horizontal local discrimination
                let vh_central = vh_dir[indx];
                let vh_neighbourhood = 0.25
                    * (vh_dir[indx - w1 - 1]
                        + vh_dir[indx - w1 + 1]
                        + vh_dir[indx + w1 - 1]
                        + vh_dir[indx + w1 + 1]);

                let vh_disc = if (0.5 - vh_central).abs() < (0.5 - vh_neighbourhood).abs() {
                    vh_neighbourhood
                } else {
                    vh_central
                };

                for c in [0, 2] {
                    // Cardinal gradients
                    let n_grad = EPS
                        + (rgb[indx][1] - rgb[indx - w2][1]).abs()
                        + (rgb[indx - w1][c] - rgb[indx + w1][c]).abs()
                        + (rgb[indx - w1][c] - rgb[indx - w3][c]).abs();

                    let s_grad = EPS
                        + (rgb[indx][1] - rgb[indx + w2][1]).abs()
                        + (rgb[indx + w1][c] - rgb[indx - w1][c]).abs()
                        + (rgb[indx + w1][c] - rgb[indx + w3][c]).abs();

                    let w_grad = EPS
                        + (rgb[indx][1] - rgb[indx - 2][1]).abs()
                        + (rgb[indx - 1][c] - rgb[indx + 1][c]).abs()
                        + (rgb[indx - 1][c] - rgb[indx - 3][c]).abs();

                    let e_grad = EPS
                        + (rgb[indx][1] - rgb[indx + 2][1]).abs()
                        + (rgb[indx + 1][c] - rgb[indx - 1][c]).abs()
                        + (rgb[indx + 1][c] - rgb[indx + 3][c]).abs();

                    // Cardinal colour differences
                    let n_est = rgb[indx - w1][c] - rgb[indx - w1][1];
                    let s_est = rgb[indx + w1][c] - rgb[indx + w1][1];
                    let w_est = rgb[indx - 1][c] - rgb[indx - 1][1];
                    let e_est = rgb[indx + 1][c] - rgb[indx + 1][1];

                    // Vertical and horizontal estimations
                    let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
                    let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);

                    // R@G and B@G interpolation
                    rgb[indx][c] =
                        (rgb[indx][1] + (1.0 - vh_disc) * v_est + vh_disc * h_est).clamp(0.0, 1.0);
                }
            }
        }

        // Convert floating point buffers back to u16 image data
        for row in 0..raw_image.height {
            for col in 0..raw_image.width {
                let indx = row * raw_image.width + col;
                for c in 0..3 {
                    // No clamp: `as u16` saturates (>65535 -> 65535, negative/NaN -> 0). Verified equivalent to clamp(0,65535) over all f32.
                    self.data[row][col][c] = (65535.0 * rgb[indx][c]) as u16;
                }
            }
        }

        // Fill the 4px border RCD's interior loops (4..n-4) leave unprocessed by replicating the nearest interior pixel. Without this the border carries only the raw CFA channel (other two channels zero) -> coloured/black fringes. The region helper adds margin so this border lands outside the requested crop, but at sensor edges the margin is clamped, so a correct border fill matters there.
        if raw_image.width > 8 && raw_image.height > 8 {
            self.border_interpolate(raw_image, 4);
        }
    }

    /// Calculate V statistic for vertical/horizontal discrimination
    fn calculate_v_stat(
        &self,
        cfa: &[f32],
        indx: usize,
        w1: usize,
        w2: usize,
        w3: usize,
        w4: usize,
    ) -> f32 {
        -18.0 * cfa[indx] * cfa[indx - w1]
            - 18.0 * cfa[indx] * cfa[indx + w1]
            - 36.0 * cfa[indx] * cfa[indx - w2]
            - 36.0 * cfa[indx] * cfa[indx + w2]
            + 18.0 * cfa[indx] * cfa[indx - w3]
            + 18.0 * cfa[indx] * cfa[indx + w3]
            - 2.0 * cfa[indx] * cfa[indx - w4]
            - 2.0 * cfa[indx] * cfa[indx + w4]
            + 38.0 * cfa[indx] * cfa[indx]
            - 70.0 * cfa[indx - w1] * cfa[indx + w1]
            - 12.0 * cfa[indx - w1] * cfa[indx - w2]
            + 24.0 * cfa[indx - w1] * cfa[indx + w2]
            - 38.0 * cfa[indx - w1] * cfa[indx - w3]
            + 16.0 * cfa[indx - w1] * cfa[indx + w3]
            + 12.0 * cfa[indx - w1] * cfa[indx - w4]
            - 6.0 * cfa[indx - w1] * cfa[indx + w4]
            + 46.0 * cfa[indx - w1] * cfa[indx - w1]
            + 24.0 * cfa[indx + w1] * cfa[indx - w2]
            - 12.0 * cfa[indx + w1] * cfa[indx + w2]
            + 16.0 * cfa[indx + w1] * cfa[indx - w3]
            - 38.0 * cfa[indx + w1] * cfa[indx + w3]
            - 6.0 * cfa[indx + w1] * cfa[indx - w4]
            + 12.0 * cfa[indx + w1] * cfa[indx + w4]
            + 46.0 * cfa[indx + w1] * cfa[indx + w1]
            + 14.0 * cfa[indx - w2] * cfa[indx + w2]
            - 12.0 * cfa[indx - w2] * cfa[indx + w3]
            - 2.0 * cfa[indx - w2] * cfa[indx - w4]
            + 2.0 * cfa[indx - w2] * cfa[indx + w4]
            + 11.0 * cfa[indx - w2] * cfa[indx - w2]
            - 12.0 * cfa[indx + w2] * cfa[indx - w3]
            + 2.0 * cfa[indx + w2] * cfa[indx - w4]
            - 2.0 * cfa[indx + w2] * cfa[indx + w4]
            + 11.0 * cfa[indx + w2] * cfa[indx + w2]
            + 2.0 * cfa[indx - w3] * cfa[indx + w3]
            - 6.0 * cfa[indx - w3] * cfa[indx - w4]
            + 10.0 * cfa[indx - w3] * cfa[indx - w3]
            - 6.0 * cfa[indx + w3] * cfa[indx + w4]
            + 10.0 * cfa[indx + w3] * cfa[indx + w3]
            + 1.0 * cfa[indx - w4] * cfa[indx - w4]
            + 1.0 * cfa[indx + w4] * cfa[indx + w4]
    }

    /// Calculate H statistic for vertical/horizontal discrimination
    fn calculate_h_stat(&self, cfa: &[f32], indx: usize) -> f32 {
        -18.0 * cfa[indx] * cfa[indx - 1]
            - 18.0 * cfa[indx] * cfa[indx + 1]
            - 36.0 * cfa[indx] * cfa[indx - 2]
            - 36.0 * cfa[indx] * cfa[indx + 2]
            + 18.0 * cfa[indx] * cfa[indx - 3]
            + 18.0 * cfa[indx] * cfa[indx + 3]
            - 2.0 * cfa[indx] * cfa[indx - 4]
            - 2.0 * cfa[indx] * cfa[indx + 4]
            + 38.0 * cfa[indx] * cfa[indx]
            - 70.0 * cfa[indx - 1] * cfa[indx + 1]
            - 12.0 * cfa[indx - 1] * cfa[indx - 2]
            + 24.0 * cfa[indx - 1] * cfa[indx + 2]
            - 38.0 * cfa[indx - 1] * cfa[indx - 3]
            + 16.0 * cfa[indx - 1] * cfa[indx + 3]
            + 12.0 * cfa[indx - 1] * cfa[indx - 4]
            - 6.0 * cfa[indx - 1] * cfa[indx + 4]
            + 46.0 * cfa[indx - 1] * cfa[indx - 1]
            + 24.0 * cfa[indx + 1] * cfa[indx - 2]
            - 12.0 * cfa[indx + 1] * cfa[indx + 2]
            + 16.0 * cfa[indx + 1] * cfa[indx - 3]
            - 38.0 * cfa[indx + 1] * cfa[indx + 3]
            - 6.0 * cfa[indx + 1] * cfa[indx - 4]
            + 12.0 * cfa[indx + 1] * cfa[indx + 4]
            + 46.0 * cfa[indx + 1] * cfa[indx + 1]
            + 14.0 * cfa[indx - 2] * cfa[indx + 2]
            - 12.0 * cfa[indx - 2] * cfa[indx + 3]
            - 2.0 * cfa[indx - 2] * cfa[indx - 4]
            + 2.0 * cfa[indx - 2] * cfa[indx + 4]
            + 11.0 * cfa[indx - 2] * cfa[indx - 2]
            - 12.0 * cfa[indx + 2] * cfa[indx - 3]
            + 2.0 * cfa[indx + 2] * cfa[indx - 4]
            - 2.0 * cfa[indx + 2] * cfa[indx + 4]
            + 11.0 * cfa[indx + 2] * cfa[indx + 2]
            + 2.0 * cfa[indx - 3] * cfa[indx + 3]
            - 6.0 * cfa[indx - 3] * cfa[indx - 4]
            + 10.0 * cfa[indx - 3] * cfa[indx - 3]
            - 6.0 * cfa[indx + 3] * cfa[indx + 4]
            + 10.0 * cfa[indx + 3] * cfa[indx + 3]
            + 1.0 * cfa[indx - 4] * cfa[indx - 4]
            + 1.0 * cfa[indx + 4] * cfa[indx + 4]
    }

    /// Calculate P statistic for diagonal discrimination
    fn calculate_p_stat(
        &self,
        cfa: &[f32],
        indx: usize,
        w1: usize,
        w2: usize,
        w3: usize,
        w4: usize,
    ) -> f32 {
        -18.0 * cfa[indx] * cfa[indx - w1 - 1]
            - 18.0 * cfa[indx] * cfa[indx + w1 + 1]
            - 36.0 * cfa[indx] * cfa[indx - w2 - 2]
            - 36.0 * cfa[indx] * cfa[indx + w2 + 2]
            + 18.0 * cfa[indx] * cfa[indx - w3 - 3]
            + 18.0 * cfa[indx] * cfa[indx + w3 + 3]
            - 2.0 * cfa[indx] * cfa[indx - w4 - 4]
            - 2.0 * cfa[indx] * cfa[indx + w4 + 4]
            + 38.0 * cfa[indx] * cfa[indx]
            - 70.0 * cfa[indx - w1 - 1] * cfa[indx + w1 + 1]
            - 12.0 * cfa[indx - w1 - 1] * cfa[indx - w2 - 2]
            + 24.0 * cfa[indx - w1 - 1] * cfa[indx + w2 + 2]
            - 38.0 * cfa[indx - w1 - 1] * cfa[indx - w3 - 3]
            + 16.0 * cfa[indx - w1 - 1] * cfa[indx + w3 + 3]
            + 12.0 * cfa[indx - w1 - 1] * cfa[indx - w4 - 4]
            - 6.0 * cfa[indx - w1 - 1] * cfa[indx + w4 + 4]
            + 46.0 * cfa[indx - w1 - 1] * cfa[indx - w1 - 1]
            + 24.0 * cfa[indx + w1 + 1] * cfa[indx - w2 - 2]
            - 12.0 * cfa[indx + w1 + 1] * cfa[indx + w2 + 2]
            + 16.0 * cfa[indx + w1 + 1] * cfa[indx - w3 - 3]
            - 38.0 * cfa[indx + w1 + 1] * cfa[indx + w3 + 3]
            - 6.0 * cfa[indx + w1 + 1] * cfa[indx - w4 - 4]
            + 12.0 * cfa[indx + w1 + 1] * cfa[indx + w4 + 4]
            + 46.0 * cfa[indx + w1 + 1] * cfa[indx + w1 + 1]
            + 14.0 * cfa[indx - w2 - 2] * cfa[indx + w2 + 2]
            - 12.0 * cfa[indx - w2 - 2] * cfa[indx + w3 + 3]
            - 2.0 * cfa[indx - w2 - 2] * cfa[indx - w4 - 4]
            + 2.0 * cfa[indx - w2 - 2] * cfa[indx + w4 + 4]
            + 11.0 * cfa[indx - w2 - 2] * cfa[indx - w2 - 2]
            - 12.0 * cfa[indx + w2 + 2] * cfa[indx - w3 - 3]
            + 2.0 * cfa[indx + w2 + 2] * cfa[indx - w4 - 4]
            - 2.0 * cfa[indx + w2 + 2] * cfa[indx + w4 + 4]
            + 11.0 * cfa[indx + w2 + 2] * cfa[indx + w2 + 2]
            + 2.0 * cfa[indx - w3 - 3] * cfa[indx + w3 + 3]
            - 6.0 * cfa[indx - w3 - 3] * cfa[indx - w4 - 4]
            + 10.0 * cfa[indx - w3 - 3] * cfa[indx - w3 - 3]
            - 6.0 * cfa[indx + w3 + 3] * cfa[indx + w4 + 4]
            + 10.0 * cfa[indx + w3 + 3] * cfa[indx + w3 + 3]
            + 1.0 * cfa[indx - w4 - 4] * cfa[indx - w4 - 4]
            + 1.0 * cfa[indx + w4 + 4] * cfa[indx + w4 + 4]
    }

    /// Calculate Q statistic for diagonal discrimination
    fn calculate_q_stat(
        &self,
        cfa: &[f32],
        indx: usize,
        w1: usize,
        w2: usize,
        w3: usize,
        w4: usize,
    ) -> f32 {
        -18.0 * cfa[indx] * cfa[indx + w1 - 1]
            - 18.0 * cfa[indx] * cfa[indx - w1 + 1]
            - 36.0 * cfa[indx] * cfa[indx + w2 - 2]
            - 36.0 * cfa[indx] * cfa[indx - w2 + 2]
            + 18.0 * cfa[indx] * cfa[indx + w3 - 3]
            + 18.0 * cfa[indx] * cfa[indx - w3 + 3]
            - 2.0 * cfa[indx] * cfa[indx + w4 - 4]
            - 2.0 * cfa[indx] * cfa[indx - w4 + 4]
            + 38.0 * cfa[indx] * cfa[indx]
            - 70.0 * cfa[indx + w1 - 1] * cfa[indx - w1 + 1]
            - 12.0 * cfa[indx + w1 - 1] * cfa[indx + w2 - 2]
            + 24.0 * cfa[indx + w1 - 1] * cfa[indx - w2 + 2]
            - 38.0 * cfa[indx + w1 - 1] * cfa[indx + w3 - 3]
            + 16.0 * cfa[indx + w1 - 1] * cfa[indx - w3 + 3]
            + 12.0 * cfa[indx + w1 - 1] * cfa[indx + w4 - 4]
            - 6.0 * cfa[indx + w1 - 1] * cfa[indx - w4 + 4]
            + 46.0 * cfa[indx + w1 - 1] * cfa[indx + w1 - 1]
            + 24.0 * cfa[indx - w1 + 1] * cfa[indx + w2 - 2]
            - 12.0 * cfa[indx - w1 + 1] * cfa[indx - w2 + 2]
            + 16.0 * cfa[indx - w1 + 1] * cfa[indx + w3 - 3]
            - 38.0 * cfa[indx - w1 + 1] * cfa[indx - w3 + 3]
            - 6.0 * cfa[indx - w1 + 1] * cfa[indx + w4 - 4]
            + 12.0 * cfa[indx - w1 + 1] * cfa[indx - w4 + 4]
            + 46.0 * cfa[indx - w1 + 1] * cfa[indx - w1 + 1]
            + 14.0 * cfa[indx + w2 - 2] * cfa[indx - w2 + 2]
            - 12.0 * cfa[indx + w2 - 2] * cfa[indx - w3 + 3]
            - 2.0 * cfa[indx + w2 - 2] * cfa[indx + w4 - 4]
            + 2.0 * cfa[indx + w2 - 2] * cfa[indx - w4 + 4]
            + 11.0 * cfa[indx + w2 - 2] * cfa[indx + w2 - 2]
            - 12.0 * cfa[indx - w2 + 2] * cfa[indx + w3 - 3]
            + 2.0 * cfa[indx - w2 + 2] * cfa[indx + w4 - 4]
            - 2.0 * cfa[indx - w2 + 2] * cfa[indx - w4 + 4]
            + 11.0 * cfa[indx - w2 + 2] * cfa[indx - w2 + 2]
            + 2.0 * cfa[indx + w3 - 3] * cfa[indx - w3 + 3]
            - 6.0 * cfa[indx + w3 - 3] * cfa[indx + w4 - 4]
            + 10.0 * cfa[indx + w3 - 3] * cfa[indx + w3 - 3]
            - 6.0 * cfa[indx - w3 + 3] * cfa[indx - w4 + 4]
            + 10.0 * cfa[indx - w3 + 3] * cfa[indx - w3 + 3]
            + 1.0 * cfa[indx + w4 - 4] * cfa[indx + w4 - 4]
            + 1.0 * cfa[indx - w4 + 4] * cfa[indx - w4 + 4]
    }

    /// Interpolate border pixels
    fn border_interpolate(&mut self, raw_image: &RawImage, border: usize) {
        // Top border
        for row in 0..border {
            for col in 0..raw_image.width {
                let source_row = border;
                for c in 0..3 {
                    self.data[row][col][c] = self.data[source_row][col][c];
                }
            }
        }

        // Bottom border
        for row in (raw_image.height - border)..raw_image.height {
            for col in 0..raw_image.width {
                let source_row = raw_image.height - border - 1;
                for c in 0..3 {
                    self.data[row][col][c] = self.data[source_row][col][c];
                }
            }
        }

        // Left border
        for row in 0..raw_image.height {
            for col in 0..border {
                let source_col = border;
                for c in 0..3 {
                    self.data[row][col][c] = self.data[row][source_col][c];
                }
            }
        }

        // Right border
        for row in 0..raw_image.height {
            for col in (raw_image.width - border)..raw_image.width {
                let source_col = raw_image.width - border - 1;
                for c in 0..3 {
                    self.data[row][col][c] = self.data[row][source_col][c];
                }
            }
        }
    }
}

// /// Example Bayer pattern function for RGGB
// pub fn bayer_rggb(row: usize, col: usize) -> usize {
//     match (row & 1, col & 1) {
//         (0, 0) => 0, // R
//         (0, 1) => 1, // G
//         (1, 0) => 1, // G
//         (1, 1) => 2, // B
//         _ => unreachable!(),
//     }
// }

// /// Example usage
// #[cfg(test)]
// mod tests {
//     use super::*;

//     #[test]
//     fn test_rcd_demosaic() {
//         // Create a test image with Bayer RGGB pattern
//         let mut img = RawImage::new(100, 100, Box::new(bayer_rggb));

//         // Fill with some test CFA data
//         for row in 0..100 {
//             for col in 0..100 {
//                 let fc = bayer_rggb(row, col);
//                 // Simple gradient pattern for testing
//                 img.data[row][col][fc] = ((row + col) * 256) as u16;
//             }
//         }

//         // Apply RCD demosaicing
//         img.rcd_demosaic();

//         // Check that all channels have been populated
//         for row in 0..100 {
//             for col in 0..100 {
//                 assert!(img.data[row][col][0] > 0 || row == 0 || col == 0);
//                 assert!(img.data[row][col][1] > 0 || row == 0 || col == 0);
//                 assert!(img.data[row][col][2] > 0 || row == 0 || col == 0);
//             }
//         }
//     }
// }
