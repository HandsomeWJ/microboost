//! Speaker echo suppression.
//!
//! Removes what the PC is playing (podcasts, videos, call audio) from the mic
//! signal, using a loopback copy of the speaker mix as the reference. Runs at
//! the mic rate, before the boost gain is applied, and adds two blocks
//! ([`EchoCanceller::latency`], ~11 ms) of latency to the mic path.
//!
//! ```text
//! reference x ─► history ─► partitioned FD adaptive filters (bg adapts, fg = stable copy)
//! mic d       ─► e = d − fg(x) ─► × duck gain ─► out
//! ```
//!
//! * The bulk delay between reference and echo (render latency + room + capture
//!   latency, typically 20–100 ms, up to 1 s) is found by cross-correlating the
//!   *envelopes* of the two signals in 2.5 ms frames. The adaptive filter window
//!   (~85 ms) is then positioned around that delay, so it only has to span the
//!   room's early reflections instead of the whole delay range.
//! * The filter is a partitioned-block frequency-domain NLMS (overlap-save,
//!   per-bin normalisation, rotating gradient constraint). Per-bin normalisation
//!   is what makes it converge on speech, where time-domain NLMS crawls.
//! * Foreground/background filter pair: the background filter always adapts; the
//!   foreground filter is only overwritten when the background one is clearly
//!   better *and* actually cancels something, and the background one is reset
//!   from the foreground when it has diverged. Updates happen once per block,
//!   so the comparison cannot be fooled by sample-to-sample tracking of the
//!   near-end voice. Together this stands in for a double-talk detector.
//! * Residual suppression (adaptive mode): an STFT stage (same block size,
//!   sqrt-Hann, 50 % overlap) applies a per-bin Wiener gain to the canceller
//!   output, floored at `−strength`. The leftover echo in each bin is predicted
//!   from two voice-free quantities, the filter's echo estimate and the aligned
//!   reference, each scaled by a per-bin ratio learned with *minimum statistics*
//!   (running minimum with slow upward drift). The user's voice can only raise
//!   those ratios, so the learning cannot be poisoned by talking, and the
//!   reference-based prediction keeps working when the linear canceller barely
//!   cancels anything (nonlinear speakers, mic AGC/enhancements). Bins where the
//!   user's voice dominates keep a gain near 1; bins where leftover echo
//!   dominates fall to the floor. The prediction is scaled up by a factor set
//!   by the strength slider (16× at 40 dB, doubling every 10 dB): leftover echo
//!   scatters several dB around a minimum-statistics prediction, and only a
//!   prediction above that scatter sends echo-only bins to the floor. The
//!   price is mild thinning of the user's voice in bins the echo also occupies
//!   while media plays; their level is otherwise untouched. A speech-band
//!   near-end detector (error power vs. predicted residual and noise floor,
//!   300 Hz up) gates the ERLE statistics and drives the status line.
//!   Suppression never engages unless echo is detected in the mic, so headphone
//!   users are untouched. Simple mode: broadband attenuation by `strength`
//!   whenever the speakers are playing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Reference RMS above which the speakers count as "playing" (≈ −50 dBFS, the
/// same floor the auto-calibration uses to tell speech from silence).
pub const REF_ACTIVE_RMS: f32 = 0.003;

const WINDOW_SEC: f32 = 0.085;
const MAX_DELAY_SEC: f32 = 1.0;
const ENV_FRAMES_PER_SEC: u32 = 400; // 2.5 ms envelope frames
const ENV_HISTORY_SEC: f32 = 3.0;
/// Negative lags (mic leading the reference) searched for diagnosis only; the
/// causal filter cannot use them.
const NEG_LAG_SEC: f32 = 0.1;
const EST_INTERVAL_SEC: f32 = 1.0;
const MU: f32 = 0.5;
/// Regularisation floor for per-bin normalisation, as a fraction of the mean
/// bin power. Stops near-empty bins from adapting on noise and drifting.
const DELTA_REL: f32 = 0.05;
/// Envelope correlation needed to move the filter window.
const CORR_RELOCATE: f32 = 0.5;
/// Envelope correlation that by itself counts as "echo present".
const CORR_DETECT: f32 = 0.6;
/// Foreground ERLE that by itself counts as "echo present".
const ERLE_DETECT_DB: f32 = 5.0;
/// Smoothing of the block power statistics used for filter selection and ERLE
/// (per block, ≈ 40 ms time constant at 48 kHz). Single blocks of coincidental
/// fit between two unrelated voices must not flip the decisions.
const STAT_ALPHA: f32 = 0.125;
/// Below this foreground ERLE the delay estimate may be revised.
const ERLE_LOCK_DB: f32 = 6.0;
const ERLE_MAX_DB: f32 = 40.0;
/// Per-block decay of the held ERLE estimate (≈1 dB/s at 48 kHz). Slow on
/// purpose: a drop during double-talk must not duck the speaker mid-sentence.
const ERLE_DECAY_DB: f32 = 0.005;
/// How long the speakers count as "playing" after the last audible sample.
const REF_HOLD_SEC: f32 = 0.3;
/// Error power this many times above the predicted residual (speech band
/// only, see NEAR_MIN_HZ) means the near end (the user) is talking.
const NEAR_RATIO: f32 = 4.0;
/// The near-end decision ignores bins below this: mics pick up speaker bass
/// through desks and walls with ±5 dB of scatter, while the user's voice
/// stands out clearly in the speech band. 0 % false alarms on a real room.
const NEAR_MIN_HZ: f32 = 300.0;
/// Over-prediction factor of the residual at 40 dB strength; it scales as
/// 2^(strength/10). The leftover echo scatters several dB around a
/// minimum-statistics prediction, so bins carrying echo only reach the floor
/// when the prediction is pushed well above that scatter.
const SUP_FACTOR_AT_40: f32 = 16.0;
/// Asymmetric smoothing of the conservative ERLE (used for residual prediction
/// and ducking depth): it follows drops 10× faster than rises, so it settles
/// near the low end of the block-to-block spread instead of the mean.
const ERLE_LOW_ALPHA_DOWN: f32 = 0.1;
const ERLE_LOW_ALPHA_UP: f32 = 0.01;
/// Upward drift of the error-floor tracker (minimum statistics): ≈2.6 dB/s.
const FLOOR_RISE: f32 = 1.003;
/// Hangover after the last near-end block before ducking may resume.
const NEAR_HOLD_SEC: f32 = 0.3;
/// Residual suppressor: over-subtraction of the predicted residual.
const SUP_OVERSUB: f32 = 2.0;
/// Residual suppressor: upward drift of the minimum-statistics ratios per
/// block (≈0.75 dB/s at 48 kHz). Slow enough that ten seconds of non-stop
/// talking over-predicts the residual by under 8 dB.
const SUP_LEAK_RISE: f32 = 1.00093;
/// Residual suppressor: smoothing of per-bin powers (~10 ms) used for the gain.
const SUP_POW_ALPHA: f32 = 0.5;
/// Residual suppressor: slower smoothing (~35 ms) of the powers whose ratios
/// are tracked by minimum statistics, so the minimum is not just picking the
/// noisiest low frames.
const SUP_SLOW_ALPHA: f32 = 0.15;
/// Clamp for the self-calibrated bias of the residual prediction.
const SUP_BIAS_MIN: f32 = 0.5;
const SUP_BIAS_MAX: f32 = 20.0;
/// Residual suppressor: gain smoothing toward 1 (voice onset) and toward the floor.
const SUP_GAIN_UP: f32 = 0.7;
const SUP_GAIN_DOWN: f32 = 0.3;
/// ERLE decay while the near end talks (≈0.25 dB/s): self-corrects at the
/// next pause, bounded so a stale estimate cannot suppress ducking forever.
const ERLE_DECAY_NEAR_DB: f32 = 0.00125;

/// Settings and live stats shared between the UI thread and the audio callback.
pub struct Shared {
    pub adaptive: AtomicBool,
    pub strength_db: AtomicU32, // f32 bits
    pub ref_active: AtomicBool,
    pub echo_detected: AtomicBool,
    pub near_active: AtomicBool,
    pub erle_db: AtomicU32,  // f32 bits
    pub duck_db: AtomicU32,  // f32 bits
    pub delay_ms: AtomicU32, // f32 bits
    pub corr: AtomicU32,     // f32 bits
    /// Mic capture time minus the reference's capture time at the aligned
    /// position, in ms (written by the app's mic callback, diagnostic).
    pub ref_offset_ms: AtomicU32, // f32 bits
    /// Best envelope-correlation lag, ms (negative = mic leads reference).
    pub lag_ms: AtomicU32, // f32 bits
    /// Fraction of reference samples that were not available (0..1).
    pub ref_missing: AtomicU32, // f32 bits
    /// Filter window start, ms.
    pub window_ms: AtomicU32, // f32 bits
    /// Max-hold ERLE, dB.
    pub erle_max_db: AtomicU32, // f32 bits
}

