use std::f32::consts::PI;

/// Filter shapes available for each band, same set you'd see on a
/// theatre console's channel EQ strip.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum FilterKind {
    Peak,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
}

impl FilterKind {
    pub const ALL: [FilterKind; 5] = [
        FilterKind::Peak,
        FilterKind::LowShelf,
        FilterKind::HighShelf,
        FilterKind::LowPass,
        FilterKind::HighPass,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            FilterKind::Peak => "Peak",
            FilterKind::LowShelf => "Low Shelf",
            FilterKind::HighShelf => "High Shelf",
            FilterKind::LowPass => "Low Pass",
            FilterKind::HighPass => "High Pass",
        }
    }
}

/// Direct Form I biquad. One instance per channel per band.
#[derive(Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    #[inline(always)]
    pub fn process(&mut self, x0: f32) -> f32 {
        let y0 = self.b0 * x0 + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x0;
        self.y2 = self.y1;
        self.y1 = y0;
        y0
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    /// Frequency response |H(f)| in dB from the currently stored coefficients,
    /// evaluated at `z^-1 = e^{-i 2π f / fs}`. Used to draw the real console
    /// curve (each band's Q controls how wide its bump or notch is).
    pub fn magnitude_db(&self, freq: f32, sample_rate: f32) -> f32 {
        let f = freq.clamp(10.0, sample_rate * 0.49);
        let w = 2.0 * PI * f / sample_rate;
        let (z1r, z1i) = (w.cos(), -w.sin()); // z^-1 = cos(w) - i sin(w)
        let z2r = z1r * z1r - z1i * z1i; // z^-2
        let z2i = 2.0 * z1r * z1i;

        let num_r = self.b0 + self.b1 * z1r + self.b2 * z2r;
        let num_i = self.b1 * z1i + self.b2 * z2i;
        let den_r = 1.0 + self.a1 * z1r + self.a2 * z2r;
        let den_i = self.a1 * z1i + self.a2 * z2i;
        let mag2 =
            (num_r * num_r + num_i * num_i) / (den_r * den_r + den_i * den_i);
        10.0 * mag2.max(1e-12).log10()
    }

    /// RBJ Audio EQ Cookbook coefficient derivation.
    pub fn set_coeffs(&mut self, kind: FilterKind, sample_rate: f32, freq: f32, gain_db: f32, q: f32) {
        let freq = freq.clamp(10.0, sample_rate * 0.49);
        let q = q.max(0.05);
        let w0 = 2.0 * PI * freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * q);
        let a = 10f32.powf(gain_db / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match kind {
            FilterKind::Peak => {
                let b0 = 1.0 + alpha * a;
                let b1 = -2.0 * cos_w0;
                let b2 = 1.0 - alpha * a;
                let a0 = 1.0 + alpha / a;
                let a1 = -2.0 * cos_w0;
                let a2 = 1.0 - alpha / a;
                (b0, b1, b2, a0, a1, a2)
            }
            FilterKind::LowShelf => {
                let sq = 2.0 * a.sqrt() * alpha;
                let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + sq);
                let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
                let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - sq);
                let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + sq;
                let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
                let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - sq;
                (b0, b1, b2, a0, a1, a2)
            }
            FilterKind::HighShelf => {
                let sq = 2.0 * a.sqrt() * alpha;
                let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + sq);
                let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
                let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - sq);
                let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + sq;
                let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
                let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - sq;
                (b0, b1, b2, a0, a1, a2)
            }
            FilterKind::LowPass => {
                let b1 = 1.0 - cos_w0;
                let b0 = b1 / 2.0;
                let b2 = b0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cos_w0;
                let a2 = 1.0 - alpha;
                (b0, b1, b2, a0, a1, a2)
            }
            FilterKind::HighPass => {
                let b1 = -(1.0 + cos_w0);
                let b0 = -b1 / 2.0;
                let b2 = b0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cos_w0;
                let a2 = 1.0 - alpha;
                (b0, b1, b2, a0, a1, a2)
            }
        };

        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }
}