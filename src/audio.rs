use crate::filter::{Biquad, FilterKind};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, Stream};
use ringbuf::HeapRb;
use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};
use rodio::Source as _;
use std::fs::File;
use std::io::BufReader;
use std::time::Duration;

/// A band strip always shows the active bands; the strips themselves are
/// preallocated up to this cap so the audio thread never reallocates.
pub const MAX_BANDS: usize = 256;
/// Default number of active bands shown when the app starts.
pub const NUM_BANDS: usize = 6;
/// Console (training) mode locks the strip to this many bands, like the EQ
/// section of a real mixing desk.
pub const CONSOLE_BANDS: usize = 5;

const FFT_SIZE: usize = 2048;
pub const NUM_BARS: usize = 32;

/// One EQ band's parameters, stored as atomics so the UI thread can write
/// them and the real-time audio callback can read them without ever taking
/// a lock (no mutex on the audio thread, full stop).
pub struct BandParams {
    pub freq_hz: AtomicU32,   // f32 bits
    pub gain_db: AtomicU32,   // f32 bits
    pub q: AtomicU32,         // f32 bits
    pub kind: AtomicU8,       // index into FilterKind::ALL
    pub enabled: AtomicBool,
}

impl BandParams {
    fn new(freq_hz: f32, gain_db: f32, q: f32, kind: FilterKind) -> Self {
        Self {
            freq_hz: AtomicU32::new(freq_hz.to_bits()),
            gain_db: AtomicU32::new(gain_db.to_bits()),
            q: AtomicU32::new(q.to_bits()),
            kind: AtomicU8::new(kind_to_idx(kind)),
            enabled: AtomicBool::new(true),
        }
    }

    pub fn freq(&self) -> f32 {
        f32::from_bits(self.freq_hz.load(Ordering::Relaxed))
    }
    pub fn set_freq(&self, v: f32) {
        self.freq_hz.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_db.load(Ordering::Relaxed))
    }
    pub fn set_gain(&self, v: f32) {
        self.gain_db.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn q(&self) -> f32 {
        f32::from_bits(self.q.load(Ordering::Relaxed))
    }
    pub fn set_q(&self, v: f32) {
        self.q.store(v.to_bits(), Ordering::Relaxed);
    }
    pub fn kind(&self) -> FilterKind {
        idx_to_kind(self.kind.load(Ordering::Relaxed))
    }
    pub fn set_kind(&self, k: FilterKind) {
        self.kind.store(kind_to_idx(k), Ordering::Relaxed);
    }
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }
}

fn kind_to_idx(k: FilterKind) -> u8 {
    FilterKind::ALL.iter().position(|x| *x == k).unwrap() as u8
}
fn idx_to_kind(i: u8) -> FilterKind {
    FilterKind::ALL[i as usize % FilterKind::ALL.len()]
}

/// Default spot (frequency, shape) for band index `i`. The first six match
/// the classic starting layout; extra slots continue the octave-ish spread.
fn band_spot(i: usize) -> (f32, FilterKind) {
    const LAYOUT: [(f32, FilterKind); 6] = [
        (80.0, FilterKind::HighPass),
        (250.0, FilterKind::Peak),
        (1000.0, FilterKind::Peak),
        (2500.0, FilterKind::Peak),
        (6000.0, FilterKind::Peak),
        (12000.0, FilterKind::HighShelf),
    ];
    const EXTRA: [(f32, FilterKind); 4] = [
        (400.0, FilterKind::Peak),
        (3000.0, FilterKind::Peak),
        (8000.0, FilterKind::Peak),
        (16000.0, FilterKind::HighShelf),
    ];
    if i < LAYOUT.len() {
        LAYOUT[i]
    } else {
        EXTRA[(i - LAYOUT.len()) % EXTRA.len()]
    }
}