impl Shared {
    pub fn new(adaptive: bool, strength_db: f32) -> Self {
        Self {
            adaptive: AtomicBool::new(adaptive),
            strength_db: AtomicU32::new(strength_db.to_bits()),
            ref_active: AtomicBool::new(false),
            echo_detected: AtomicBool::new(false),
            near_active: AtomicBool::new(false),
            erle_db: AtomicU32::new(0f32.to_bits()),
            duck_db: AtomicU32::new(0f32.to_bits()),
            delay_ms: AtomicU32::new(0f32.to_bits()),
            corr: AtomicU32::new(0f32.to_bits()),
            ref_offset_ms: AtomicU32::new(0f32.to_bits()),
            lag_ms: AtomicU32::new(0f32.to_bits()),
            ref_missing: AtomicU32::new(0f32.to_bits()),
            window_ms: AtomicU32::new(0f32.to_bits()),
            erle_max_db: AtomicU32::new(0f32.to_bits()),
        }
    }

    pub fn set_f32(a: &AtomicU32, v: f32) {
        a.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn get_f32(a: &AtomicU32) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }
}

/// Linear-interpolation resampler for the reference stream (loopback device
/// rate → mic rate). Linear is enough here: it only limits the achievable
/// cancellation above ~8 kHz, where speech has little energy anyway.
pub struct LinearResampler {
    step: f64,
    t: f64,
    last: f32,
    bypass: bool,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            t: 1.0,
            last: 0.0,
            bypass: in_rate == out_rate,
        }
    }

    /// Feed one input sample; `out` is called once per output sample produced.
    #[inline]
    pub fn push(&mut self, s: f32, mut out: impl FnMut(f32)) {
        if self.bypass {
            out(s);
            return;
        }
        // `t` is the position of the next output sample in input units, with
        // `last` at 0 and `s` at 1.
        while self.t <= 1.0 {
            out(self.last + (s - self.last) * self.t as f32);
            self.t += self.step;
        }
        self.t -= 1.0;
        self.last = s;
    }
}

#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Eight independent accumulators so LLVM can vectorise the reduction.
    let mut acc = [0f32; 8];
    for (ca, cb) in a.chunks_exact(8).zip(b.chunks_exact(8)) {
        for k in 0..8 {
            acc[k] += ca[k] * cb[k];
        }
    }
    let rem_a = a.chunks_exact(8).remainder();
    let rem_b = b.chunks_exact(8).remainder();
    let mut s: f32 = acc.iter().sum();
    for (p, q) in rem_a.iter().zip(rem_b) {
        s += p * q;
    }
    s
}

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Cx {
    re: f32,
    im: f32,
}

