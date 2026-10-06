//! Speaker echo suppression.
//!
//! Removes what the PC is playing (podcasts, videos, call audio) from the mic
//! signal, using a loopback copy of the speaker mix as the reference. Runs at
//! the mic rate, before the boost gain is applied, and adds one block
//! ([`EchoCanceller::latency`], ~5 ms) of latency to the mic path.
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
//! * Ducking (residual suppression). Adaptive mode: attenuate by
//!   `strength − measured ERLE` while speakers are playing *and* echo is
//!   detected in the mic (filter is cancelling, or envelopes correlate strongly),
//!   so headphone users are never ducked. Simple mode: attenuate by `strength`
//!   whenever the speakers are playing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Reference RMS above which the speakers count as "playing" (≈ −50 dBFS, the
/// same floor the auto-calibration uses to tell speech from silence).
pub const REF_ACTIVE_RMS: f32 = 0.003;

const WINDOW_SEC: f32 = 0.085;
const MAX_DELAY_SEC: f32 = 1.0;
const ENV_FRAMES_PER_SEC: u32 = 400; // 2.5 ms envelope frames
const ENV_HISTORY_SEC: f32 = 3.0;
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

/// Settings and live stats shared between the UI thread and the audio callback.
pub struct Shared {
    pub adaptive: AtomicBool,
    pub strength_db: AtomicU32, // f32 bits
    pub ref_active: AtomicBool,
    pub echo_detected: AtomicBool,
    pub erle_db: AtomicU32,  // f32 bits
    pub duck_db: AtomicU32,  // f32 bits
    pub delay_ms: AtomicU32, // f32 bits
    pub corr: AtomicU32,     // f32 bits
}

impl Shared {
    pub fn new(adaptive: bool, strength_db: f32) -> Self {
        Self {
            adaptive: AtomicBool::new(adaptive),
            strength_db: AtomicU32::new(strength_db.to_bits()),
            ref_active: AtomicBool::new(false),
            echo_detected: AtomicBool::new(false),
            erle_db: AtomicU32::new(0f32.to_bits()),
            duck_db: AtomicU32::new(0f32.to_bits()),
            delay_ms: AtomicU32::new(0f32.to_bits()),
            corr: AtomicU32::new(0f32.to_bits()),
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
    sm_d: f32,
    sm_efg: f32,
    sm_ebg: f32,

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
    frames_since_est: usize,
    est_every: usize,

    // Reference activity and ducking state.
    since_ref_active: usize,
    ref_hold: usize,
    corr_peak: f32,
    erle_db: f32,
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
            sm_d: 0.0,
            sm_efg: 0.0,
            sm_ebg: 0.0,
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
            frames_since_est: 0,
            est_every,
            since_ref_active: usize::MAX / 2,
            ref_hold: (REF_HOLD_SEC * srf) as usize,
            corr_peak: 0.0,
            erle_db: 0.0,
            echo_detected: false,
            duck_db: 0.0,
            duck_target: 1.0,
            duck_gain: 1.0,
            duck_attack: 1.0 - (-1.0 / (0.005 * srf)).exp(),
            duck_release: 1.0 - (-1.0 / (0.2 * srf)).exp(),
        }
    }

    pub fn set_params(&mut self, adaptive: bool, strength_db: f32) {
        self.adaptive = adaptive;
        self.strength_db = strength_db;
    }

    /// Filter length in samples.
    pub fn taps(&self) -> usize {
        self.p * self.b
    }

    /// Samples of delay this stage adds to the mic path (one block).
    pub fn latency(&self) -> usize {
        self.b
    }

    /// Start (in samples before "now") of the filter window.
    pub fn window_start(&self) -> usize {
        self.delay
    }