/// Console (training) mode default: a fixed 5-band channel-EQ strip. Low
/// shelf, three peaking bells, high shelf, spread across the spectrum just
/// like the EQ section of a real desk.
fn console_layout(i: usize) -> (f32, FilterKind) {
    const LAYOUT: [(f32, FilterKind); CONSOLE_BANDS] = [
        (80.0, FilterKind::LowShelf),
        (400.0, FilterKind::Peak),
        (1000.0, FilterKind::Peak),
        (3000.0, FilterKind::Peak),
        (12000.0, FilterKind::HighShelf),
    ];
    LAYOUT[i]
}

/// Default starting layout: a spread across the audible spectrum, like a
/// 6-band graphic strip you'd find on a console. Returns one {BandParams}
/// per slot up to MAX_BANDS; beyond the first NUM_BANDS they start disabled.
pub fn default_bands() -> Vec<BandParams> {
    (0..MAX_BANDS)
        .map(|i| {
            let (f, k) = band_spot(i);
            BandParams::new(f, 0.0, 1.0, k)
        })
        .collect()
}

pub struct SharedState {
    pub bands: Vec<BandParams>,
    pub band_count: AtomicU8,    // number of active bands (<= MAX_BANDS)
    pub sample_rate: AtomicU32,    // f32 bits, updated once the stream opens
    pub input_peak: AtomicU32,     // f32 bits, for a level meter
    pub output_peak: AtomicU32,    // f32 bits
    pub master_gain: AtomicU32,    // f32 bits, overall volume (1.0 = unity)
    pub spectrum: [AtomicU32; NUM_BARS], // f32 bits, 0..=1 per visualizer bar
    pub play_ms: AtomicU64,        // file playback position in milliseconds
    pub total_ms: AtomicU64,       // file duration in ms (0 = unknown)
    pub seek_ms: AtomicU64,        // pending seek target in ms; u64::MAX = none
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            bands: default_bands(),
            band_count: AtomicU8::new(NUM_BANDS as u8),
            sample_rate: AtomicU32::new(48000f32.to_bits()),
            input_peak: AtomicU32::new(0),
            output_peak: AtomicU32::new(0),
            master_gain: AtomicU32::new(1.0f32.to_bits()),
            spectrum: std::array::from_fn(|_| AtomicU32::new(0)),
            play_ms: AtomicU64::new(0),
            total_ms: AtomicU64::new(0),
            seek_ms: AtomicU64::new(u64::MAX),
        })
    }

    pub fn sample_rate(&self) -> f32 {
        f32::from_bits(self.sample_rate.load(Ordering::Relaxed))
    }

    /// How many bands are currently active in the strip.
    pub fn num_bands(&self) -> usize {
        self.band_count.load(Ordering::Relaxed) as usize
    }

    /// Add one more band (at most MAX_BANDS total). Its slot was preallocated,
    /// so this is safe for the audio thread even while a stream is live.
    pub fn add_band(&self) {
        let cur = self.band_count.load(Ordering::Relaxed) as usize;
        if cur < MAX_BANDS {
            let (f, k) = band_spot(cur);
            let band = &self.bands[cur];
            band.set_freq(f);
            band.set_gain(0.0);
            band.set_q(1.0);
            band.set_kind(k);
            band.enabled.store(true, Ordering::Relaxed);
            self.band_count
                .store(cur as u8 + 1, Ordering::Relaxed);
        }
    }

    /// Drop the last active band (minimum 1).
    pub fn remove_band(&self) {
        let cur = self.band_count.load(Ordering::Relaxed) as usize;
        if cur > 1 {
            self.band_count
                .store(cur as u8 - 1, Ordering::Relaxed);
        }
    }

    /// Restore a previously persisted band count, enabling exactly the first
    /// `n` slots so the strip matches the saved strip size.
    pub fn set_band_count(&self, n: usize) {
        let n = n.clamp(1, MAX_BANDS);
        self.band_count.store(n as u8, Ordering::Relaxed);
        for (i, band) in self.bands.iter().enumerate() {
            band.enabled.store(i < n, Ordering::Relaxed);
        }
    }

    /// After adding or removing a band, reposition every active band so they
    /// are evenly spaced across the spectrum in log-space. Each band keeps its
    /// own gain/Q/kind; only the frequency moves, and the existing relative
    /// ordering (and approximate log-space ratios) are preserved.
    pub fn evenly_spread_bands(&self) {
        let n = self.num_bands();
        if n == 0 {
            return;
        }
        let min_log = 20.0_f32.ln();
        let max_log = 20_000.0_f32.ln();
        // Sort active band indices by their current frequency.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|a, b| {
            self.bands[*a]
                .freq()
                .partial_cmp(&self.bands[*b].freq())
                .unwrap()
        });
        // Place each band in its slot, centered within equally-spaced divisions.
        for (rank, &idx) in order.iter().enumerate() {
            let t = (rank as f32 + 0.5) / n as f32;
            let freq = (min_log + (max_log - min_log) * t).exp();
            self.bands[idx].set_freq(freq);
        }
    }

    /// Request the file player to jump to `ms` from the start.
    pub fn seek_to_ms(&self, ms: u64) {
        self.seek_ms.store(ms, Ordering::Relaxed);
    }

    /// Reset file playback position/duration back to the start.
    pub fn reset_transport(&self) {
        self.play_ms.store(0, Ordering::Relaxed);
        self.total_ms.store(0, Ordering::Relaxed);
        self.seek_ms.store(u64::MAX, Ordering::Relaxed);
    }

    /// Restore every band to its default spot (frequency, 0 dB, Q, type) and
    /// shrink back to the classic 6-band starting layout.
    pub fn flatten(&self) {
        self.band_count.store(NUM_BANDS as u8, Ordering::Relaxed);
        for (band, def) in self.bands.iter().zip(default_bands().iter()) {
            band.set_freq(def.freq());
            band.set_gain(def.gain());
            band.set_q(def.q());
            band.set_kind(def.kind());
            band.enabled.store(true, Ordering::Relaxed);
        }
        for band in self.bands.iter().skip(NUM_BANDS) {
            band.enabled.store(false, Ordering::Relaxed);
        }
    }

    /// Console (training) mode: a fixed 5-band strip with the classic channel
    /// EQ shapes, every band at 0 dB / Q 1. Enables the console layout, or
    /// returns the strip to the classic 6-band starting layout.
    pub fn apply_console_mode(&self, on: bool) {
        if on {
            self.band_count.store(CONSOLE_BANDS as u8, Ordering::Relaxed);
            for i in 0..CONSOLE_BANDS {
                let (f, k) = console_layout(i);
                let band = &self.bands[i];
                band.set_freq(f);
                band.set_gain(0.0);
                band.set_q(1.0);
                band.set_kind(k);
                band.enabled.store(true, Ordering::Relaxed);
            }
            for band in self.bands.iter().skip(CONSOLE_BANDS) {
                band.enabled.store(false, Ordering::Relaxed);
            }
        } else {
            self.flatten();
        }
    }
}