impl Cx {
    #[inline]
    fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
    #[inline]
    fn mul(self, o: Cx) -> Cx {
        Cx::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
    #[inline]
    fn conj(self) -> Cx {
        Cx::new(self.re, -self.im)
    }
    #[inline]
    fn add(self, o: Cx) -> Cx {
        Cx::new(self.re + o.re, self.im + o.im)
    }
    #[inline]
    fn sub(self, o: Cx) -> Cx {
        Cx::new(self.re - o.re, self.im - o.im)
    }
    #[inline]
    fn scale(self, s: f32) -> Cx {
        Cx::new(self.re * s, self.im * s)
    }
    #[inline]
    fn norm_sqr(self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

/// Iterative radix-2 complex FFT for one power-of-two size.
struct Fft {
    n: usize,
    rev: Vec<usize>,
    tw: Vec<Cx>,
}

impl Fft {
    fn new(n: usize) -> Self {
        assert!(n.is_power_of_two());
        let bits = n.trailing_zeros();
        let rev = (0..n)
            .map(|i| if bits == 0 { 0 } else { i.reverse_bits() >> (usize::BITS - bits) })
            .collect();
        let tw = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                Cx::new(a.cos() as f32, a.sin() as f32)
            })
            .collect();
        Self { n, rev, tw }
    }

    fn forward(&self, buf: &mut [Cx]) {
        debug_assert_eq!(buf.len(), self.n);
        for i in 0..self.n {
            let j = self.rev[i];
            if j > i {
                buf.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= self.n {
            let half = len / 2;
            let step = self.n / len;
            for start in (0..self.n).step_by(len) {
                for k in 0..half {
                    let w = self.tw[k * step];
                    let u = buf[start + k];
                    let v = buf[start + k + half].mul(w);
                    buf[start + k] = u.add(v);
                    buf[start + k + half] = u.sub(v);
                }
            }
            len *= 2;
        }
    }

    /// Inverse transform including the 1/n scaling.
    fn inverse(&self, buf: &mut [Cx]) {
        for v in buf.iter_mut() {
            *v = v.conj();
        }
        self.forward(buf);
        let s = 1.0 / self.n as f32;
        for v in buf.iter_mut() {
            *v = v.conj().scale(s);
        }
    }
}

/// Windowed overlap-add pass of one real block through fixed per-bin gains:
/// frame = [prev | cur] × win, FFT, × gains (mirrored), IFFT, × win,
/// overlap-add; emits the completed block into `out`. Used by the test-only
/// shadow path; the live path does the same inline with e and y packed.
#[cfg(test)]
fn stft_apply(
    fft: &Fft,
    win: &[f32],
    prev: &[f32],
    cur: &[f32],
    gains: &[f32],
    buf: &mut [Cx],
    ola: &mut [f32],
    out: &mut [f32],
) {
    let b = prev.len();
    let n = 2 * b;
    for i in 0..b {
        buf[i] = Cx::new(win[i] * prev[i], 0.0);
        buf[b + i] = Cx::new(win[b + i] * cur[i], 0.0);
    }
    fft.forward(buf);
    for k in 0..=b {
        buf[k] = buf[k].scale(gains[k]);
    }
    for k in b + 1..n {
        buf[k] = buf[n - k].conj();
    }
    fft.inverse(buf);
    for i in 0..n {
        ola[i] += buf[i].re * win[i];
    }
    for i in 0..b {
        out[i] = ola[i];
        ola[i] = ola[b + i];
        ola[b + i] = 0.0;
    }
}

/// Test-only shadow path: the echo and voice components of the mic signal are
/// carried through the canceller's echo estimate and the suppressor's gains
/// separately, so tests can measure leakage and voice distortion exactly.
#[cfg(test)]
struct Split {
    d_echo: Vec<f32>,
    d_near: Vec<f32>,
    e_echo_prev: Vec<f32>,
    e_echo_cur: Vec<f32>,
    near_prev: Vec<f32>,
    near_cur: Vec<f32>,
    ola_echo: Vec<f32>,
    ola_near: Vec<f32>,
    out_echo: Vec<f32>,
    out_near: Vec<f32>,
}

pub struct EchoCanceller {
    sr: u32,
    adaptive: bool,
    strength_db: f32,

    // Reference history, double-written so any window is a contiguous slice.
    hist: usize,
    x: Vec<f32>,
    pos: usize,
    delay: usize,
    max_delay: usize,

    // Block I/O (overlap-save with block `b`, FFT size `n` = 2b, `p` partitions).
    b: usize,
    n: usize,
    p: usize,
    in_d: Vec<f32>,
    out_e: Vec<f32>,
    in_count: usize,
    fft: Fft,
    xf: Vec<Cx>, // p partitions × n bins, ring: slot `xf_head` is the newest
    xf_head: usize,
    w_fg: Vec<Cx>,
    w_bg: Vec<Cx>,
    s_pow: Vec<f32>,
    buf_a: Vec<Cx>,
    buf_b: Vec<Cx>,
    e_bg_blk: Vec<f32>,
    e_cur: Vec<f32>,
    y_cur: Vec<f32>,

    // Residual suppressor (STFT with hop b, frame n, sqrt-Hann in and out).
    win: Vec<f32>,
    e_prev: Vec<f32>,
    y_prev: Vec<f32>,
    sup_se: Vec<f32>,
    sup_sy: Vec<f32>,
    sup_sx: Vec<f32>,
    sup_se_slow: Vec<f32>,
    sup_sy_slow: Vec<f32>,
    sup_sx_slow: Vec<f32>,
    sup_leak_y: Vec<f32>,
    sup_leak_x: Vec<f32>,
    sup_gain: Vec<f32>,
    sup_gain_sm: Vec<f32>,
    spec_e: Vec<Cx>,
    ola: Vec<f32>,
    sup_floor_db: f32,
    #[cfg(test)]
    split: Option<Split>,

    sm_d: f32,
    sm_efg: f32,
    sm_ebg: f32,
    sup_r: Vec<f32>,
    sup_bias: f32,
    sup_floor: f32,
    tune_factor: f32,
    tune_use_bias: bool,
    tune_mode_switch: bool,
    tune_near_ratio: f32,
    tune_near_min_bin: usize,
    tune_near_div: f32,
    near_hold: usize,
    near_hold_blocks: usize,
    near_present: bool,

    // Envelope-based delay estimator.
    env_len: usize,
    env_acc_x: f32,
    env_acc_d: f32,
    env_cnt: usize,
    env_x: Vec<f32>,
    env_d: Vec<f32>,
    env_n: usize,
    env_pos: usize,
    env_filled: usize,
    lin_x: Vec<f32>,
    lin_d: Vec<f32>,
    max_lag: usize,
    neg_lag: usize,
    best_lag_frames: i64,
    prev_lag_frames: Option<i64>,
    frames_since_est: usize,
    est_every: usize,

    // Reference activity and ducking state.
    since_ref_active: usize,
    ref_hold: usize,
    corr_peak: f32,
    erle_db: f32,     // max-hold with slow decay: detection and window lock
    erle_low_db: f32, // low-biased average over far-end-only blocks: residual prediction, ducking depth
    echo_detected: bool,
    duck_db: f32,
    duck_target: f32,
    duck_gain: f32,
    duck_attack: f32,
    duck_release: f32,
}

impl EchoCanceller {
    pub fn new(sr: u32) -> Self {
        let b = ((sr as f32 * 0.0053) as usize).next_power_of_two().clamp(64, 512);
        let n = 2 * b;
        let p = ((WINDOW_SEC * sr as f32) as usize).div_ceil(b).max(2);
        let taps = p * b;
        let env_len = (sr / ENV_FRAMES_PER_SEC).max(1) as usize;
        let max_delay = (sr as f32 * MAX_DELAY_SEC) as usize + env_len;
        let hist = max_delay + taps + n + 1;
        let env_n = (ENV_HISTORY_SEC * ENV_FRAMES_PER_SEC as f32) as usize;
        let max_lag = (MAX_DELAY_SEC * ENV_FRAMES_PER_SEC as f32) as usize;
        let neg_lag = (NEG_LAG_SEC * ENV_FRAMES_PER_SEC as f32) as usize;
        let est_every = (EST_INTERVAL_SEC * ENV_FRAMES_PER_SEC as f32) as usize;
        let srf = sr as f32;
        Self {
            sr,
            adaptive: true,
            strength_db: 40.0,
            hist,
            x: vec![0.0; hist * 2],
            pos: 0,
            delay: 0,
            max_delay,
            b,
            n,
            p,
            in_d: vec![0.0; b],
            out_e: vec![0.0; b],
            in_count: 0,
            fft: Fft::new(n),
            xf: vec![Cx::default(); p * n],
            xf_head: 0,
            w_fg: vec![Cx::default(); p * n],
            w_bg: vec![Cx::default(); p * n],
            s_pow: vec![0.0; n],
            buf_a: vec![Cx::default(); n],
            buf_b: vec![Cx::default(); n],
            e_bg_blk: vec![0.0; b],
            e_cur: vec![0.0; b],
            y_cur: vec![0.0; b],
            win: (0..n)
                .map(|i| (0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).sqrt())
                .collect(),
            e_prev: vec![0.0; b],
            y_prev: vec![0.0; b],
            sup_se: vec![0.0; b + 1],
            sup_sy: vec![0.0; b + 1],
            sup_sx: vec![0.0; b + 1],
            sup_se_slow: vec![0.0; b + 1],
            sup_sy_slow: vec![0.0; b + 1],
            sup_sx_slow: vec![0.0; b + 1],
            sup_leak_y: vec![1.0; b + 1],
            sup_leak_x: vec![1.0; b + 1],
            sup_gain: vec![1.0; b + 1],
            sup_gain_sm: vec![1.0; b + 1],
            spec_e: vec![Cx::default(); b + 1],
            ola: vec![0.0; n],
            sup_floor_db: 0.0,
            #[cfg(test)]
            split: None,
            sm_d: 0.0,
            sm_efg: 0.0,
            sm_ebg: 0.0,
            sup_r: vec![0.0; b + 1],
            sup_bias: 1.0,
            sup_floor: 1.0,
            tune_factor: SUP_FACTOR_AT_40,
            tune_use_bias: false,
            tune_mode_switch: false,
            tune_near_ratio: NEAR_RATIO,
            tune_near_min_bin: ((NEAR_MIN_HZ * n as f32 / sr as f32) as usize).min(b),
            tune_near_div: 1.0,
            near_hold: 0,
            near_hold_blocks: ((NEAR_HOLD_SEC * srf) as usize / b).max(1),
            near_present: false,
            env_len,
            env_acc_x: 0.0,
            env_acc_d: 0.0,
            env_cnt: 0,
            env_x: vec![0.0; env_n],
            env_d: vec![0.0; env_n],
            env_n,
            env_pos: 0,
            env_filled: 0,
            lin_x: vec![0.0; env_n],
            lin_d: vec![0.0; env_n],
            max_lag,
            neg_lag,
            best_lag_frames: 0,
            prev_lag_frames: None,
            frames_since_est: 0,
            est_every,
            since_ref_active: usize::MAX / 2,
            ref_hold: (REF_HOLD_SEC * srf) as usize,
            corr_peak: 0.0,
            erle_db: 0.0,
            erle_low_db: 0.0,
            echo_detected: false,
            duck_db: 0.0,
            duck_target: 1.0,
            duck_gain: 1.0,
            duck_attack: 1.0 - (-1.0 / (0.03 * srf)).exp(),
            duck_release: 1.0 - (-1.0 / (0.01 * srf)).exp(),
        }
    }

    pub fn set_params(&mut self, adaptive: bool, strength_db: f32) {
        self.adaptive = adaptive;
        self.strength_db = strength_db;
        self.tune_factor = SUP_FACTOR_AT_40 * 2f32.powf((strength_db - 40.0) / 10.0);
    }

    /// Suppressor tuning: over-prediction factor of the residual, whether the
    /// self-calibrated bias scales it, and whether the factor drops to 1 while
    /// the near end is detected.
    pub fn set_tuning(&mut self, factor: f32, use_bias: bool, mode_switch: bool) {
        self.tune_factor = factor;
        self.tune_use_bias = use_bias;
        self.tune_mode_switch = mode_switch;
    }

    /// While the near end is detected, the over-prediction factor is divided by this.
    pub fn set_near_divisor(&mut self, div: f32) {
        self.tune_near_div = div.max(1.0);
    }

    /// Near-end detector tuning: ratio threshold and the lowest frequency (Hz)
    /// taken into account (the user's voice is told apart from leftover echo
    /// best in the speech band, above the bass that mics pick up from desks).
    pub fn set_near_tuning(&mut self, ratio: f32, min_hz: f32) {
        self.tune_near_ratio = ratio;
        self.tune_near_min_bin = ((min_hz * self.n as f32 / self.sr as f32) as usize).min(self.b);
    }

    /// Filter length in samples.
    pub fn taps(&self) -> usize {
        self.p * self.b
    }

    /// Samples of delay this stage adds to the mic path: one block for the
    /// canceller plus one for the suppressor's overlap-add (adaptive mode).
    pub fn latency(&self) -> usize {
        if self.adaptive {
            2 * self.b
        } else {
            self.b
        }
    }

    /// Start (in samples before "now") of the filter window.
    pub fn window_start(&self) -> usize {
        self.delay
    }

    /// Best recent cancellation (max-hold, slow decay).
    pub fn erle_db(&self) -> f32 {
        self.erle_db
    }

    /// Conservative (low-biased) cancellation while only the speakers are heard.
    pub fn erle_low_db(&self) -> f32 {
        self.erle_low_db
    }

    pub fn echo_detected(&self) -> bool {
        self.echo_detected
    }

    pub fn ref_active(&self) -> bool {
        self.since_ref_active < self.ref_hold
    }

    pub fn duck_gain(&self) -> f32 {
        self.duck_gain
    }

    /// True while the mic holds more than the predicted residual echo, i.e.
    /// the user is talking (with hangover).
    pub fn near_end_active(&self) -> bool {
        self.near_present
    }

    /// Test-only: like [`process_parts`](Self::process_parts) with the mic
    /// signal given as echo + voice components. Returns
    /// `(output, output's echo part, output's voice part)`.
    #[cfg(test)]
    fn process_split(&mut self, d_echo: f32, d_near: f32, x: f32) -> (f32, f32, f32) {
        let b = self.b;
        if self.split.is_none() {
            self.split = Some(Split {
                d_echo: vec![0.0; b],
                d_near: vec![0.0; b],
                e_echo_prev: vec![0.0; b],
                e_echo_cur: vec![0.0; b],
                near_prev: vec![0.0; b],
                near_cur: vec![0.0; b],
                ola_echo: vec![0.0; 2 * b],
                ola_near: vec![0.0; 2 * b],
                out_echo: vec![0.0; b],
                out_near: vec![0.0; b],
            });
        }
        let idx = self.in_count;
        let (oe, on) = {
            let sp = self.split.as_mut().unwrap();
            sp.d_echo[idx] = d_echo;
            sp.d_near[idx] = d_near;
            (sp.out_echo[idx], sp.out_near[idx])
        };
        let (e, g) = self.process_parts(d_echo + d_near, x);
        (e * g, oe * g, on * g)
    }

    #[cfg(test)]
    fn debug_near(&self) -> String {
        format!(
            "bias {:.2}, floor {:.2e}, low {:.1}, max {:.1}, near {}",
            self.sup_bias, self.sup_floor, self.erle_low_db, self.erle_db, self.near_present
        )
    }

    /// Index of the newest reference sample in the first half of the history.
    #[inline]
    fn newest(&self) -> usize {
        if self.pos == 0 {
            self.hist - 1
        } else {
            self.pos - 1
        }
    }

    /// Echo-cancel one mic sample. Returns `(cancelled, duck_gain)`, where
    /// `cancelled` is for the mic sample fed [`latency`](Self::latency) calls
    /// ago; the final output is their product (see [`process`](Self::process)).
    #[inline]
    pub fn process_parts(&mut self, d: f32, x: f32) -> (f32, f32) {
        self.x[self.pos] = x;
        self.x[self.pos + self.hist] = x;
        self.pos += 1;
        if self.pos == self.hist {
            self.pos = 0;
        }

        let ax = x.abs();
        if ax > REF_ACTIVE_RMS {
            self.since_ref_active = 0;
        } else {
            self.since_ref_active = self.since_ref_active.saturating_add(1);
        }

        self.env_acc_x += ax;
        self.env_acc_d += d.abs();
        self.env_cnt += 1;
        if self.env_cnt == self.env_len {
            self.push_env_frame();
        }

        let e = self.out_e[self.in_count];
        self.in_d[self.in_count] = d;
        self.in_count += 1;
        if self.in_count == self.b {
            self.in_count = 0;
            self.process_block();
        }

        let t = self.duck_target;
        let coef = if t < self.duck_gain {
            self.duck_attack
        } else {
            self.duck_release
        };
        self.duck_gain += (t - self.duck_gain) * coef;

        (e, self.duck_gain)
    }

    #[inline]
    pub fn process(&mut self, d: f32, x: f32) -> f32 {
        let (e, g) = self.process_parts(d, x);
        e * g
    }

    fn process_block(&mut self) {
        let (b, n, p) = (self.b, self.n, self.p);
        if !self.adaptive {
            self.out_e.copy_from_slice(&self.in_d);
            #[cfg(test)]
            if let Some(sp) = self.split.as_mut() {
                sp.out_echo.copy_from_slice(&sp.d_echo);
                sp.out_near.copy_from_slice(&sp.d_near);
            }
            self.update_duck();
            return;
        }

        // Reference frame aligned with this mic block (overlap-save: previous
        // block + current block), taken `delay` samples back in the history.
        let end = self.newest() + self.hist - self.delay + 1;
        let frame = &self.x[end - n..end];
        let ref_energy = frame.iter().map(|v| v * v).sum::<f32>() / n as f32;

        self.xf_head = (self.xf_head + p - 1) % p;
        {
            let slot = &mut self.xf[self.xf_head * n..(self.xf_head + 1) * n];
            for (c, &v) in slot.iter_mut().zip(frame) {
                *c = Cx::new(v, 0.0);
            }
            self.fft.forward(slot);
        }

        // Filter outputs of both weight sets (a-priori: weights from before
        // this block's update).
        self.buf_a.fill(Cx::default());
        self.buf_b.fill(Cx::default());
        for q in 0..p {
            let slot = (self.xf_head + q) % p;
            let xs = &self.xf[slot * n..(slot + 1) * n];
            let wf = &self.w_fg[q * n..(q + 1) * n];
            let wb = &self.w_bg[q * n..(q + 1) * n];
            for k in 0..n {
                self.buf_a[k] = self.buf_a[k].add(wf[k].mul(xs[k]));
                self.buf_b[k] = self.buf_b[k].add(wb[k].mul(xs[k]));
            }
        }
        self.fft.inverse(&mut self.buf_a);
        self.fft.inverse(&mut self.buf_b);

        let (mut pd, mut pef, mut peb) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..b {
            let d = self.in_d[i];
            let y = self.buf_a[b + i].re;
            let ef = d - y;
            let eb = d - self.buf_b[b + i].re;
            self.e_cur[i] = ef;
            self.y_cur[i] = y;
            self.e_bg_blk[i] = eb;
            pd += d * d;
            pef += ef * ef;
            peb += eb * eb;
        }

        if ref_energy > REF_ACTIVE_RMS * REF_ACTIVE_RMS {
            // Background update: normalised block LMS per bin.
            for i in 0..b {
                self.buf_a[i] = Cx::default();
                self.buf_a[b + i] = Cx::new(self.e_bg_blk[i], 0.0);
            }
            self.fft.forward(&mut self.buf_a);
            self.s_pow.fill(0.0);
            for q in 0..p {
                let xs = &self.xf[q * n..(q + 1) * n];
                for k in 0..n {
                    self.s_pow[k] += xs[k].norm_sqr();
                }
            }
            let delta = DELTA_REL * self.s_pow.iter().sum::<f32>() / n as f32 + 1e-12;
            for q in 0..p {
                let slot = (self.xf_head + q) % p;
                let xs = &self.xf[slot * n..(slot + 1) * n];
                let wb = &mut self.w_bg[q * n..(q + 1) * n];
                for k in 0..n {
                    let g = xs[k].conj().mul(self.buf_a[k]);
                    wb[k] = wb[k].add(g.scale(MU / (self.s_pow[k] + delta)));
                }
            }
            // Gradient constraint: zero the circular half of every partition.
            // Two partitions share one complex FFT pair (w_p real → re, w_q → im).
            let mut q = 0;
            while q < p {
                let q2 = (q + 1).min(p - 1);
                for k in 0..n {
                    let a = self.w_bg[q * n + k];
                    let c = if q2 != q { self.w_bg[q2 * n + k] } else { Cx::default() };
                    self.buf_b[k] = Cx::new(a.re - c.im, a.im + c.re); // a + j·c
                }
                self.fft.inverse(&mut self.buf_b);
                for v in &mut self.buf_b[b..] {
                    *v = Cx::default();
                }
                self.fft.forward(&mut self.buf_b);
                for k in 0..n {
                    let z = self.buf_b[k];
                    let zc = self.buf_b[(n - k) % n].conj();
                    self.w_bg[q * n + k] = z.add(zc).scale(0.5);
                    if q2 != q {
                        let diff = z.sub(zc); // = 2j·W_q
                        self.w_bg[q2 * n + k] = Cx::new(diff.im * 0.5, -diff.re * 0.5);
                    }
                }
                q += 2;
            }

            // Filter selection and ERLE on smoothed a-priori block powers.
            self.sm_d += STAT_ALPHA * (pd - self.sm_d);
            self.sm_efg += STAT_ALPHA * (pef - self.sm_efg);
            self.sm_ebg += STAT_ALPHA * (peb - self.sm_ebg);
            if self.sm_d > 1e-12 {
                if self.sm_ebg < 0.7 * self.sm_efg && self.sm_ebg < 0.5 * self.sm_d {
                    self.w_fg.copy_from_slice(&self.w_bg);
                    self.sm_efg = self.sm_ebg;
                } else if self.sm_ebg > 2.0 * self.sm_efg || self.sm_ebg > 1.5 * self.sm_d {
                    self.w_bg.copy_from_slice(&self.w_fg);
                    self.sm_ebg = self.sm_efg;
                }
                let erle_inst =
                    (10.0 * (self.sm_d / self.sm_efg.max(1e-20)).log10()).clamp(0.0, ERLE_MAX_DB);
                if self.near_present {
                    // (Decided by the suppressor one block ago.) Error is
                    // dominated by the user's voice: do not read it as lost ERLE.
                    // The slow decay self-corrects at the next pause.
                    self.erle_db = (self.erle_db - ERLE_DECAY_NEAR_DB).max(0.0);
                    self.erle_low_db = (self.erle_low_db - ERLE_DECAY_NEAR_DB).max(0.0);
                } else {
                    self.erle_db = erle_inst.max(self.erle_db - ERLE_DECAY_DB);
                    // Only blocks with real echo in them say anything about ERLE;
                    // in silence e ≈ d ≈ noise and the ratio is meaningless.
                    if pd > 1e-9 {
                        let a = if erle_inst < self.erle_low_db {
                            ERLE_LOW_ALPHA_DOWN
                        } else {
                            ERLE_LOW_ALPHA_UP
                        };
                        self.erle_low_db += a * (erle_inst - self.erle_low_db);
                    }
                }
            }
        }

        #[cfg(test)]
        if let Some(sp) = self.split.as_mut() {
            for i in 0..b {
                sp.e_echo_cur[i] = sp.d_echo[i] - self.y_cur[i];
                sp.near_cur[i] = sp.d_near[i];
            }
        }
        self.update_duck();
        self.suppress_block();
    }

    /// Per-bin residual echo suppression on the canceller output (see module docs).
    fn suppress_block(&mut self) {
        let (b, n) = (self.b, self.n);
        let active = self.echo_detected && self.ref_active();
        let gmin = 10f32.powf(-self.strength_db / 20.0);
        self.sup_floor_db = if active { self.strength_db } else { 0.0 };

        // Analysis of error and echo estimate in one complex FFT (e → re, y → im).
        for i in 0..b {
            self.buf_b[i] = Cx::new(self.win[i] * self.e_prev[i], self.win[i] * self.y_prev[i]);
            self.buf_b[b + i] =
                Cx::new(self.win[b + i] * self.e_cur[i], self.win[b + i] * self.y_cur[i]);
        }
        self.fft.forward(&mut self.buf_b);
        // Aligned reference spectrum: the newest partition the filter just used.
        let xs = &self.xf[self.xf_head * n..(self.xf_head + 1) * n];
        let (mut sy_max, mut sx_max) = (0.0f32, 0.0f32);
        for k in 0..=b {
            let z = self.buf_b[k];
            let zc = self.buf_b[(n - k) % n].conj();
            let e = z.add(zc).scale(0.5);
            let dif = z.sub(zc);
            let y = Cx::new(dif.im * 0.5, -dif.re * 0.5);
            self.spec_e[k] = e;
            let (pe, py, px) = (e.norm_sqr(), y.norm_sqr(), xs[k].norm_sqr());
            self.sup_se[k] += SUP_POW_ALPHA * (pe - self.sup_se[k]);
            self.sup_sy[k] += SUP_POW_ALPHA * (py - self.sup_sy[k]);
            self.sup_sx[k] += SUP_POW_ALPHA * (px - self.sup_sx[k]);
            self.sup_se_slow[k] += SUP_SLOW_ALPHA * (pe - self.sup_se_slow[k]);
            self.sup_sy_slow[k] += SUP_SLOW_ALPHA * (py - self.sup_sy_slow[k]);
            self.sup_sx_slow[k] += SUP_SLOW_ALPHA * (px - self.sup_sx_slow[k]);
            sy_max = sy_max.max(self.sup_sy_slow[k]);
            sx_max = sx_max.max(self.sup_sx_slow[k]);
        }

        // Residual prediction from voice-free predictors with minimum-statistics
        // ratios: talking can only raise se, so it cannot lower the minimum.
        let (mut se_sum, mut r_sum) = (0.0f32, 0.0f32);
        for k in 0..=b {
            let se = self.sup_se[k];
            let sy = self.sup_sy[k];
            let sx = self.sup_sx[k];
            if active {
                let (ses, sys, sxs) = (self.sup_se_slow[k], self.sup_sy_slow[k], self.sup_sx_slow[k]);
                if sys > 1e-3 * sy_max && sys > 1e-20 {
                    self.sup_leak_y[k] =
                        (ses / sys).min(self.sup_leak_y[k] * SUP_LEAK_RISE).clamp(1e-5, 10.0);
                }
                if sxs > 1e-3 * sx_max && sxs > 1e-20 {
                    self.sup_leak_x[k] =
                        (ses / sxs).min(self.sup_leak_x[k] * SUP_LEAK_RISE).clamp(1e-7, 10.0);
                }
            }
            self.sup_r[k] = (self.sup_leak_y[k] * sy).max(self.sup_leak_x[k] * sx);
            if k >= self.tune_near_min_bin {
                se_sum += se;
                r_sum += self.sup_r[k];
            }
        }

        // Self-calibrating bias: in echo-only frames se_sum/r_sum is the
        // prediction's shortfall; the running minimum tracks it because talking
        // only pushes the ratio up. Noise floor of the error, frozen while the
        // user talks. Near-end decision on the same quantities, with hangover.
        if active && r_sum > 1e-20 {
            self.sup_bias = (se_sum / r_sum)
                .min(self.sup_bias * SUP_LEAK_RISE)
                .clamp(SUP_BIAS_MIN, SUP_BIAS_MAX);
        }
        if !self.near_present {
            self.sup_floor = se_sum.min(self.sup_floor * FLOOR_RISE).max(1e-20);
        }
        if active && se_sum > self.tune_near_ratio * (self.sup_bias * r_sum).max(self.sup_floor) {
            self.near_hold = self.near_hold_blocks;
        } else {
            self.near_hold = self.near_hold.saturating_sub(1);
        }
        self.near_present = active && self.near_hold > 0;

        // Wiener gain per bin. Nobody talking: scale the prediction up so bins
        // with echo in them go to the floor; the user talking: unbiased
        // prediction, so bins their voice dominates stay near 1.
        let factor = (if self.tune_use_bias { self.sup_bias } else { 1.0 })
            * if self.tune_mode_switch && self.near_present {
                (self.tune_factor / self.tune_near_div).max(1.0)
            } else {
                self.tune_factor
            };
        for k in 0..=b {
            let g = if !active {
                1.0
            } else {
                let r = factor * self.sup_r[k];
                let near = (self.sup_se[k] - r).max(0.0);
                (near / (near + SUP_OVERSUB * r + 1e-30)).max(gmin)
            };
            let a = if g > self.sup_gain[k] { SUP_GAIN_UP } else { SUP_GAIN_DOWN };
            self.sup_gain[k] += a * (g - self.sup_gain[k]);
        }
        for k in 0..=b {
            let lo = k.saturating_sub(1);
            let hi = (k + 1).min(b);
            let mut acc = 0.0;
            for j in lo..=hi {
                acc += self.sup_gain[j];
            }
            self.sup_gain_sm[k] = acc / (hi - lo + 1) as f32;
        }

        // Synthesis: apply gains, mirror to a Hermitian spectrum, overlap-add.
        for k in 0..=b {
            self.buf_a[k] = self.spec_e[k].scale(self.sup_gain_sm[k]);
        }
        for k in b + 1..n {
            self.buf_a[k] = self.buf_a[n - k].conj();
        }
        self.fft.inverse(&mut self.buf_a);
        for i in 0..n {
            self.ola[i] += self.buf_a[i].re * self.win[i];
        }
        for i in 0..b {
            self.out_e[i] = self.ola[i];
            self.ola[i] = self.ola[b + i];
            self.ola[b + i] = 0.0;
        }
        self.e_prev.copy_from_slice(&self.e_cur);
        self.y_prev.copy_from_slice(&self.y_cur);

        #[cfg(test)]
        if let Some(sp) = self.split.as_mut() {
            stft_apply(&self.fft, &self.win, &sp.e_echo_prev, &sp.e_echo_cur, &self.sup_gain_sm, &mut self.buf_a, &mut sp.ola_echo, &mut sp.out_echo);
            stft_apply(&self.fft, &self.win, &sp.near_prev, &sp.near_cur, &self.sup_gain_sm, &mut self.buf_a, &mut sp.ola_near, &mut sp.out_near);
            sp.e_echo_prev.copy_from_slice(&sp.e_echo_cur);
            sp.near_prev.copy_from_slice(&sp.near_cur);
        }
    }

    fn update_duck(&mut self) {
        self.echo_detected = self.erle_db > ERLE_DETECT_DB || self.corr_peak > CORR_DETECT;
        self.duck_db = if !self.ref_active() {
            0.0
        } else if self.adaptive {
            0.0 // handled per bin by the residual suppressor
        } else {
            self.strength_db
        };
        self.duck_target = 10f32.powf(-self.duck_db / 20.0);
    }

    fn push_env_frame(&mut self) {
        let n = self.env_len as f32;
        self.env_x[self.env_pos] = self.env_acc_x / n;
        self.env_d[self.env_pos] = self.env_acc_d / n;
        self.env_acc_x = 0.0;
        self.env_acc_d = 0.0;
        self.env_cnt = 0;
        self.env_pos += 1;
        if self.env_pos == self.env_n {
            self.env_pos = 0;
        }
        if self.env_filled < self.env_n {
            self.env_filled += 1;
        }
        self.frames_since_est += 1;
        if self.frames_since_est >= self.est_every && self.env_filled == self.env_n {
            self.frames_since_est = 0;
            self.estimate_delay();
        }
    }

    /// Normalised cross-correlation of the mic and reference envelopes over the
    /// most recent two seconds, for lags 0..1 s. Re-positions the filter window
    /// when the peak is strong, the filter is not already cancelling, and the
    /// peak lies outside the current window's comfort zone.
    fn estimate_delay(&mut self) {
        for i in 0..self.env_n {
            let j = (self.env_pos + i) % self.env_n;
            self.lin_x[i] = self.env_x[j];
            self.lin_d[i] = self.env_d[j];
        }
        // Mic window ends `neg_lag` frames early so negative lags can be
        // searched too (reference shifted into the most recent frames).
        let n = self.env_n - self.max_lag - self.neg_lag;
        let nf = n as f32;
        let mic = &self.lin_d[self.max_lag..self.max_lag + n];

        let active = self.lin_x[self.max_lag..self.max_lag + n]
            .iter()
            .filter(|&&v| v > REF_ACTIVE_RMS)
            .count();
        if active < n / 4 {
            return; // speakers mostly silent: nothing to learn, hold state
        }
        let sd: f32 = mic.iter().sum();
        let sdd: f32 = mic.iter().map(|v| v * v).sum();
        let md = sd / nf;
        let vd = sdd / nf - md * md;
        if vd <= 1e-20 {
            return;
        }

        let mut best_l = 0i64;
        let mut best_c = -1.0f32;
        for l in -(self.neg_lag as i64)..self.max_lag as i64 {
            let start = (self.max_lag as i64 - l) as usize;
            let xr = &self.lin_x[start..start + n];
            let sx: f32 = xr.iter().sum();
            let sxx: f32 = xr.iter().map(|v| v * v).sum();
            let sxd = dot(xr, mic);
            let mx = sx / nf;
            let vx = sxx / nf - mx * mx;
            if vx <= 1e-20 {
                continue;
            }
            let c = (sxd / nf - mx * md) / (vx * vd).sqrt();
            if c > best_c {
                best_c = c;
                best_l = l;
            }
        }
        self.corr_peak = best_c.max(self.corr_peak * 0.8);
        if best_c > CORR_RELOCATE {
            self.best_lag_frames = best_l;
        }

        // Relocate only on two consecutive estimates that agree within 10 ms:
        // envelope correlation of real-room pickup can peak at a reflection.
        let consistent = matches!(self.prev_lag_frames, Some(p) if (p - best_l).abs() <= 4);
        self.prev_lag_frames = if best_c > CORR_RELOCATE { Some(best_l) } else { None };
        if best_c > CORR_RELOCATE && consistent && self.erle_db < ERLE_LOCK_DB && best_l >= 0 {
            let t = best_l as usize * self.env_len;
            let taps = self.taps();
            let lo = self.delay + taps / 8;
            let hi = self.delay + taps * 5 / 8;
            if t < lo || t > hi {
                let new_delay = t.saturating_sub(taps / 4).min(self.max_delay);
                if new_delay != self.delay {
                    self.delay = new_delay;
                    self.w_fg.fill(Cx::default());
                    self.w_bg.fill(Cx::default());
                    self.xf.fill(Cx::default());
                    self.erle_db = 0.0;
                    self.erle_low_db = 0.0;
                    self.sm_d = 0.0;
                    self.sm_efg = 0.0;
                    self.sm_ebg = 0.0;
                    self.sup_bias = 1.0;
                    self.sup_floor = 1.0;
                    self.near_hold = 0;
                    self.sup_leak_y.fill(1.0);
                    self.sup_leak_x.fill(1.0);
                    self.sup_se.fill(0.0);
                    self.sup_sy.fill(0.0);
                    self.sup_sx.fill(0.0);
                    self.sup_se_slow.fill(0.0);
                    self.sup_sy_slow.fill(0.0);
                    self.sup_sx_slow.fill(0.0);
                }
            }
        }
    }

    /// Nominal echo delay estimate in milliseconds (centre of the filter window).
    pub fn delay_ms(&self) -> f32 {
        (self.delay + self.taps() / 4) as f32 * 1000.0 / self.sr as f32
    }

    /// Best envelope-correlation lag in ms (negative = mic leads the reference).
    pub fn best_lag_ms(&self) -> f32 {
        self.best_lag_frames as f32 * self.env_len as f32 * 1000.0 / self.sr as f32
    }

    pub fn publish(&self, s: &Shared) {
        s.ref_active.store(self.ref_active(), Ordering::Relaxed);
        s.echo_detected.store(self.echo_detected, Ordering::Relaxed);
        s.near_active.store(self.near_present, Ordering::Relaxed);
        Shared::set_f32(&s.erle_db, self.erle_low_db);
        Shared::set_f32(&s.duck_db, if self.adaptive { self.sup_floor_db } else { self.duck_db });
        Shared::set_f32(&s.delay_ms, self.delay_ms());
        Shared::set_f32(&s.corr, self.corr_peak);
        Shared::set_f32(&s.lag_ms, self.best_lag_ms());
        Shared::set_f32(&s.window_ms, self.delay as f32 * 1000.0 / self.sr as f32);
        Shared::set_f32(&s.erle_max_db, self.erle_db);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48000;

    /// Deterministic LCG so tests do not need the `rand` crate.
    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    /// The repo's 5.7 s speech fixture, looped to `secs`, scaled to `rms`.
    fn voice(secs: f32, rms: f32, offset_secs: f32) -> Vec<f32> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test_voice.wav");
        let mut r = hound::WavReader::open(path).expect("fixture");
        assert_eq!(r.spec().sample_rate, SR);
        let raw: Vec<f32> = r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
        let cur = (raw.iter().map(|v| v * v).sum::<f32>() / raw.len() as f32).sqrt();
        let g = rms / cur;
        let n = (secs * SR as f32) as usize;
        let off = (offset_secs * SR as f32) as usize;
        (0..n).map(|i| raw[(i + off) % raw.len()] * g).collect()
    }

    /// Echo path: bulk delay plus a decaying cluster of early reflections.
    fn echo_path(delay_ms: f32) -> (usize, Vec<f32>) {
        let delay = (delay_ms * SR as f32 / 1000.0) as usize;
        let mut rng = Lcg(7);
        let mut h = vec![0.0f32; (0.030 * SR as f32) as usize];
        h[0] = 0.08;
        for (i, v) in h.iter_mut().enumerate().skip(1) {
            if i % 97 == 0 {
                *v = 0.05 * rng.next_f32() * (-(i as f32) / 600.0).exp();
            }
        }
        (delay, h)
    }

    fn convolve(x: &[f32], delay: usize, h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (k, &hk) in h.iter().enumerate() {
            if hk == 0.0 {
                continue;
            }
            let shift = delay + k;
            for i in shift..x.len() {
                y[i] += hk * x[i - shift];
            }
        }
        y
    }

    fn db(num: f32, den: f32) -> f32 {
        10.0 * (num / den.max(1e-30)).log10()
    }

    #[test]
    fn resampler_identity_and_ratio() {
        let mut r = LinearResampler::new(48000, 48000);
        let mut out = Vec::new();
        for i in 0..100 {
            r.push(i as f32, |s| out.push(s));
        }
        assert_eq!(out.len(), 100);
        assert_eq!(out[57], 57.0);

        let mut r = LinearResampler::new(48000, 44100);
        let mut out = Vec::new();
        for i in 0..48000 {
            // 1 kHz sine, one second
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin();
            r.push(s, |v| out.push(v));
        }
        assert!((out.len() as i64 - 44100).abs() <= 2, "got {} samples", out.len());
        let rms = (out.iter().map(|v| v * v).sum::<f32>() / out.len() as f32).sqrt();
        assert!((rms - 0.7071).abs() < 0.01, "rms {}", rms);
    }

    #[test]
    fn fft_roundtrip_matches_dft() {
        let f = Fft::new(16);
        let mut rng = Lcg(3);
        let sig: Vec<Cx> = (0..16).map(|_| Cx::new(rng.next_f32(), rng.next_f32())).collect();
        let mut buf = sig.clone();
        f.forward(&mut buf);
        for k in 0..16 {
            let mut acc = Cx::default();
            for (t, v) in sig.iter().enumerate() {
                let a = -2.0 * std::f32::consts::PI * (k * t) as f32 / 16.0;
                acc = acc.add(v.mul(Cx::new(a.cos(), a.sin())));
            }
            assert!((acc.re - buf[k].re).abs() < 1e-4 && (acc.im - buf[k].im).abs() < 1e-4);
        }
        f.inverse(&mut buf);
        for (a, b) in sig.iter().zip(&buf) {
            assert!((a.re - b.re).abs() < 1e-5 && (a.im - b.im).abs() < 1e-5);
        }
    }

    #[test]
    fn converges_on_delayed_echo() {
        let x = voice(16.0, 0.1, 0.0);
        let (delay, h) = echo_path(50.0);
        let echo = convolve(&x, delay, &h);
        let mut rng = Lcg(1);
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        let lat = aec.latency();
        let mut d_all = Vec::with_capacity(x.len());
        let mut e_all = Vec::with_capacity(x.len());
        let t0 = std::time::Instant::now();
        for i in 0..x.len() {
            let d = echo[i] + 1e-4 * rng.next_f32();
            let (e, _) = aec.process_parts(d, x[i]);
            d_all.push(d);
            e_all.push(e);
        }
        let cpu = t0.elapsed().as_secs_f32() / (x.len() as f32 / SR as f32);
        eprintln!("CPU: {:.1}% of one core for real-time at {} Hz", cpu * 100.0, SR);
        let tail = x.len() - 3 * SR as usize;
        let pd: f32 = d_all[tail - lat..x.len() - lat].iter().map(|v| v * v).sum();
        let pe: f32 = e_all[tail..].iter().map(|v| v * v).sum();
        let erle = db(pd, pe);
        let start = aec.window_start();
        eprintln!(
            "ERLE {:.1} dB, window starts at {} samples ({} taps), est {:.1} ms, held ERLE {:.1}, conservative {:.1}",
            erle, start, aec.taps(), aec.delay_ms(), aec.erle_db(), aec.erle_low_db()
        );
        assert!(erle > 15.0, "ERLE {:.1} dB", erle);
        assert!(start <= delay && delay < start + aec.taps(), "window {}..{} misses {}", start, start + aec.taps(), delay);
        assert!(aec.echo_detected());
        assert!(aec.erle_db() > 10.0);
        // Adaptive ducking should be the leftover of strength − ERLE, not full strength.
        assert!(aec.duck_gain() > 10f32.powf(-40.0 / 20.0) * 3.0);
    }

    #[test]
    fn relocates_window_for_long_delay() {
        // 150 ms is outside the initial 85 ms window: the envelope estimator must
        // move the window, after which the filter converges from scratch.
        let x = voice(16.0, 0.1, 0.0);
        let (delay, h) = echo_path(150.0);
        let echo = convolve(&x, delay, &h);
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        let lat = aec.latency();
        let mut d_all = Vec::with_capacity(x.len());
        let mut e_all = Vec::with_capacity(x.len());
        for i in 0..x.len() {
            let (e, _) = aec.process_parts(echo[i], x[i]);
            d_all.push(echo[i]);
            e_all.push(e);
        }
        let tail = x.len() - 3 * SR as usize;
        let pd: f32 = d_all[tail - lat..x.len() - lat].iter().map(|v| v * v).sum();
        let pe: f32 = e_all[tail..].iter().map(|v| v * v).sum();
        let erle = db(pd, pe);
        let start = aec.window_start();
        eprintln!("long delay: ERLE {:.1} dB, window {}..{} for true delay {}, est {:.1} ms", erle, start, start + aec.taps(), delay, aec.delay_ms());
        assert!(start <= delay && delay < start + aec.taps(), "window {}..{} misses {}", start, start + aec.taps(), delay);
        assert!(erle > 15.0, "ERLE {:.1} dB", erle);
        assert!((aec.best_lag_ms() - 150.0).abs() < 5.0, "lag estimate {:.1} ms", aec.best_lag_ms());
    }

    #[test]
    fn negative_lag_is_reported_not_used() {
        // Mic leading the reference by 40 ms: the causal filter cannot model it,
        // but the estimator must report the negative lag so the app can tell.
        let x = voice(10.0, 0.1, 0.0);
        let (_, h) = echo_path(0.0);
        let echo = convolve(&x, 0, &h);
        let shift = (0.040 * SR as f32) as usize;
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        for i in 0..x.len() - shift {
            // reference delivered `shift` samples late relative to the echo
            aec.process_parts(echo[i + shift], x[i]);
        }
        eprintln!("negative lag: estimate {:.1} ms, held ERLE {:.1}", aec.best_lag_ms(), aec.erle_db());
        assert!((aec.best_lag_ms() + 40.0).abs() < 5.0, "lag estimate {:.1} ms", aec.best_lag_ms());
        assert_eq!(aec.window_start(), 0);
    }

    #[test]
    fn double_talk_keeps_near_end_and_recovers() {
        let secs = 18.0;
        let x = voice(secs, 0.1, 0.0);
        let near_full = voice(secs, 0.02, 2.3);
        let (delay, h) = echo_path(35.0);
        let echo = convolve(&x, delay, &h);
        let n = x.len();
        let dt_start = 7 * SR as usize;
        let dt_end = 13 * SR as usize;
        let mut rng = Lcg(11);
        let mut aec = EchoCanceller::new(SR);
        // Strength above the 40 dB ERLE cap so far-end-only periods are ducked
        // (by 20 dB) and the near-end periods have something to release from.
        aec.set_params(true, 60.0);
        let lat = aec.latency();
        let mut near = vec![0.0f32; n];
        let mut d_all = vec![0.0f32; n];
        let mut out_all = vec![0.0f32; n];
        let mut out_echo = vec![0.0f32; n];
        let mut out_near = vec![0.0f32; n];
        // Far-end-only suppression is judged only while the speakers are playing.
        let (mut p_far_in, mut p_far_out) = (0.0f32, 0.0f32);
        for i in 0..n {
            near[i] = if i >= dt_start && i < dt_end { near_full[i] } else { 0.0 };
            let d_echo = echo[i] + 1.5e-4 * rng.next_f32(); // ≈ −76 dBFS mic noise
            d_all[i] = d_echo + near[i];
            let (o, oe, on) = aec.process_split(d_echo, near[i], x[i]);
            out_all[i] = o;
            out_echo[i] = oe;
            out_near[i] = on;
            if i >= 5 * SR as usize && i < dt_start && aec.ref_active() {
                p_far_in += d_all[i - lat] * d_all[i - lat];
                p_far_out += out_all[i] * out_all[i];
            }
        }
        let far_atten_db = db(p_far_out, p_far_in);
        // Talk-over: how much speaker audio is left in the output (echo part,
        // relative to the raw echo in the mic) and how much the voice changed.
        let (mut p_echo, mut p_leak, mut p_near, mut p_vdist, mut s_on, mut s_nn) =
            (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in dt_start + SR as usize..dt_end {
            let nr = near[i - lat];
            let ec = echo[i - lat];
            p_echo += ec * ec;
            p_leak += out_echo[i] * out_echo[i];
            p_near += nr * nr;
            p_vdist += (out_near[i] - nr) * (out_near[i] - nr);
            s_on += out_near[i] * nr;
            s_nn += nr * nr;
        }
        let leak_db = db(p_leak, p_echo);
        let voice_dist_db = db(p_vdist, p_near);
        let voice_gain = s_on / s_nn;
        eprintln!(
            "talk-over: speaker leakage {:.1} dB rel. raw echo, voice gain {:.2}, voice distortion {:.1} dB",
            leak_db, voice_gain, voice_dist_db
        );
        let (mut pd_tail, mut pe_tail) = (0.0f32, 0.0f32);
        for i in n - 3 * SR as usize..n {
            pd_tail += d_all[i - lat] * d_all[i - lat];
            pe_tail += out_all[i] * out_all[i];
        }
        let erle_after = db(pd_tail, pe_tail);
        eprintln!(
            "output attenuation after double-talk {:.1} dB, conservative ERLE {:.1}, far-end-only output attenuation {:.1} dB",
            erle_after, aec.erle_low_db(), far_atten_db
        );
        assert!(erle_after > 12.0, "attenuation after double-talk {:.1} dB", erle_after);
        assert!(far_atten_db < -30.0, "far-end-only period not suppressed: {:.1} dB", far_atten_db);
        assert!(voice_gain > 0.8, "user's voice attenuated during talk-over: gain {:.2}", voice_gain);
        assert!(voice_dist_db < -15.0, "voice distortion during talk-over {:.1} dB", voice_dist_db);
        assert!(leak_db < -32.0, "speaker leakage during talk-over only {:.1} dB", leak_db);
    }

    #[test]
    fn weak_canceller_keeps_voice_audible() {
        // A saturating speaker (tanh) makes the echo path nonlinear, so the
        // linear canceller achieves little, and the user's voice sits 6 dB below
        // the echo at the mic. Whatever happens, the voice must stay audible.
        let secs = 16.0;
        let x = voice(secs, 0.1, 0.0);
        let near_full = voice(secs, 0.0045, 2.3); // echo RMS ≈ 0.009 → voice −6 dB
        let (delay, h) = echo_path(40.0);
        let lin = convolve(&x, delay, &h);
        let echo: Vec<f32> = lin.iter().map(|&v| (v * 25.0).tanh() / 25.0).collect();
        let n = x.len();
        let dt_start = 6 * SR as usize;
        let dt_end = 14 * SR as usize;
        let mut rng = Lcg(5);
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        let lat = aec.latency();
        let mut near = vec![0.0f32; n];
        let mut out_echo = vec![0.0f32; n];
        let mut out_near = vec![0.0f32; n];
        for i in 0..n {
            near[i] = if i >= dt_start && i < dt_end { near_full[i] } else { 0.0 };
            let d_echo = echo[i] + 1.5e-4 * rng.next_f32();
            let (_, oe, on) = aec.process_split(d_echo, near[i], x[i]);
            out_echo[i] = oe;
            out_near[i] = on;
        }
        let (mut p_echo, mut p_leak, mut p_near, mut s_on, mut s_nn) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in dt_start + SR as usize..dt_end {
            let nr = near[i - lat];
            let ec = echo[i - lat];
            p_echo += ec * ec;
            p_leak += out_echo[i] * out_echo[i];
            p_near += nr * nr;
            s_on += out_near[i] * nr;
            s_nn += nr * nr;
        }
        let voice_gain = s_on / s_nn;
        let leak_db = db(p_leak, p_echo);
        let (mut p_far_e, mut p_far_o) = (0.0f32, 0.0f32);
        for i in 4 * SR as usize..dt_start {
            p_far_e += echo[i - lat] * echo[i - lat];
            p_far_o += out_echo[i] * out_echo[i];
        }
        eprintln!(
            "weak canceller: conservative ERLE {:.1} dB, max {:.1}; far-end-only echo out {:.1} dB; talk-over voice gain {:.2} ({:.1} dB), speaker leakage {:.1} dB",
            aec.erle_low_db(), aec.erle_db(), db(p_far_o, p_far_e), voice_gain, 20.0 * voice_gain.log10(), leak_db
        );
        let _ = p_near;
        assert!(voice_gain > 0.7, "voice attenuated to {:.2} with a weak canceller", voice_gain);
        eprintln!("weak canceller state: {}", aec.debug_near());
        // A saturating speaker leaves level-dependent residue that linear
        // predictors under-estimate at loud moments, so suppression is modest
        // here; the voice guarantee is what this test exists for.
        assert!(leak_db < -8.0, "speaker audio barely reduced: {:.1} dB", leak_db);
    }

    /// Offline harness on a real `echo_diag.wav` (L = raw mic, R = aligned
    /// reference) recorded by the app. Set ECHO_DIAG_WAV to run it. Prints
    /// per-second behaviour and, with the voice fixture mixed in at seconds 3-6,
    /// talk-over voice gain and echo leakage on the real room.
    #[test]
    fn offline_real_recording() {
        let Ok(path) = std::env::var("ECHO_DIAG_WAV") else {
            return;
        };
        let mut r = hound::WavReader::open(&path).expect("open wav");
        let spec = r.spec();
        assert_eq!(spec.channels, 2);
        let sr = spec.sample_rate;
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => r.samples::<f32>().map(|v| v.unwrap()).collect(),
            hound::SampleFormat::Int => r.samples::<i32>().map(|v| v.unwrap() as f32 / (1u32 << (spec.bits_per_sample - 1)) as f32).collect(),
        };
        let n = samples.len() / 2;
        let mic: Vec<f32> = (0..n).map(|i| samples[2 * i]).collect();
        let reff: Vec<f32> = (0..n).map(|i| samples[2 * i + 1]).collect();
        let mic_rms = (mic.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
        let voice_level: f32 = std::env::var("ECHO_DIAG_VOICE_DB").ok().and_then(|v| v.parse().ok()).unwrap_or(12.0);
        let near_full = voice(n as f32 / sr as f32, mic_rms * 10f32.powf(voice_level / 20.0), 2.3);
        let strength: f32 = std::env::var("ECHO_DIAG_STRENGTH").ok().and_then(|v| v.parse().ok()).unwrap_or(40.0);
        let (talk_start, talk_end) = (3 * sr as usize, 6 * sr as usize);
        let mut aec = EchoCanceller::new(sr);
        aec.set_params(true, strength);
        let envf = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
        if std::env::var("ECHO_DIAG_FACTOR").is_ok() {
            aec.set_tuning(envf("ECHO_DIAG_FACTOR", SUP_FACTOR_AT_40), envf("ECHO_DIAG_BIAS", 0.0) > 0.5, envf("ECHO_DIAG_SWITCH", 0.0) > 0.5);
        }
        aec.set_near_tuning(envf("ECHO_DIAG_NEAR_RATIO", NEAR_RATIO), envf("ECHO_DIAG_NEAR_MINHZ", NEAR_MIN_HZ));
        aec.set_near_divisor(envf("ECHO_DIAG_NEAR_DIV", 1.0));
        let lat = aec.latency();
        let mut out_all = vec![0.0f32; n];
        let mut out_echo = vec![0.0f32; n];
        let mut out_near = vec![0.0f32; n];
        let mut near_flags = vec![false; n];
        let mut per_sec: Vec<String> = Vec::new();
        let (mut p_in, mut p_out, mut near_cnt) = (0.0f32, 0.0f32, 0usize);
        for i in 0..n {
            let near = if i >= talk_start && i < talk_end { near_full[i] } else { 0.0 };
            let (o, oe, on) = aec.process_split(mic[i], near, reff[i]);
            out_all[i] = o;
            out_echo[i] = oe;
            out_near[i] = on;
            near_flags[i] = aec.near_end_active();
            if i >= lat {
                p_in += (mic[i - lat] + if i - lat >= talk_start && i - lat < talk_end { near_full[i - lat] } else { 0.0 }).powi(2);
            }
            p_out += o * o;
            if aec.near_end_active() {
                near_cnt += 1;
            }
            if (i + 1) % sr as usize == 0 {
                let sec = (i + 1) / sr as usize;
                per_sec.push(format!(
                    "  {:2}s: out/in {:6.1} dB · near {:3.0}% · ERLE max {:4.1} typ {:4.1} · corr {:.2} · lag {:+5.1} ms · win {:3.0} ms · bias {:5.2} · floor {:.1e} · ref_active {}",
                    sec, db(p_out, p_in), 100.0 * near_cnt as f32 / sr as f32, aec.erle_db(), aec.erle_low_db(), aec.corr_peak,
                    aec.best_lag_ms(), aec.window_start() as f32 * 1000.0 / sr as f32, aec.sup_bias, aec.sup_floor, aec.ref_active()
                ));
                p_in = 0.0;
                p_out = 0.0;
                near_cnt = 0;
            }
        }
        eprintln!("real recording {} ({} Hz, {:.1} s, mic rms {:.5}), voice +{} dB at 3-6 s, strength {} dB", path, sr, n as f32 / sr as f32, mic_rms, voice_level, strength);
        for l in &per_sec {
            eprintln!("{}", l);
        }
        // Talk-over metrics on the real echo (seconds 3.5-6)
        let (mut p_echo, mut p_leak, mut s_on, mut s_nn, mut p_vd) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for i in talk_start + sr as usize / 2..talk_end {
            let nr = near_full[i - lat];
            let ec = mic[i - lat];
            p_echo += ec * ec;
            p_leak += out_echo[i] * out_echo[i];
            s_on += out_near[i] * nr;
            s_nn += nr * nr;
            p_vd += (out_near[i] - nr) * (out_near[i] - nr);
        }
        let vg = s_on / s_nn;
        eprintln!("talk-over on real echo: speaker leakage {:.1} dB rel. raw mic echo, voice gain {:.2} ({:.1} dB), voice distortion {:.1} dB", db(p_leak, p_echo), vg, 20.0 * vg.log10(), db(p_vd, s_nn));
        // Silent-user suppression (seconds 7-9, speakers playing)
        let (mut pi, mut po) = (0.0f32, 0.0f32);
        for i in 7 * sr as usize..9 * sr as usize {
            pi += mic[i - lat] * mic[i - lat];
            po += out_all[i] * out_all[i];
        }
        eprintln!("user silent 7-9 s: output {:.1} dB relative to raw mic", db(po, pi));
        let frac = |a: usize, b: usize| 100.0 * near_flags[a..b].iter().filter(|&&f| f).count() as f32 / (b - a) as f32;
        eprintln!("near-end flagged: talking 3.5-6 s {:.0}%, silent 7-10 s {:.0}%", frac(talk_start + sr as usize / 2, talk_end), frac(7 * sr as usize, n));
    }

    #[test]
    fn headphones_no_echo_passes_mic_untouched() {
        let x = voice(10.0, 0.1, 0.0);
        let near = voice(10.0, 0.02, 2.3);
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        let lat = aec.latency();
        let mut out = Vec::with_capacity(x.len());
        let mut min_gain = 1.0f32;
        let mut max_erle = 0.0f32;
        for i in 0..x.len() {
            out.push(aec.process(near[i], x[i]));
            if i >= 2 * SR as usize {
                min_gain = min_gain.min(aec.duck_gain());
            }
            max_erle = max_erle.max(aec.erle_db());
        }
        let (mut p_in, mut p_diff) = (0.0f32, 0.0f32);
        for i in 2 * SR as usize..x.len() {
            let nr = near[i - lat];
            p_in += nr * nr;
            p_diff += (out[i] - nr) * (out[i] - nr);
        }
        let dist = db(p_diff, p_in);
        eprintln!(
            "headphones: distortion {:.1} dB, min duck gain {:.3}, corr peak {:.2}, held ERLE {:.1}, max ERLE {:.1}",
            dist, min_gain, aec.corr_peak, aec.erle_db(), max_erle
        );
        assert!(dist < -25.0, "distortion {:.1} dB", dist);
        assert!(!aec.echo_detected());
        assert!(min_gain > 0.9, "mic was ducked without echo: gain {}", min_gain);
    }

    #[test]
    fn simple_mode_ducks_while_speakers_play() {
        let x = voice(4.0, 0.1, 0.0);
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(false, 40.0);
        let mut gain_at_1s = 1.0;
        let mut gain_at_end = 0.0;
        for i in 0..4 * SR as usize {
            // speakers play for the first two seconds, then silence
            let xi = if i < 2 * SR as usize { x[i] } else { 0.0 };
            aec.process(0.01, xi);
            if i == SR as usize {
                gain_at_1s = aec.duck_gain();
            }
            if i == 4 * SR as usize - 1 {
                gain_at_end = aec.duck_gain();
            }
        }
        let target = 10f32.powf(-40.0 / 20.0);
        assert!(gain_at_1s < target * 1.5, "not ducked: {}", gain_at_1s);
        assert!(gain_at_end > 0.95, "did not release: {}", gain_at_end);
    }
}
