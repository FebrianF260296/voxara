use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use std::path::Path;

pub const BAND_CENTERS_HZ: [f32; 8] = [120.0, 250.0, 500.0, 1000.0, 1800.0, 2800.0, 4200.0, 6500.0];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DspConfig {
    /// 0.0..=1.0: Attenuates primary (215-375 Hz) & secondary (1000 Hz) hypernasal resonances.
    pub denasal_strength: f32,
    /// 210.0..=380.0: Primary nasal murmur center frequency in Hz.
    pub nasal_freq_hz: f32,
    /// 0.0..=1.0: Restores oral vowel formants F1 (650 Hz) & F2 (1900 Hz).
    pub oral_formant_boost: f32,
    /// 0.0..=1.0: Dynamic transient enhancer for plosives & fricatives (P, B, T, D, K, G, S, F, C).
    pub consonant_boost: f32,
    /// 0.0..=1.0: Dynamic 5.8 kHz de-esser preventing piercing sibilance when consonant boost is high.
    pub deesser_strength: f32,
    /// 0.0..=1.0: Smooth downward expander attenuating nasal breath hiss during pauses.
    pub air_leak_gate: f32,
    /// 0.0..=1.0: Syllabic breath-loss AGC keeping trailing words in long sentences steady.
    pub breath_compensator: f32,
    /// -6.0..=12.0: Output makeup gain in dB.
    pub output_gain_db: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            denasal_strength: 0.80,
            nasal_freq_hz: 280.0,
            oral_formant_boost: 0.75,
            consonant_boost: 0.80,
            deesser_strength: 0.60,
            air_leak_gate: 0.40,
            breath_compensator: 0.75,
            output_gain_db: 2.0,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct AudioMetrics {
    pub rms_before_db: f32,
    pub rms_after_db: f32,
    /// Spectral balance shift at nasal frequency normalized by broadband RMS delta.
    pub nasal_band_delta_db: f32,
    /// Spectral balance shift at consonant presence band normalized by broadband RMS delta.
    pub consonant_band_delta_db: f32,
    pub estimated_f0_hz: f32,
}

/// Second-order Direct Form II Transposed Biquad IIR filter.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn normalize(a0: f32, a1: f32, a2: f32, b0: f32, b1: f32, b2: f32) -> Self {
        let inv_a0 = 1.0 / a0.max(1e-12);
        Self {
            b0: b0 * inv_a0,
            b1: b1 * inv_a0,
            b2: b2 * inv_a0,
            a1: a1 * inv_a0,
            a2: a2 * inv_a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn low_pass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * freq.clamp(20.0, sample_rate * 0.48) / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q.max(0.1));
        Self::normalize(
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
            (1.0 - cos_w0) * 0.5,
            1.0 - cos_w0,
            (1.0 - cos_w0) * 0.5,
        )
    }

    pub fn high_pass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * freq.clamp(20.0, sample_rate * 0.45) / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q.max(0.1));
        Self::normalize(
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
            (1.0 + cos_w0) * 0.5,
            -(1.0 + cos_w0),
            (1.0 + cos_w0) * 0.5,
        )
    }

    pub fn band_pass(sample_rate: f32, freq: f32, q: f32) -> Self {
        let w0 = 2.0 * PI * freq.clamp(20.0, sample_rate * 0.45) / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q.max(0.1));
        Self::normalize(1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha, alpha, 0.0, -alpha)
    }

    pub fn peaking_eq(sample_rate: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.05 {
            return Self::identity();
        }
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq.clamp(20.0, sample_rate * 0.45) / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * q.max(0.1));
        Self::normalize(
            1.0 + alpha / a,
            -2.0 * cos_w0,
            1.0 - alpha / a,
            1.0 + alpha * a,
            -2.0 * cos_w0,
            1.0 - alpha * a,
        )
    }

    pub fn high_shelf(sample_rate: f32, freq: f32, slope: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.05 {
            return Self::identity();
        }
        let a = 10.0_f32.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq.clamp(20.0, sample_rate * 0.45) / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / 2.0
            * ((a + 1.0 / a) * (1.0 / slope.max(0.2) - 1.0) + 2.0)
                .max(0.01)
                .sqrt();
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Self::normalize(
            (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
            (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
            a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
            a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
        )
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        if !y.is_finite() {
            self.reset();
            0.0
        } else {
            y
        }
    }

    /// Update coefficients while preserving delay state for glitch-free live slider moves.
    pub fn copy_coeffs_from(&mut self, other: Biquad) {
        self.b0 = other.b0;
        self.b1 = other.b1;
        self.b2 = other.b2;
        self.a1 = other.a1;
        self.a2 = other.a2;
    }
}

/// Real-time, zero-allocation 6-stage Articulation Megaphone DSP with sample-rate-invariant smoothing.
pub struct RealtimeDspChain {
    sample_rate: f32,
    cfg: DspConfig,
    makeup_linear: f32,
    hp_subsonic: Biquad,
    notch_nasal_primary: Biquad,
    notch_nasal_secondary: Biquad,
    nasal_monitor_bp: Biquad,
    oral_monitor_bp: Biquad,
    peak_oral_f1: Biquad,
    peak_oral_f2: Biquad,
    presence_shelf: Biquad,
    tongue_band_extract: Biquad,
    burst_band_extract: Biquad,
    deesser_bp: Biquad,
    // Envelopes
    gate_env: f32,
    gate_gain: f32,
    breath_env: f32,
    breath_gain: f32,
    nasal_env: f32,
    oral_env: f32,
    fast_cons_env: f32,
    slow_cons_env: f32,
    deess_env: f32,
    comp_env: f32,
    // Sample-rate-invariant time constants
    gate_attack: f32,
    gate_release: f32,
    gate_smooth: f32,
    breath_attack: f32,
    breath_release: f32,
    breath_smooth: f32,
    ratio_coeff: f32,
    cons_fast_coeff: f32,
    cons_slow_coeff: f32,
    deess_coeff: f32,
    comp_attack: f32,
    comp_release: f32,
}

impl RealtimeDspChain {
    pub fn new(sample_rate: u32, cfg: DspConfig) -> Self {
        let sr = (sample_rate.max(8000)) as f32;
        let mut chain = Self {
            sample_rate: sr,
            cfg,
            makeup_linear: 1.0,
            hp_subsonic: Biquad::high_pass(sr, 85.0, 0.707),
            notch_nasal_primary: Biquad::identity(),
            notch_nasal_secondary: Biquad::identity(),
            nasal_monitor_bp: Biquad::band_pass(sr, cfg.nasal_freq_hz.clamp(210.0, 400.0), 2.2),
            oral_monitor_bp: Biquad::band_pass(sr, 1400.0, 0.8),
            peak_oral_f1: Biquad::identity(),
            peak_oral_f2: Biquad::identity(),
            presence_shelf: Biquad::identity(),
            tongue_band_extract: Biquad::band_pass(sr, 2400.0, 0.85),
            burst_band_extract: Biquad::high_pass(sr, 3300.0, 0.707),
            deesser_bp: Biquad::band_pass(sr, (5800.0_f32).min(sr * 0.42), 1.6),
            gate_env: 0.0,
            gate_gain: 1.0,
            breath_env: 0.05,
            breath_gain: 1.0,
            nasal_env: 0.0,
            oral_env: 0.0,
            fast_cons_env: 0.0,
            slow_cons_env: 1e-4,
            deess_env: 0.0,
            comp_env: 0.0,
            gate_attack: (-1.0 / (0.002 * sr)).exp(),
            gate_release: (-1.0 / (0.095 * sr)).exp(),
            gate_smooth: (-1.0 / (0.008 * sr)).exp(),
            breath_attack: (-1.0 / (0.015 * sr)).exp(),
            breath_release: (-1.0 / (0.220 * sr)).exp(),
            breath_smooth: (-1.0 / (0.045 * sr)).exp(),
            ratio_coeff: (-1.0 / (0.030 * sr)).exp(),
            cons_fast_coeff: (-1.0 / (0.0012 * sr)).exp(),
            cons_slow_coeff: (-1.0 / (0.045 * sr)).exp(),
            deess_coeff: (-1.0 / (0.003 * sr)).exp(),
            comp_attack: (-1.0 / (0.006 * sr)).exp(),
            comp_release: (-1.0 / (0.085 * sr)).exp(),
        };
        chain.recompute_filters();
        chain
    }

    pub fn update_config(&mut self, cfg: DspConfig) {
        if self.cfg != cfg {
            self.cfg = cfg;
            self.recompute_filters();
        }
    }

    fn recompute_filters(&mut self) {
        let sr = self.sample_rate;
        let denasal = self.cfg.denasal_strength.clamp(0.0, 1.0);
        let nasal_hz = self.cfg.nasal_freq_hz.clamp(210.0, 380.0);
        let oral = self.cfg.oral_formant_boost.clamp(0.0, 1.0);
        let cons = self.cfg.consonant_boost.clamp(0.0, 1.0);

        self.makeup_linear = 10.0_f32.powf(self.cfg.output_gain_db.clamp(-12.0, 18.0) / 20.0);
        self.nasal_monitor_bp
            .copy_coeffs_from(Biquad::band_pass(sr, nasal_hz, 2.2));
        self.notch_nasal_primary
            .copy_coeffs_from(Biquad::peaking_eq(sr, nasal_hz, 2.1, -12.0 * denasal));
        self.notch_nasal_secondary
            .copy_coeffs_from(Biquad::peaking_eq(sr, 1000.0, 2.4, -7.0 * denasal));
        self.peak_oral_f1
            .copy_coeffs_from(Biquad::peaking_eq(sr, 650.0, 1.4, 5.0 * oral));
        self.peak_oral_f2
            .copy_coeffs_from(Biquad::peaking_eq(sr, 1900.0, 1.3, 5.8 * oral));
        self.presence_shelf
            .copy_coeffs_from(Biquad::high_shelf(sr, 3800.0, 0.85, 4.2 * cons));
    }

    #[inline]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        // 1. Subsonic thump removal
        let x = self.hp_subsonic.process(input);

        // 2. Sample-rate-invariant Air-Leak Expander
        let abs_x = x.abs();
        if abs_x > self.gate_env {
            self.gate_env = self.gate_attack * self.gate_env + (1.0 - self.gate_attack) * abs_x;
        } else {
            self.gate_env = self.gate_release * self.gate_env + (1.0 - self.gate_release) * abs_x;
        }
        let thresh = 0.001 + self.cfg.air_leak_gate.clamp(0.0, 1.0) * 0.014;
        let target_gate = if self.gate_env >= thresh {
            1.0
        } else {
            let ratio = (self.gate_env / thresh.max(1e-5)).clamp(0.0, 1.0);
            0.22 + 0.78 * ratio * ratio
        };
        self.gate_gain = self.gate_smooth * self.gate_gain + (1.0 - self.gate_smooth) * target_gate;
        let gated = x * self.gate_gain;

        // 3. Syllabic Breath-Loss Compensator (50 ms smooth gain for long sentences)
        if abs_x > self.breath_env {
            self.breath_env =
                self.breath_attack * self.breath_env + (1.0 - self.breath_attack) * abs_x;
        } else {
            self.breath_env =
                self.breath_release * self.breath_env + (1.0 - self.breath_release) * abs_x;
        }
        let breath_amt = self.cfg.breath_compensator.clamp(0.0, 1.0);
        let speech_floor = (thresh * 1.2).max(0.006);
        let target_breath_gain = if breath_amt > 0.01 && self.breath_env > speech_floor {
            let desired_level = 0.16_f32;
            let max_boost = 1.0 + 2.4 * breath_amt;
            (desired_level / self.breath_env.max(0.01)).clamp(0.75, max_boost)
        } else {
            1.0
        };
        self.breath_gain =
            self.breath_smooth * self.breath_gain + (1.0 - self.breath_smooth) * target_breath_gain;
        let leveled = gated * self.breath_gain;

        // 4. Adaptive Nasal-to-Oral Ratio Suppressor + Oral Formant EQ
        let nasal_mon = self.nasal_monitor_bp.process(leveled).abs();
        let oral_mon = self.oral_monitor_bp.process(leveled).abs();
        self.nasal_env = self.ratio_coeff * self.nasal_env + (1.0 - self.ratio_coeff) * nasal_mon;
        self.oral_env = self.ratio_coeff * self.oral_env + (1.0 - self.ratio_coeff) * oral_mon;
        let nasal_ratio = (self.nasal_env / (self.oral_env + 0.004)).clamp(0.0, 2.0);
        let adaptive_depth = (0.75 + 0.30 * nasal_ratio).clamp(0.65, 1.28);

        let notched_1 = self.notch_nasal_primary.process(leveled);
        let s = leveled + (notched_1 - leveled) * adaptive_depth;
        let s = self.notch_nasal_secondary.process(s);
        let s = self.peak_oral_f1.process(s);
        let mut s = self.peak_oral_f2.process(s);

        // 5. Dual-Band Consonant Enhancer + Dynamic De-Esser
        let tongue = self.tongue_band_extract.process(s);
        let hf = self.burst_band_extract.process(s);
        let abs_hf = hf.abs();
        self.fast_cons_env =
            self.cons_fast_coeff * self.fast_cons_env + (1.0 - self.cons_fast_coeff) * abs_hf;
        self.slow_cons_env =
            self.cons_slow_coeff * self.slow_cons_env + (1.0 - self.cons_slow_coeff) * abs_hf;

        let cons_strength = self.cfg.consonant_boost.clamp(0.0, 1.0);
        if cons_strength > 0.01 && self.fast_cons_env > 0.0010 {
            let ratio = (self.fast_cons_env / (self.slow_cons_env + 0.002)).clamp(1.0, 2.5);
            let boost_amount = (cons_strength * (0.45 + 0.85 * (ratio - 1.0))).min(1.75);
            let hf_harmonics = (hf * 1.8).tanh() * 0.22 * cons_strength;
            s += tongue * (cons_strength * 0.30) + hf * boost_amount + hf_harmonics;
        }
        s = self.presence_shelf.process(s);

        // De-esser prevents piercing sibilance
        let deess_amt = self.cfg.deesser_strength.clamp(0.0, 1.0);
        if deess_amt > 0.01 {
            let sib = self.deesser_bp.process(s);
            self.deess_env =
                self.deess_coeff * self.deess_env + (1.0 - self.deess_coeff) * sib.abs();
            let sib_thresh = 0.14;
            if self.deess_env > sib_thresh {
                let excess = ((self.deess_env - sib_thresh) / self.deess_env).clamp(0.0, 0.75);
                s -= sib * (excess * deess_amt);
            }
        }

        // 6. Precomputed makeup gain + soft-knee compressor + limiter
        let mut out = s * self.makeup_linear;
        let abs_out = out.abs();
        if abs_out > self.comp_env {
            self.comp_env = self.comp_attack * self.comp_env + (1.0 - self.comp_attack) * abs_out;
        } else {
            self.comp_env =
                self.comp_release * self.comp_env + (1.0 - self.comp_release) * abs_out;
        }
        let comp_thresh = 0.42;
        if self.comp_env > comp_thresh {
            let over = self.comp_env / comp_thresh;
            out *= over.powf(-0.65);
        }

        let ceiling = 0.944;
        (out / ceiling).tanh() * ceiling
    }

    pub fn process_buffer(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(samples.len());
        for &s in samples {
            out.push(self.process_sample(s));
        }
        out
    }
}