/// Simple radix-2 FFT spectrum analyzer. Accumulates windowed samples in the
/// audio callback and, whenever a full frame has been collected, pushes the
/// per-bar energy (0..=1, normalised against a slowly-releasing peak) into
/// `SharedState::spectrum` so the UI can draw bouncing bars.
struct SpectrumAnalyzer {
    frame: Vec<f32>,
    pos: usize,
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    cos_tab: Vec<f32>,
    sin_tab: Vec<f32>,
    rev: Vec<usize>,
    levels: [f32; NUM_BARS],
    peak_lin: f32,
}

impl SpectrumAnalyzer {
    fn new() -> Self {
        let n = FFT_SIZE;
        let mut window = vec![0.0f32; n];
        for (i, w) in window.iter_mut().enumerate() {
            *w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n as f32 - 1.0)).cos();
        }
        let bits = (n as f32).log2() as u32;
        let rev = (0..n)
            .map(|i| (0..bits).fold(0usize, |acc, b| acc | (((i >> b) & 1) << (bits - 1 - b))))
            .collect();
        let mut cos_tab = Vec::with_capacity(n / 2);
        let mut sin_tab = Vec::with_capacity(n / 2);
        for k in 0..n / 2 {
            let a = -2.0 * PI * k as f32 / n as f32;
            cos_tab.push(a.cos());
            sin_tab.push(a.sin());
        }
        Self {
            frame: vec![0.0f32; n],
            pos: 0,
            window,
            re: vec![0.0f32; n],
            im: vec![0.0f32; n],
            cos_tab,
            sin_tab,
            rev,
            levels: [0.0f32; NUM_BARS],
            peak_lin: 0.0,
        }
    }

    fn push(&mut self, sample: f32, sample_rate: f32, spectrum: &[AtomicU32; NUM_BARS]) {
        self.frame[self.pos] = sample;
        self.pos += 1;
        if self.pos == FFT_SIZE {
            self.pos = 0;
            self.analyze(sample_rate, spectrum);
        }
    }

    /// Clear the pending frame so a seek jump doesn't smear into the next FFT.
    fn reset(&mut self) {
        self.pos = 0;
        self.frame.fill(0.0);
    }

    fn analyze(&mut self, sample_rate: f32, spectrum: &[AtomicU32; NUM_BARS]) {
        let n = FFT_SIZE;
        for (i, w) in self.window.iter().enumerate() {
            self.re[i] = self.frame[i] * w;
            self.im[i] = 0.0;
        }
        self.fft();

        let sr = sample_rate.max(1.0);
        let f0 = 30.0;
        let f1 = (sr * 0.45).min(20_000.0).max(f0 * 1.01);
        let ratio = (f1 / f0).powf(1.0 / NUM_BARS as f32);

        let mut max_m = 0.0f32;
        for b in 0..NUM_BARS {
            let fl = f0 * ratio.powf(b as f32);
            let fh = fl * ratio;
            let k0 = ((fl * n as f32 / sr) as usize).max(1);
            let k1 = ((fh * n as f32 / sr) as usize).min(n / 2);
            if k1 > k0 {
                let count = (k1 - k0) as f32;
                let mut sum = 0.0f32;
                for k in k0..k1 {
                    let r = self.re[k];
                    let im = self.im[k];
                    sum += r * r + im * im;
                }
                let rms = (sum / count).sqrt();
                self.levels[b] = rms;
                if rms > max_m {
                    max_m = rms;
                }
            } else {
                self.levels[b] = 0.0;
            }
        }

        if max_m > self.peak_lin {
            self.peak_lin = max_m;
        } else {
            self.peak_lin *= 0.99;
            if self.peak_lin < 1e-6 {
                self.peak_lin = 0.0;
            }
        }

        let peak = self.peak_lin;
        for b in 0..NUM_BARS {
            let level = if peak > 1e-9 {
                ((self.levels[b] / peak).sqrt() * 1.2).min(1.0)
            } else {
                0.0
            };
            spectrum[b].store(level.to_bits(), Ordering::Relaxed);
        }
    }

    fn fft(&mut self) {
        let n = FFT_SIZE;
        for (i, &j) in self.rev.iter().enumerate() {
            if i < j {
                self.re.swap(i, j);
                self.im.swap(i, j);
            }
        }
        let mut len = 2usize;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..half {
                    let w = self.cos_tab[k * step];
                    let wi = self.sin_tab[k * step];
                    let a = start + k;
                    let b = a + half;
                    let x = self.re[b];
                    let y = self.im[b];
                    let tr = w * x - wi * y;
                    let ti = w * y + wi * x;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            len <<= 1;
        }
    }
}