    pub fn erle_db(&self) -> f32 {
        self.erle_db
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
            let ef = d - self.buf_a[b + i].re;
            let eb = d - self.buf_b[b + i].re;
            self.out_e[i] = ef;
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
                self.erle_db = erle_inst.max(self.erle_db - ERLE_DECAY_DB);
            }
        }

        self.update_duck();
    }

    fn update_duck(&mut self) {
        self.echo_detected = self.erle_db > ERLE_DETECT_DB || self.corr_peak > CORR_DETECT;
        self.duck_db = if !self.ref_active() {
            0.0
        } else if self.adaptive {
            if self.echo_detected {
                (self.strength_db - self.erle_db).max(0.0)
            } else {
                0.0
            }
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
        let n = self.env_n - self.max_lag;
        let nf = n as f32;
        let mic = &self.lin_d[self.max_lag..];

        let active = self.lin_x[self.max_lag..]
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

        let mut best_l = 0usize;
        let mut best_c = -1.0f32;
        for l in 0..self.max_lag {
            let xr = &self.lin_x[self.max_lag - l..self.env_n - l];
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

        if best_c > CORR_RELOCATE && self.erle_db < ERLE_LOCK_DB {
            let t = best_l * self.env_len;
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
                    self.sm_d = 0.0;
                    self.sm_efg = 0.0;
                    self.sm_ebg = 0.0;
                }
            }
        }
    }

    /// Nominal echo delay estimate in milliseconds (centre of the filter window).
    pub fn delay_ms(&self) -> f32 {
        (self.delay + self.taps() / 4) as f32 * 1000.0 / self.sr as f32
    }

    pub fn publish(&self, s: &Shared) {
        s.ref_active.store(self.ref_active(), Ordering::Relaxed);
        s.echo_detected.store(self.echo_detected, Ordering::Relaxed);
        Shared::set_f32(&s.erle_db, self.erle_db);
        Shared::set_f32(&s.duck_db, self.duck_db);
        Shared::set_f32(&s.delay_ms, self.delay_ms());
        Shared::set_f32(&s.corr, self.corr_peak);
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
            "ERLE {:.1} dB, window starts at {} samples ({} taps), est {:.1} ms, held ERLE {:.1}",
            erle, start, aec.taps(), aec.delay_ms(), aec.erle_db()
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
        let mut aec = EchoCanceller::new(SR);
        aec.set_params(true, 40.0);
        let lat = aec.latency();
        let mut near = vec![0.0f32; n];
        let mut d_all = vec![0.0f32; n];
        let mut e_all = vec![0.0f32; n];
        let mut min_gain_dt = 1.0f32;
        for i in 0..n {
            near[i] = if i >= dt_start && i < dt_end { near_full[i] } else { 0.0 };
            d_all[i] = echo[i] + near[i];
            let (e, g) = aec.process_parts(d_all[i], x[i]);
            e_all[i] = e;
            if i >= dt_start + SR as usize && i < dt_end {
                min_gain_dt = min_gain_dt.min(g);
            }
        }
        let (mut p_near, mut p_dist) = (0.0f32, 0.0f32);
        for i in dt_start + SR as usize..dt_end {
            let nr = near[i - lat];
            p_near += nr * nr;
            p_dist += (e_all[i] - nr) * (e_all[i] - nr);
        }
        let (mut pd_tail, mut pe_tail) = (0.0f32, 0.0f32);
        for i in n - 3 * SR as usize..n {
            pd_tail += d_all[i - lat] * d_all[i - lat];
            pe_tail += e_all[i] * e_all[i];
        }
        let distortion = db(p_dist, p_near);
        let erle_after = db(pd_tail, pe_tail);
        eprintln!(
            "near-end distortion {:.1} dB, ERLE after double-talk {:.1} dB, min duck gain during double-talk {:.2}",
            distortion, erle_after, min_gain_dt
        );
        assert!(distortion < -10.0, "near-end distortion {:.1} dB", distortion);
        assert!(erle_after > 12.0, "ERLE after double-talk {:.1} dB", erle_after);
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