/// Chunk-continuous anti-aliased linear resampler (zero-allocation when writing into reused `out`).
pub struct StatefulResampler {
    src_sr: u32,
    dst_sr: u32,
    aa_filter: Option<Biquad>,
    frac_pos: f64,
    prev_sample: f32,
    has_prev: bool,
}

impl StatefulResampler {
    pub fn new(src_sr: u32, dst_sr: u32) -> Self {
        let src = src_sr.max(8000);
        let dst = dst_sr.max(8000);
        let aa_filter = if dst < src {
            // Butterworth anti-aliasing LPF at 0.43 * dst_sr before decimation
            Some(Biquad::low_pass(src as f32, (dst as f32) * 0.43, 0.707))
        } else {
            None
        };
        Self {
            src_sr: src,
            dst_sr: dst,
            aa_filter,
            frac_pos: 0.0,
            prev_sample: 0.0,
            has_prev: false,
        }
    }

    pub fn process_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if input.is_empty() {
            return;
        }
        if self.src_sr == self.dst_sr {
            out.extend_from_slice(input);
            return;
        }

        let step = self.src_sr as f64 / self.dst_sr as f64;
        for &raw in input {
            let x = match self.aa_filter {
                Some(ref mut f) => f.process(raw),
                None => raw,
            };
            if !self.has_prev {
                self.prev_sample = x;
                self.has_prev = true;
            }
            while self.frac_pos < 1.0 {
                let frac = self.frac_pos as f32;
                out.push(self.prev_sample * (1.0 - frac) + x * frac);
                self.frac_pos += step;
            }
            self.frac_pos -= 1.0;
            self.prev_sample = x;
        }
    }
}