/// Wraps a rodio source (file playback) and runs every sample through the same
/// biquad band chain as the live path, so the EQ actually changes what you
/// hear. It also taps the processed audio into the spectrum analyzer and the
/// output meter.
struct EqSource<S> {
    inner: S,
    state: Arc<SharedState>,
    analyzer: SpectrumAnalyzer,
    src_rate: f32,
    channels: usize,
    frame_pos: usize,
    frame_n: u64,          // mono samples consumed so far this file
    last_report: u64,      // last sample count pushed to play_ms
    chains: Vec<Vec<Biquad>>,
    coeff_state: Vec<(u32, u32, u32, u8)>,
    peak_acc: f32,
    peak_counter: u32,
}

impl<S: Iterator<Item = f32> + rodio::Source> EqSource<S> {
    fn new(inner: S, state: Arc<SharedState>) -> Self {
        let src_rate = inner.sample_rate().get() as f32;
        let channels = inner.channels().get() as usize;
        Self {
            inner,
            state,
            analyzer: SpectrumAnalyzer::new(),
            src_rate,
            channels,
            frame_pos: 0,
            frame_n: 0,
            last_report: 0,
            chains: vec![vec![Biquad::default(); MAX_BANDS]; channels],
            coeff_state: vec![(u32::MAX, u32::MAX, u32::MAX, u8::MAX); MAX_BANDS],
            peak_acc: 0.0,
            peak_counter: 0,
        }
    }

    /// Some decoders can change rate/channel layout mid-stream. Rebuild the
    /// filter chains whenever that happens.
    fn ensure_config(&mut self) {
        let ch = self.inner.channels().get() as usize;
        let rate = self.inner.sample_rate().get() as f32;
        if ch != self.channels || (rate - self.src_rate).abs() > 0.5 {
            self.channels = ch;
            self.src_rate = rate;
            self.chains = vec![vec![Biquad::default(); MAX_BANDS]; ch];
            self.frame_pos = 0;
            self.coeff_state = vec![(u32::MAX, u32::MAX, u32::MAX, u8::MAX); MAX_BANDS];
        }
        self.state.sample_rate.store(self.src_rate.to_bits(), Ordering::Relaxed);
    }

    /// Consume inner samples until this source has advanced to `target`
    /// mono-samples from the start (i.e. a user seek jump).
    fn skip_to(&mut self, target_samples: u64) {
        while self.frame_n < target_samples {
            if self.inner.next().is_none() {
                break;
            }
            self.frame_n += 1;
        }
        // Clear filter memory + FFT frame so the jump doesn't click.
        for chain in self.chains.iter_mut() {
            for biquad in chain.iter_mut() {
                biquad.reset();
            }
        }
        self.analyzer.reset();
    }
}

impl<S: Iterator<Item = f32> + rodio::Source> Iterator for EqSource<S> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // Service a pending seek before pulling anything else.
        let seek = self.state.seek_ms.load(Ordering::Relaxed);
        if seek != u64::MAX {
            self.state.seek_ms.store(u64::MAX, Ordering::Relaxed);
            self.ensure_config();
            let target = seek * self.src_rate as u64 / 1000;
            self.skip_to(target);
            self.state.play_ms.store(seek, Ordering::Relaxed);
        }

        let sample = self.inner.next()?;
        self.ensure_config();

        let nb = self.state.num_bands();
        let mut x = sample;
        {
            let chain = &mut self.chains[self.frame_pos % self.channels];
            for ((band, biquad), cached) in self
                .state
                .bands
                .iter()
                .take(nb)
                .zip(chain.iter_mut())
                .zip(self.coeff_state.iter_mut())
            {
                if band.enabled() {
                    let key = (
                        band.freq_hz.load(Ordering::Relaxed),
                        band.gain_db.load(Ordering::Relaxed),
                        band.q.load(Ordering::Relaxed),
                        band.kind.load(Ordering::Relaxed),
                    );
                    if *cached != key {
                        biquad.reset();
                        biquad.set_coeffs(
                            band.kind(),
                            self.src_rate,
                            band.freq(),
                            band.gain(),
                            band.q(),
                        );
                        *cached = key;
                    }
                    x = biquad.process(x);
                }
            }
        }

        let master = f32::from_bits(self.state.master_gain.load(Ordering::Relaxed));
        x *= master;

        self.frame_n += 1;
        if self.frame_n - self.last_report >= 4096 {
            self.last_report = self.frame_n;
            let ms = self.frame_n * 1000 / (self.src_rate as u64);
            let total = self.state.total_ms.load(Ordering::Relaxed);
            let ms = if total > 0 { ms.min(total) } else { ms };
            self.state.play_ms.store(ms, Ordering::Relaxed);
        }

        self.analyzer.push(x, self.src_rate, &self.state.spectrum);

        if x.abs() > self.peak_acc {
            self.peak_acc = x.abs();
        }
        self.peak_counter += 1;
        if self.peak_counter >= 200 {
            self.peak_counter = 0;
            self.state
                .output_peak
                .store(self.peak_acc.to_bits(), Ordering::Relaxed);
            self.peak_acc = 0.0;
        }

        self.frame_pos += 1;
        Some(x)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<S: Iterator<Item = f32> + rodio::Source> rodio::Source for EqSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