pub fn resample_batch(samples: &[f32], src_sr: u32, dst_sr: u32) -> Vec<f32> {
    if samples.is_empty() || src_sr == dst_sr {
        return samples.to_vec();
    }
    let mut r = StatefulResampler::new(src_sr, dst_sr);
    let mut out = Vec::with_capacity((samples.len() as f64 * dst_sr as f64 / src_sr as f64) as usize + 8);
    r.process_into(samples, &mut out);
    out
}

pub fn process_cleft_palate_audio(
    samples: &[f32],
    sample_rate: u32,
    cfg: &DspConfig,
) -> (Vec<f32>, AudioMetrics) {
    if samples.is_empty() {
        return (Vec::new(), AudioMetrics::default());
    }
    let rms_before = rms_db(samples);
    let nasal_before = band_energy_db(samples, sample_rate, cfg.nasal_freq_hz, 2.0);
    let cons_before = band_energy_db(samples, sample_rate, 3800.0, 0.9);

    let mut chain = RealtimeDspChain::new(sample_rate, *cfg);
    let processed = chain.process_buffer(samples);

    let rms_after = rms_db(&processed);
    let nasal_after = band_energy_db(&processed, sample_rate, cfg.nasal_freq_hz, 2.0);
    let cons_after = band_energy_db(&processed, sample_rate, 3800.0, 0.9);
    let f0 = estimate_f0_hz(&processed, sample_rate);

    // Normalize band deltas relative to broadband RMS shift so metrics isolate spectral clarity
    let rms_shift = rms_after - rms_before;
    let metrics = AudioMetrics {
        rms_before_db: rms_before,
        rms_after_db: rms_after,
        nasal_band_delta_db: (nasal_after - nasal_before) - rms_shift,
        consonant_band_delta_db: (cons_after - cons_before) - rms_shift,
        estimated_f0_hz: f0,
    };
    (processed, metrics)
}

/// Distance in Hz from `freq` to the nearest harmonic (k >= 1) of `f0`.
fn distance_to_nearest_harmonic(freq: f32, f0: f32) -> f32 {
    if f0 <= 0.0 {
        return f32::MAX;
    }
    let rem = freq.rem_euclid(f0);
    // Below f0 the only harmonic is f0 itself; otherwise take the closer neighbour.
    if freq < f0 {
        f0 - freq
    } else {
        rem.min(f0 - rem)
    }
}

/// Heuristically auto-tunes `DspConfig` from a speech sample (not a clinical measurement).
///
/// The fundamental F0 is estimated first. The level is then measured at each harmonic
/// `k * F0` (up to about 1.2 kHz) with a band narrower than the harmonic spacing. A harmonic
/// in 215-375 Hz (k >= 2) that stands out from the mean of its two neighbours by at least
/// 1.5 dB is taken as the nasal resonance; the more it stands out, the stronger the cut.
/// If no harmonic stands out, a moderate default strength is used and the notch frequency is
/// placed away from F0 and its harmonics so the cut does not thin the voice itself.
/// Consonant boost follows the 1300 Hz vs 3800 Hz level deficit.
pub fn auto_calibrate_dsp_from_sample(samples: &[f32], sample_rate: u32) -> (DspConfig, f32, [f32; 8]) {
    const MIN_NASAL_HZ: f32 = 215.0;
    const MAX_NASAL_HZ: f32 = 375.0;
    const MAX_HARMONIC_HZ: f32 = 1200.0;
    const MIN_EXCESS_DB: f32 = 1.5;
    const DEFAULT_NASAL_HZ: f32 = 280.0;
    const DEFAULT_STRENGTH: f32 = 0.55;

    let f0 = estimate_f0_hz(samples, sample_rate);

    // Levels (dB) at harmonics k*f0 for k = 1..=n; levels[k - 1] belongs to harmonic k.
    let limit_hz = MAX_HARMONIC_HZ.min(sample_rate as f32 * 0.45);
    let n_harm = if f0 > 0.0 { (limit_hz / f0).floor() as usize } else { 0 };
    let levels: Vec<f32> = (1..=n_harm)
        .map(|k| {
            let h = k as f32 * f0;
            band_energy_db(samples, sample_rate, h, h / (0.6 * f0))
        })
        .collect();

    // Look for a harmonic in the nasal range that exceeds the mean of its neighbours.
    let mut best: Option<(f32, f32)> = None; // (harmonic frequency, excess dB)
    for k in 2..n_harm {
        let h = k as f32 * f0;
        if !(MIN_NASAL_HZ..=MAX_NASAL_HZ).contains(&h) {
            continue;
        }
        let excess = levels[k - 1] - 0.5 * (levels[k - 2] + levels[k]);
        if best.map_or(true, |(_, e)| excess > e) {
            best = Some((h, excess));
        }
    }

    let (nasal_freq_hz, denasal_strength) = match best {
        Some((h, excess)) if excess >= MIN_EXCESS_DB => (
            h.clamp(MIN_NASAL_HZ, MAX_NASAL_HZ),
            (DEFAULT_STRENGTH + excess * 0.05).clamp(0.45, 0.95),
        ),
        _ => {
            // No distinct resonance: keep the default unless it sits on F0 or a harmonic.
            let freq = if distance_to_nearest_harmonic(DEFAULT_NASAL_HZ, f0) > 35.0 {
                DEFAULT_NASAL_HZ
            } else {
                let mut pick = DEFAULT_NASAL_HZ;
                let mut pick_dist = -1.0_f32;
                let mut cand = MIN_NASAL_HZ;
                while cand <= MAX_NASAL_HZ {
                    let d = distance_to_nearest_harmonic(cand, f0);
                    if d > pick_dist {
                        pick_dist = d;
                        pick = cand;
                    }
                    cand += 5.0;
                }
                pick
            };
            (freq, DEFAULT_STRENGTH)
        }
    };

    let oral_energy = band_energy_db(samples, sample_rate, 1300.0, 0.9);
    let cons_energy = band_energy_db(samples, sample_rate, 3800.0, 0.9);
    let cons_deficit = (oral_energy - cons_energy).clamp(-2.0, 20.0);
    let consonant_boost = (0.55 + cons_deficit * 0.025).clamp(0.45, 0.92);

    let bands = compute_8band_spectrum(samples, sample_rate);

    let cfg = DspConfig {
        denasal_strength,
        nasal_freq_hz,
        oral_formant_boost: 0.78,
        consonant_boost,
        deesser_strength: 0.60,
        air_leak_gate: 0.40,
        breath_compensator: 0.78,
        output_gain_db: 2.0,
    };
    (cfg, f0, bands)
}