/// Owns the two cpal streams. Dropping this stops audio.
pub struct AudioEngine {
    _input_stream: Option<Stream>,
    _output_stream: Option<Stream>,
    _player: Option<Player>,
    _device_sink: Option<MixerDeviceSink>,
}

impl Clone for AudioEngine {
    fn clone(&self) -> Self {
        // Cloning the engine just duplicates the handles; streams are not cloneable, so we can't derive Clone.
        // Instead, we will not clone the engine; callers should store Option<AudioEngine> directly.
        panic!("AudioEngine cannot be cloned");
    }
}

impl AudioEngine {
    /// True when this engine plays a file (has a rodio player + progress).
    pub fn is_file(&self) -> bool {
        self._player.is_some()
    }

    /// Pauses or resumes whatever the engine is producing (file player and/or
    /// capture streams), so the audio thread keeps running but stays silent.
    pub fn set_paused(&self, paused: bool) {
        if let Some(player) = &self._player {
            if paused {
                player.pause();
            } else {
                player.play();
            }
        }
        if let Some(out) = &self._output_stream {
            if paused {
                let _ = out.pause();
            } else {
                let _ = out.play();
            }
        }
        if let Some(inp) = &self._input_stream {
            if paused {
                let _ = inp.pause();
            } else {
                let _ = inp.play();
            }
        }
    }
}

/// Decode a file once just to count samples. Used when the decoder can't
/// report a length up front (common for mp3/ogg/flac streams).
fn measure_total_ms(path: &str) -> u64 {
    let Ok(file) = File::open(path) else { return 0 };
    let Ok(dec) = rodio::Decoder::new(BufReader::new(file)) else { return 0 };
    let sr = dec.sample_rate().get() as u64;
    let channels = dec.channels().get() as u64;
    if sr == 0 || channels == 0 {
        return 0;
    }
    let cap = sr * 4 * 3600; // never scan beyond ~4 hours
    let mut n: u64 = 0;
    for _ in dec {
        n += 1;
        if n >= cap {
            break;
        }
    }
    (n / channels) * 1000 / sr
}

impl AudioEngine {
    /// Loads a file and starts playing it through the EQ.
    pub fn try_new_file(state: Arc<SharedState>, file_path: &str) -> Result<Self, String> {
        // Set up rodio output device sink
        let device_sink = DeviceSinkBuilder::open_default_sink()
            .map_err(|e| format!("failed to get default output stream: {e}"))?;

        let player = Player::connect_new(&device_sink.mixer());

        // Decode the file, then wrap it in a source that processes every sample
        // through the EQ band chain before it reaches the speaker.
        let file = File::open(file_path).map_err(|e| format!("failed to open file: {e}"))?;
        let source = rodio::Decoder::new(BufReader::new(file))
            .map_err(|e| format!("failed to decode file: {e}"))?;

        // Length may be reported directly (wav) or need a full sample count
        // (mp3/ogg/flac). Either way we drive the transport ourselves from
        // then on, so the scrubber always has a real duration to span.
        let total_ms = source
            .total_duration()
            .map(|d| d.as_millis() as u64)
            .filter(|m| *m > 0)
            .unwrap_or_else(|| measure_total_ms(file_path));
        state.play_ms.store(0, Ordering::Relaxed);
        state.total_ms.store(total_ms, Ordering::Relaxed);
        state.seek_ms.store(u64::MAX, Ordering::Relaxed);

        let eq_source = EqSource::new(source, state.clone());
        player.append(eq_source);
        player.play();

        Ok(AudioEngine {
            _input_stream: None,
            _output_stream: None,
            _player: Some(player),
            _device_sink: Some(device_sink),
        })
    }