pub fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -96.0;
    }
    let sum_sq: f32 = samples.iter().map(|&x| x * x).sum();
    let rms = (sum_sq / samples.len() as f32).sqrt().max(1e-6);
    20.0 * rms.log10()
}

pub fn band_energy_db(samples: &[f32], sample_rate: u32, center_hz: f32, q: f32) -> f32 {
    if samples.is_empty() {
        return -96.0;
    }
    let mut bp = Biquad::band_pass(sample_rate as f32, center_hz, q);
    let mut sum_sq = 0.0_f32;
    for &x in samples {
        let y = bp.process(x);
        sum_sq += y * y;
    }
    let rms = (sum_sq / samples.len() as f32).sqrt().max(1e-6);
    20.0 * rms.log10()
}

/// Estimates median fundamental vocal pitch F0 (75..=380 Hz) using normalized autocorrelation.
pub fn estimate_f0_hz(samples: &[f32], sample_rate: u32) -> f32 {
    if samples.len() < (sample_rate as usize / 20) {
        return 140.0;
    }
    let sr = sample_rate as f32;
    let frame_len = (sr * 0.040) as usize;
    let hop = frame_len / 2;
    let min_lag = (sr / 380.0).max(8.0) as usize;
    let max_lag = (sr / 75.0).min((frame_len / 2) as f32) as usize;
    if max_lag <= min_lag + 2 {
        return 140.0;
    }

    let mut pitches = Vec::new();
    let mut pos = 0usize;
    while pos + frame_len <= samples.len() {
        let frame = &samples[pos..pos + frame_len];
        let energy: f32 = frame.iter().map(|&x| x * x).sum();
        if energy / frame_len as f32 > 0.0002 {
            // Normalized autocorrelation for every candidate lag.
            let mut corrs = vec![0.0_f32; max_lag + 2];
            let mut best_corr = 0.0_f32;
            for lag in min_lag..=max_lag {
                let mut num = 0.0_f32;
                let mut denom_a = 0.0_f32;
                let mut denom_b = 0.0_f32;
                let n = frame_len - lag;
                for i in 0..n {
                    let a = frame[i];
                    let b = frame[i + lag];
                    num += a * b;
                    denom_a += a * a;
                    denom_b += b * b;
                }
                let denom = (denom_a * denom_b).sqrt();
                if denom > 1e-6 {
                    let corr = num / denom;
                    corrs[lag] = corr;
                    if corr > best_corr {
                        best_corr = corr;
                    }
                }
            }
            // Prefer the shortest lag that is a local maximum close to the global best,
            // so that multiples of the period (octave-down errors) are not chosen.
            let mut best_lag = 0usize;
            for lag in min_lag..=max_lag {
                let c = corrs[lag];
                let prev = if lag > min_lag { corrs[lag - 1] } else { f32::MIN };
                if c >= 0.9 * best_corr && c >= prev && c > corrs[lag + 1] {
                    best_lag = lag;
                    break;
                }
            }
            if best_corr > 0.55 && best_lag > 0 {
                pitches.push(sr / best_lag as f32);
            }
        }
        pos += hop;
    }

    if pitches.is_empty() {
        return 140.0;
    }
    pitches.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pitches[pitches.len() / 2]
}

pub fn compute_8band_spectrum(samples: &[f32], sample_rate: u32) -> [f32; 8] {
    let mut raw = [0.0_f32; 8];
    if samples.is_empty() {
        return raw;
    }
    for (idx, &center) in BAND_CENTERS_HZ.iter().enumerate() {
        if center < (sample_rate as f32 * 0.45) {
            raw[idx] = band_energy_db(samples, sample_rate, center, 1.4);
        } else {
            raw[idx] = -60.0;
        }
    }
    let mean = raw.iter().sum::<f32>() / 8.0;
    for val in &mut raw {
        *val = (*val - mean).clamp(-10.0, 10.0);
    }
    raw
}

pub fn save_mono_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;
    for &s in samples {
        let clamped = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        writer.write_sample(clamped).map_err(|e| e.to_string())?;
    }
    writer.finalize().map_err(|e| e.to_string())
}

pub fn load_mono_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let mut reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, _) => {
            reader.samples::<f32>().filter_map(Result::ok).collect()
        }
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .filter_map(Result::ok)
            .map(|s| s as f32 / 32768.0)
            .collect(),
        (hound::SampleFormat::Int, 24) | (hound::SampleFormat::Int, 32) => {
            let max_val = (1_i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(Result::ok)
                .map(|s| s as f32 / max_val)
                .collect()
        }
        (hound::SampleFormat::Int, 8) => reader
            .samples::<i8>()
            .filter_map(Result::ok)
            .map(|s| s as f32 / 128.0)
            .collect(),
        _ => return Err("Format sampel WAV tidak didukung".to_string()),
    };

    if ch == 1 {
        return Ok((interleaved, spec.sample_rate));
    }
    let mut mono = Vec::with_capacity(interleaved.len() / ch);
    for frame in interleaved.chunks_exact(ch) {
        mono.push(frame.iter().sum::<f32>() / ch as f32);
    }
    Ok((mono, spec.sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_wave(freq: f32, sr: u32, secs: f32, amp: f32) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / sr as f32).sin())
            .collect()
    }

    #[test]
    fn test_normalized_denasal_and_consonant_metrics() {
        let sr = 16000;
        let nasal = sine_wave(280.0, sr, 0.5, 0.45);
        let oral = sine_wave(650.0, sr, 0.5, 0.25);
        let mut cons = sine_wave(3800.0, sr, 0.5, 0.04);
        for i in 3200..3600 {
            cons[i] *= 4.0;
        }
        let mixed: Vec<f32> = (0..nasal.len())
            .map(|i| nasal[i] + oral[i] + cons[i])
            .collect();

        let cfg = DspConfig::default();
        let (_out, metrics) = process_cleft_palate_audio(&mixed, sr, &cfg);

        assert!(
            metrics.nasal_band_delta_db < -4.0,
            "Expected normalized nasal attenuation < -4 dB, got {}",
            metrics.nasal_band_delta_db
        );
        assert!(
            metrics.consonant_band_delta_db > 2.0,
            "Expected normalized consonant boost > 2 dB, got {}",
            metrics.consonant_band_delta_db
        );
    }

    #[test]
    fn test_breath_compensator_sustains_long_sentence_tail() {
        let sr = 48000;
        let mut sentence = sine_wave(650.0, sr, 2.0, 0.16);
        for i in (sr as usize)..sentence.len() {
            sentence[i] *= 0.22;
        }
        let cfg = DspConfig::default();
        let (out, _) = process_cleft_palate_audio(&sentence, sr, &cfg);
        let tail_in_rms = rms_db(&sentence[(sr as usize + 12000)..]);
        let tail_out_rms = rms_db(&out[(sr as usize + 12000)..]);
        assert!(
            tail_out_rms > tail_in_rms + 4.5,
            "Expected breath compensator recovery at 48k: in={}, out={}",
            tail_in_rms,
            tail_out_rms
        );
    }

    #[test]
    fn test_stateful_resampler_anti_aliasing_and_continuity() {
        // High frequency 14 kHz tone at 48 kHz should be attenuated when downsampling to 16 kHz (Nyquist=8k)
        let high = sine_wave(14000.0, 48000, 0.2, 0.5);
        let mut resampler = StatefulResampler::new(48000, 16000);
        let mut chunk_out = Vec::new();
        let mut combined = Vec::new();
        for chunk in high.chunks(480) {
            resampler.process_into(chunk, &mut chunk_out);
            combined.extend_from_slice(&chunk_out);
        }
        assert!(rms_db(&combined) < -18.0, "Expected AA filter attenuation, got {}", rms_db(&combined));

        // Continuity test: 440 Hz tone resampled 44.1 -> 48 kHz in chunks has no spike jumps
        let tone = sine_wave(440.0, 44100, 0.2, 0.4);
        let mut r2 = StatefulResampler::new(44100, 48000);
        let mut out2 = Vec::new();
        let mut all2 = Vec::new();
        for chunk in tone.chunks(256) {
            r2.process_into(chunk, &mut out2);
            all2.extend_from_slice(&out2);
        }
        let max_diff = all2.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0_f32, f32::max);
        assert!(max_diff < 0.08, "Unexpected click/discontinuity max_diff={max_diff}");
    }

    /// Sums harmonics k = 1..=20 of `f0` with amplitude 0.3 / k, skipping those at or above
    /// 0.45 * sr. The harmonic nearest to `boost.0` Hz is boosted by `boost.1` dB.
    fn harmonic_voice(f0: f32, sr: u32, secs: f32, boost: Option<(f32, f32)>) -> Vec<f32> {
        let n = (sr as f32 * secs) as usize;
        let max_hz = 0.45 * sr as f32;
        let boosted_k = boost.map(|(hz, _)| ((hz / f0).round() as usize).max(1));
        let mut out = vec![0.0_f32; n];
        for k in 1..=20usize {
            let freq = k as f32 * f0;
            if freq >= max_hz {
                continue;
            }
            let mut amp = 0.3 / k as f32;
            if let (Some(bk), Some((_, db))) = (boosted_k, boost) {
                if bk == k {
                    amp *= 10.0_f32.powf(db / 20.0);
                }
            }
            for (i, v) in out.iter_mut().enumerate() {
                *v += amp * (2.0 * PI * freq * i as f32 / sr as f32).sin();
            }
        }
        out
    }

    #[test]
    fn test_auto_calibrate_finds_boosted_harmonic_in_male_voice() {
        let sr = 16000;
        let sig = harmonic_voice(110.0, sr, 1.0, Some((330.0, 8.0)));
        let (cfg, f0, _) = auto_calibrate_dsp_from_sample(&sig, sr);
        assert!((f0 - 110.0).abs() < 8.0, "f0 estimate {f0}");
        assert!((cfg.nasal_freq_hz - 330.0).abs() <= 15.0, "Detected {} Hz", cfg.nasal_freq_hz);
        assert!(cfg.denasal_strength > 0.8, "strength {}", cfg.denasal_strength);
    }

    #[test]
    fn test_auto_calibrate_avoids_f0_for_female_voice() {
        let sr = 16000;
        let sig = harmonic_voice(230.0, sr, 1.0, None);
        let (cfg, _f0, _) = auto_calibrate_dsp_from_sample(&sig, sr);
        assert!((cfg.nasal_freq_hz - 230.0).abs() >= 35.0, "Detected {} Hz", cfg.nasal_freq_hz);
        assert!(cfg.denasal_strength <= 0.6, "strength {}", cfg.denasal_strength);
    }

    #[test]
    fn test_auto_calibrate_avoids_f0_and_second_harmonic() {
        let sr = 16000;
        let sig = harmonic_voice(140.0, sr, 1.0, None);
        let (cfg, _f0, _) = auto_calibrate_dsp_from_sample(&sig, sr);
        assert!((cfg.nasal_freq_hz - 140.0).abs() >= 35.0, "Detected {} Hz", cfg.nasal_freq_hz);
        assert!((cfg.nasal_freq_hz - 280.0).abs() >= 35.0, "Detected {} Hz", cfg.nasal_freq_hz);
    }

    #[test]
    fn test_dsp_config_deserializes_with_missing_fields() {
        let cfg: DspConfig = serde_json::from_str(r#"{"denasal_strength":0.5,"nasal_freq_hz":300.0}"#)
            .expect("partial config should load");
        assert_eq!(cfg.denasal_strength, 0.5);
        assert_eq!(cfg.nasal_freq_hz, 300.0);
        assert_eq!(cfg.deesser_strength, 0.60);
    }
}