    /// Starts capturing from a specific output device (by name) and optional input device.
    pub fn try_new_named(state: Arc<SharedState>, output_name: &str, input_name: Option<&str>) -> Result<Self, String> {
        println!("Attempting to create engine with Output: {} and Input: {:?}", output_name, input_name);
        let host = cpal::default_host();
        let output_device = host
            .output_devices()
            .map_err(|e| format!("enumerating output devices: {e}"))?
            .find(|d| d.name().map(|n| n == output_name).unwrap_or(false))
            .ok_or_else(|| format!("specified output device not found: {}", output_name))?;
        let input_device_opt = if let Some(name) = input_name {
            host.input_devices()
                .map_err(|e| format!("enumerating input devices: {e}"))?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| {
                    println!("Found input device: {}", name);
                    d
                })
                .ok_or_else(|| format!("specified input device not found: {}", name))
                .ok()
        } else {
            None
        };
        start_streams_with_devices(state, input_device_opt, output_device)
            .map(|(input, output)| AudioEngine { 
                _input_stream: input, 
                _output_stream: Some(output), 
                _player: None,
                _device_sink: None,
            })
    }
}

fn start_streams_with_devices(state: Arc<SharedState>, input_device_opt: Option<cpal::Device>, output_device: cpal::Device) -> Result<(Option<Stream>, Stream), String> {
    // Output config
    let output_config = output_device
        .default_output_config()
        .map_err(|e| format!("output config: {e}"))?;
    
    println!("Output Config: {:?} (Rate: {}, Channels: {})", 
             output_config.sample_format(), 
             output_config.sample_rate().0, 
             output_config.channels());

    // Input config (optional)
    let input_config_opt = if let Some(ref dev) = input_device_opt {
        let cfg = dev
            .default_input_config()
            .map_err(|e| format!("input config: {e}"))?;
        println!("Input Config: {:?} (Rate: {}, Channels: {})", 
                 cfg.sample_format(), 
                 cfg.sample_rate().0, 
                 cfg.channels());
        Some(cfg)
    } else {
        None
    };
    // Use output sample rate as master
    let sample_rate = output_config.sample_rate().0 as f32;
    state.sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
    let ring = HeapRb::<f32>::new(sample_rate as usize * 2);
    let (producer, consumer) = ring.split();
    let input_state = state.clone();
    // Build optional input stream (any common sample format).
    let input_stream = if let (Some(input_device), Some(input_cfg)) = (input_device_opt, input_config_opt) {
        match input_cfg.sample_format() {
            SampleFormat::F32 => Some(build_input_stream::<f32>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            SampleFormat::I16 => Some(build_input_stream::<i16>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            SampleFormat::U16 => Some(build_input_stream::<u16>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            SampleFormat::I32 => Some(build_input_stream::<i32>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            SampleFormat::U32 => Some(build_input_stream::<u32>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            SampleFormat::F64 => Some(build_input_stream::<f64>(&input_device, &input_cfg.into(), producer, input_state.clone())?),
            other => return Err(format!("unsupported input sample format: {other:?}")),
        }
    } else {
        None
    };
    // Output stream (full EQ chain for any common sample format).
    let out_channels = output_config.channels() as usize;
    let output_stream = match output_config.sample_format() {
        SampleFormat::F32 => build_output_stream::<f32>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        SampleFormat::I16 => build_output_stream::<i16>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        SampleFormat::U16 => build_output_stream::<u16>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        SampleFormat::I32 => build_output_stream::<i32>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        SampleFormat::U32 => build_output_stream::<u32>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        SampleFormat::F64 => build_output_stream::<f64>(&output_device, &output_config.into(), out_channels, consumer, input_state)?,
        other => return Err(format!("unsupported output sample format: {other:?}")),
    };
    // Start streams
    if let Some(ref s) = input_stream {
        s.play().map_err(|e| format!("play input: {e}"))?;
    }
    output_stream.play().map_err(|e| format!("play output: {e}"))?;
    Ok((input_stream, output_stream))
}

fn stream_error(e: cpal::StreamError) {
    eprintln!("stream error: {e}");
}

/// Builds an input capture stream for any common sample format, converting
/// samples to f32 (mono-summed into the shared ring buffer) before pushing.
fn build_input_stream<F>(
    input_device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: ringbuf::Producer<f32, Arc<HeapRb<f32>>>,
    state: Arc<SharedState>,
) -> Result<Stream, String>
where
    F: cpal::SizedSample,
    f32: FromSample<F>,
{
    input_device
        .build_input_stream(
            config,
            move |data: &[F], _| {
                let mut peak = 0f32;
                for &s in data {
                    let sample = f32::from_sample_(s);
                    peak = peak.max(sample.abs());
                    let _ = producer.push(sample);
                }
                state.input_peak.store(peak.to_bits(), Ordering::Relaxed);
            },
            stream_error,
            None,
        )
        .map_err(|e| format!("build input stream: {e}"))
}

/// Builds the output stream for any common sample format, running the full
/// EQ chain (per-channel biquad chains from the shared band state) before
/// converting back to the device format.
fn build_output_stream<F>(
    output_device: &cpal::Device,
    config: &cpal::StreamConfig,
    out_channels: usize,
    mut consumer: ringbuf::Consumer<f32, Arc<HeapRb<f32>>>,
    state: Arc<SharedState>,
) -> Result<Stream, String>
where
    F: cpal::SizedSample + FromSample<f32>,
{
    let mut chains: Vec<[Biquad; MAX_BANDS]> = vec![[Biquad::default(); MAX_BANDS]; out_channels];
    let mut analyzer = SpectrumAnalyzer::new();
    output_device
        .build_output_stream(
            config,
            move |data: &mut [F], _| {
                let sr = state.sample_rate();
                let nb = state.num_bands();
                for chain in chains.iter_mut() {
                    for (band, biquad) in state.bands.iter().take(nb).zip(chain.iter_mut()) {
                        if band.enabled() {
                            biquad.set_coeffs(band.kind(), sr, band.freq(), band.gain(), band.q());
                        }
                    }
                }
                let master = f32::from_bits(state.master_gain.load(Ordering::Relaxed));
                let mut peak = 0f32;
                for frame in data.chunks_mut(out_channels) {
                    let sample = consumer.pop().unwrap_or(0.0);
                    let mut last = 0.0;
                    for (ch_idx, out) in frame.iter_mut().enumerate() {
                        let mut x = sample;
                        {
                            let chain_idx = ch_idx.min(chains.len().saturating_sub(1));
                            let chain = &mut chains[chain_idx];
                            for (band, biquad) in state.bands.iter().take(nb).zip(chain.iter_mut()) {
                                if band.enabled() {
                                    x = biquad.process(x);
                                }
                            }
                        }
                        x *= master;
                        peak = peak.max(x.abs());
                        *out = F::from_sample_(x);
                        last = x;
                    }
                    analyzer.push(last, sr, &state.spectrum);
                }
                state.output_peak.store(peak.to_bits(), Ordering::Relaxed);
            },
            stream_error,
            None,
        )
        .map_err(|e| format!("build output stream: {e}"))
}
