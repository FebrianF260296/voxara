use crate::dsp::{DspConfig, RealtimeDspChain, StatefulResampler};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::Duration;

/// Single-producer single-consumer lock-free ring buffer storing `f32` samples as `AtomicU32` bits.
/// Zero mutexes and zero heap allocations in push/pop.
pub struct LockFreeRingBuffer {
    buf: Box<[AtomicU32]>,
    mask: usize,
    read_idx: AtomicUsize,
    write_idx: AtomicUsize,
}

impl LockFreeRingBuffer {
    pub fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(64).next_power_of_two();
        let mut vec = Vec::with_capacity(cap);
        for _ in 0..cap {
            vec.push(AtomicU32::new(0));
        }
        Self {
            buf: vec.into_boxed_slice(),
            mask: cap - 1,
            read_idx: AtomicUsize::new(0),
            write_idx: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    #[inline]
    pub fn len(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    /// Consumer-side clear: advances the read index to the write index.
    /// Must only be called from the consumer thread (or while no consumer is running).
    #[inline]
    pub fn clear(&self) {
        let w = self.write_idx.load(Ordering::Acquire);
        self.read_idx.store(w, Ordering::Release);
    }

    #[inline]
    pub fn push(&self, sample: f32) -> bool {
        let w = self.write_idx.load(Ordering::Relaxed);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= self.capacity() {
            return false;
        }
        self.buf[w & self.mask].store(sample.to_bits(), Ordering::Relaxed);
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        true
    }

    /// Pushes as many samples as fit and returns how many were dropped (0 if everything fit).
    #[inline]
    pub fn push_slice(&self, samples: &[f32]) -> usize {
        let mut pushed = 0usize;
        for &s in samples {
            if !self.push(s) {
                break;
            }
            pushed += 1;
        }
        samples.len() - pushed
    }

    #[inline]
    pub fn pop(&self) -> Option<f32> {
        let r = self.read_idx.load(Ordering::Relaxed);
        let w = self.write_idx.load(Ordering::Acquire);
        if r == w {
            return None;
        }
        let bits = self.buf[r & self.mask].load(Ordering::Relaxed);
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        Some(f32::from_bits(bits))
    }

    pub fn drain_into(&self, dst: &mut Vec<f32>) {
        while let Some(s) = self.pop() {
            dst.push(s);
        }
    }

    /// Drops `count` oldest samples from consumer side smoothly if drift builds up.
    pub fn discard_oldest(&self, count: usize) {
        let r = self.read_idx.load(Ordering::Relaxed);
        let w = self.write_idx.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r);
        let to_skip = count.min(avail);
        self.read_idx.store(r.wrapping_add(to_skip), Ordering::Release);
    }
}

/// Lock-free atomic bridge between GUI sliders/toggles and real-time `cpal` audio threads.
pub struct AtomicAudioParams {
    denasal_strength: AtomicU32,
    nasal_freq_hz: AtomicU32,
    oral_formant_boost: AtomicU32,
    consonant_boost: AtomicU32,
    deesser_strength: AtomicU32,
    air_leak_gate: AtomicU32,
    breath_compensator: AtomicU32,
    output_gain_db: AtomicU32,

    pub live_passthrough: AtomicBool,
    pub is_recording: AtomicBool,
    pub input_level: AtomicU32,
    pub output_level: AtomicU32,
    pub measured_latency_ms: AtomicU32,
    pub in_sample_rate: AtomicU32,
    pub out_sample_rate: AtomicU32,

    /// True while a playback clip (including its short fade-out) is audible.
    pub is_playing: AtomicBool,
    /// True when `measured_latency_ms` comes from real WASAPI timestamps instead of an estimate.
    pub latency_measured: AtomicBool,
    /// Set by the input callback when the record ring dropped samples while recording.
    pub record_overflow: AtomicBool,
    /// Latest input latency in ms (callback time minus capture time); negative = unavailable.
    pub input_latency_ms: AtomicU32,
}

impl AtomicAudioParams {
    pub fn new(cfg: DspConfig) -> Self {
        let params = Self {
            denasal_strength: AtomicU32::new(0),
            nasal_freq_hz: AtomicU32::new(0),
            oral_formant_boost: AtomicU32::new(0),
            consonant_boost: AtomicU32::new(0),
            deesser_strength: AtomicU32::new(0),
            air_leak_gate: AtomicU32::new(0),
            breath_compensator: AtomicU32::new(0),
            output_gain_db: AtomicU32::new(0),
            // OFF by default on startup to prevent speaker-mic feedback howl
            live_passthrough: AtomicBool::new(false),
            is_recording: AtomicBool::new(false),
            input_level: AtomicU32::new(0.0_f32.to_bits()),
            output_level: AtomicU32::new(0.0_f32.to_bits()),
            measured_latency_ms: AtomicU32::new(0.0_f32.to_bits()),
            in_sample_rate: AtomicU32::new(48000),
            out_sample_rate: AtomicU32::new(48000),
            is_playing: AtomicBool::new(false),
            latency_measured: AtomicBool::new(false),
            record_overflow: AtomicBool::new(false),
            input_latency_ms: AtomicU32::new((-1.0_f32).to_bits()),
        };
        params.store_dsp_config(cfg);
        params
    }

    pub fn store_dsp_config(&self, cfg: DspConfig) {
        self.denasal_strength
            .store(cfg.denasal_strength.to_bits(), Ordering::Relaxed);
        self.nasal_freq_hz
            .store(cfg.nasal_freq_hz.to_bits(), Ordering::Relaxed);
        self.oral_formant_boost
            .store(cfg.oral_formant_boost.to_bits(), Ordering::Relaxed);
        self.consonant_boost
            .store(cfg.consonant_boost.to_bits(), Ordering::Relaxed);
        self.deesser_strength
            .store(cfg.deesser_strength.to_bits(), Ordering::Relaxed);
        self.air_leak_gate
            .store(cfg.air_leak_gate.to_bits(), Ordering::Relaxed);
        self.breath_compensator
            .store(cfg.breath_compensator.to_bits(), Ordering::Relaxed);
        self.output_gain_db
            .store(cfg.output_gain_db.to_bits(), Ordering::Relaxed);
    }

    pub fn load_dsp_config(&self) -> DspConfig {
        DspConfig {
            denasal_strength: f32::from_bits(self.denasal_strength.load(Ordering::Relaxed)),
            nasal_freq_hz: f32::from_bits(self.nasal_freq_hz.load(Ordering::Relaxed)),
            oral_formant_boost: f32::from_bits(self.oral_formant_boost.load(Ordering::Relaxed)),
            consonant_boost: f32::from_bits(self.consonant_boost.load(Ordering::Relaxed)),
            deesser_strength: f32::from_bits(self.deesser_strength.load(Ordering::Relaxed)),
            air_leak_gate: f32::from_bits(self.air_leak_gate.load(Ordering::Relaxed)),
            breath_compensator: f32::from_bits(self.breath_compensator.load(Ordering::Relaxed)),
            output_gain_db: f32::from_bits(self.output_gain_db.load(Ordering::Relaxed)),
        }
    }

    #[inline]
    pub fn store_f32(slot: &AtomicU32, val: f32) {
        slot.store(val.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn load_f32(slot: &AtomicU32) -> f32 {
        f32::from_bits(slot.load(Ordering::Relaxed))
    }
}

/// Command sent from the UI thread to the output callback.
pub enum PlaybackCmd {
    Play(Arc<[f32]>),
    Stop,
}

/// Content of the mailbox: one pending command plus one clip handed back by the audio thread
/// so that its (potentially large) memory is freed on the UI side instead of in the callback.
#[derive(Default)]
pub struct MailboxSlot {
    pub pending: Option<PlaybackCmd>,
    pub retired: Option<Arc<[f32]>>,
}

/// Single-slot mailbox between the UI (producer) and the output callback (consumer).
/// The UI side may block on the mutex; the audio side only ever uses `try_lock`.
#[derive(Default)]
pub struct PlaybackMailbox {
    pub slot: Mutex<MailboxSlot>,
}

impl PlaybackMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// UI-side: post a command, replacing any command the audio thread has not picked up yet.
    pub fn post(&self, cmd: PlaybackCmd) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        // Free the clip returned by the audio thread (and any superseded pending clip) here.
        slot.retired = None;
        slot.pending = Some(cmd);
    }

    /// Drops any pending command and retired clip (used when the engine stops).
    pub fn reset(&self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.pending = None;
        slot.retired = None;
    }
}

/// Plain output mixing state (live passthrough + playback clip + ducking), independent of cpal
/// so it can be unit tested. Used from inside the output callback: lock-free except for one
/// non-blocking `try_lock` on the playback mailbox per `fill`.
pub struct OutputMixer {
    params: Arc<AtomicAudioParams>,
    pass_ring: Arc<LockFreeRingBuffer>,
    mailbox: Arc<PlaybackMailbox>,

    out_sr: u32,
    prefill_samples: usize,
    target_samples: usize,
    drift_high_samples: usize,
    drift_low_samples: usize,
    hard_max_samples: usize,
    fade_len: usize,
    duck_coef: f32,

    prefilled: bool,
    last_live: f32,
    duck_gain: f32,
    smoothed_out_lvl: f32,

    current: Option<Arc<[f32]>>,
    pos: usize,
    /// Remaining samples of the fade-out of `current` (0 = not fading).
    fade_remaining: usize,
    /// Command to apply once the fade-out finishes.
    after_fade: Option<PlaybackCmd>,
    /// Old clip waiting to be handed back to the UI through the mailbox.
    to_retire: Option<Arc<[f32]>>,

    /// Output latency (playback minus callback timestamp) for the next `fill`, if known.
    pub output_latency_ms: Option<f32>,

    scratch: Vec<f32>,
}

const DUCK_TARGET_GAIN: f32 = 0.12;

impl OutputMixer {
    pub fn new(
        params: Arc<AtomicAudioParams>,
        pass_ring: Arc<LockFreeRingBuffer>,
        mailbox: Arc<PlaybackMailbox>,
        out_sr: u32,
    ) -> Self {
        let sr = out_sr.max(1) as usize;
        let prefill = (sr * 12) / 1000;
        let target = prefill * 2;
        let duck_tc_samples = (out_sr.max(1) as f32) * 0.015;
        Self {
            params,
            pass_ring,
            mailbox,
            out_sr: out_sr.max(1),
            prefill_samples: prefill,
            target_samples: target,
            drift_high_samples: target + (sr * 20) / 1000,
            drift_low_samples: target.saturating_sub((sr * 8) / 1000),
            hard_max_samples: (sr * 150) / 1000,
            fade_len: ((sr * 5) / 1000).max(1),
            duck_coef: 1.0 - (-1.0 / duck_tc_samples).exp(),
            prefilled: false,
            last_live: 0.0,
            duck_gain: 1.0,
            smoothed_out_lvl: 0.0,
            current: None,
            pos: 0,
            fade_remaining: 0,
            after_fade: None,
            to_retire: None,
            output_latency_ms: None,
            scratch: Vec::with_capacity(4096),
        }
    }

    #[inline]
    fn clip_playing(&self) -> bool {
        self.current.as_ref().map_or(false, |c| self.pos < c.len())
    }

    /// Single non-blocking mailbox poll per callback.
    fn poll_mailbox(&mut self) {
        let cmd = match self.mailbox.slot.try_lock() {
            Ok(mut slot) => {
                if slot.retired.is_none() {
                    if let Some(old) = self.to_retire.take() {
                        slot.retired = Some(old);
                    }
                }
                slot.pending.take()
            }
            Err(_) => None,
        };
        if let Some(cmd) = cmd {
            self.apply_cmd(cmd);
        }
    }

    fn retire(&mut self, clip: Arc<[f32]>) {
        // If an older clip is still waiting for hand-back, it is dropped here (rare fallback).
        self.to_retire = Some(clip);
    }

    fn apply_cmd(&mut self, cmd: PlaybackCmd) {
        if self.clip_playing() {
            // Fade the audible clip out first, then switch.
            if self.fade_remaining == 0 {
                self.fade_remaining = self.fade_len;
            }
            if let Some(PlaybackCmd::Play(superseded)) = self.after_fade.replace(cmd) {
                self.retire(superseded);
            }
        } else {
            self.switch_to(cmd);
        }
    }

    fn switch_to(&mut self, cmd: PlaybackCmd) {
        if let Some(old) = self.current.take() {
            self.retire(old);
        }
        self.pos = 0;
        self.fade_remaining = 0;
        if let PlaybackCmd::Play(clip) = cmd {
            self.current = Some(clip);
        }
    }

    fn next_clip_sample(&mut self) -> f32 {
        let (len, raw) = match self.current.as_ref() {
            None => return 0.0,
            Some(c) => (c.len(), c.get(self.pos).copied()),
        };
        let Some(mut s) = raw else {
            // Clip finished (or empty): apply the follow-up command if any, else just release it.
            let cmd = self.after_fade.take().unwrap_or(PlaybackCmd::Stop);
            self.switch_to(cmd);
            return 0.0;
        };
        debug_assert!(self.pos < len);
        self.pos += 1;
        if self.fade_remaining > 0 {
            s *= self.fade_remaining as f32 / self.fade_len as f32;
            self.fade_remaining -= 1;
            if self.fade_remaining == 0 {
                let cmd = self.after_fade.take().unwrap_or(PlaybackCmd::Stop);
                self.switch_to(cmd);
            }
        }
        s
    }

    /// Produces `frames` mono samples (live passthrough ducked under the playback clip).
    pub fn fill(&mut self, frames: usize) -> &[f32] {
        self.scratch.clear();
        self.poll_mailbox();

        let pass_enabled = self.params.live_passthrough.load(Ordering::Relaxed);
        let mut drop_one = false;
        let mut repeat_one = false;
        if !pass_enabled {
            self.prefilled = false;
            self.pass_ring.clear();
        } else {
            let mut q_len = self.pass_ring.len();
            if !self.prefilled && q_len >= self.prefill_samples {
                self.prefilled = true;
            }
            if q_len > self.hard_max_samples {
                // Safety net: far too much queued, jump back to the target latency.
                self.pass_ring.discard_oldest(q_len - self.target_samples);
                q_len = self.target_samples;
            }
            if self.prefilled {
                if q_len > self.drift_high_samples {
                    drop_one = true;
                } else if q_len > 0 && q_len < self.drift_low_samples {
                    repeat_one = true;
                }
            }
        }

        let mut peak = 0.0_f32;
        for i in 0..frames {
            let live = if pass_enabled && self.prefilled {
                if i == 0 && repeat_one {
                    self.last_live
                } else {
                    if i == 0 && drop_one {
                        let _ = self.pass_ring.pop();
                    }
                    match self.pass_ring.pop() {
                        Some(v) => {
                            self.last_live = v;
                            v
                        }
                        None => {
                            // Underrun: go back to the prefill state instead of crackling.
                            self.prefilled = false;
                            self.last_live = 0.0;
                            0.0
                        }
                    }
                }
            } else {
                0.0
            };

            let duck_target = if self.clip_playing() {
                DUCK_TARGET_GAIN
            } else {
                1.0
            };
            self.duck_gain += (duck_target - self.duck_gain) * self.duck_coef;

            let pb = self.next_clip_sample();
            let mixed = (pb + live * self.duck_gain).clamp(-0.98, 0.98);
            peak = peak.max(mixed.abs());
            self.scratch.push(mixed);
        }

        self.smoothed_out_lvl = self.smoothed_out_lvl * 0.72 + peak * 0.28;
        AtomicAudioParams::store_f32(&self.params.output_level, self.smoothed_out_lvl);
        self.params
            .is_playing
            .store(self.clip_playing(), Ordering::Relaxed);

        let queue_ms = (self.pass_ring.len() as f32 / self.out_sr as f32) * 1000.0;
        let in_lat = AtomicAudioParams::load_f32(&self.params.input_latency_ms);
        match (in_lat >= 0.0, self.output_latency_ms) {
            (true, Some(out_lat)) => {
                AtomicAudioParams::store_f32(
                    &self.params.measured_latency_ms,
                    in_lat + queue_ms + out_lat,
                );
                self.params.latency_measured.store(true, Ordering::Relaxed);
            }
            _ => {
                AtomicAudioParams::store_f32(&self.params.measured_latency_ms, 15.0 + queue_ms);
                self.params.latency_measured.store(false, Ordering::Relaxed);
            }
        }

        &self.scratch
    }
}

fn input_latency_ms(info: &cpal::InputCallbackInfo) -> f32 {
    let ts = info.timestamp();
    ts.callback
        .duration_since(&ts.capture)
        .map_or(-1.0, |d| d.as_secs_f32() * 1000.0)
}

fn output_latency_ms(info: &cpal::OutputCallbackInfo) -> Option<f32> {
    let ts = info.timestamp();
    ts.playback
        .duration_since(&ts.callback)
        .map(|d| d.as_secs_f32() * 1000.0)
}

/// Background recorder: drains the record ring on its own thread so samples are not lost even
/// when the UI does not repaint (e.g. window minimized).
pub struct Recorder {
    params: Arc<AtomicAudioParams>,
    ring: Arc<LockFreeRingBuffer>,
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn start(ring: Arc<LockFreeRingBuffer>, params: Arc<AtomicAudioParams>) -> Recorder {
        ring.clear();
        params.record_overflow.store(false, Ordering::Relaxed);
        let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let t_ring = Arc::clone(&ring);
        let t_samples = Arc::clone(&samples);
        let t_stop = Arc::clone(&stop);
        params.is_recording.store(true, Ordering::Relaxed);
        let handle = std::thread::spawn(move || {
            while !t_stop.load(Ordering::Relaxed) {
                {
                    let mut v = t_samples.lock().unwrap_or_else(|e| e.into_inner());
                    t_ring.drain_into(&mut v);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        Recorder {
            params,
            ring,
            samples,
            stop,
            handle: Some(handle),
        }
    }

    /// Number of samples captured so far (mono, at the input sample rate).
    pub fn captured_len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Stops recording and returns `(samples, overflowed)`.
    pub fn finish(mut self) -> (Vec<f32>, bool) {
        self.params.is_recording.store(false, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let mut v = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        // The worker thread is gone, so this thread is the only consumer now.
        self.ring.drain_into(&mut v);
        let out = std::mem::take(&mut *v);
        let overflow = self.params.record_overflow.load(Ordering::Relaxed);
        (out, overflow)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            self.params.is_recording.store(false, Ordering::Relaxed);
            let _ = h.join();
        }
    }
}

/// Picks the position of the device to select: the preferred name if present, otherwise the
/// system default device, otherwise the first device.
fn pick_device_pos(devices: &[(usize, String)], preferred: &str, default_name: &str) -> usize {
    // Pass 1: exact match on the preferred name.
    if !preferred.is_empty() {
        if let Some(p) = devices.iter().position(|(_, n)| n == preferred) {
            return p;
        }
    }
    // Pass 2: system default device.
    if !default_name.is_empty() {
        if let Some(p) = devices.iter().position(|(_, n)| n == default_name) {
            return p;
        }
    }
    0
}

/// True when a device name looks like a virtual audio cable (VB-Cable, Voicemeeter, or a
/// device the user renamed to "Voxara ..."). Case-insensitive.
pub fn is_virtual_mic_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    ["cable", "virtual", "voicemeeter", "voxara"]
        .iter()
        .any(|k| lower.contains(k))
}

/// True for VB-Cable's playback endpoint ("CABLE Input ..."). Case-insensitive.
pub fn is_cable_input_name(name: &str) -> bool {
    name.to_lowercase().contains("cable input")
}

pub struct AudioEngine {
    pub params: Arc<AtomicAudioParams>,
    pub passthrough_ring: Arc<LockFreeRingBuffer>,
    pub playback: Arc<PlaybackMailbox>,
    pub record_ring: Arc<LockFreeRingBuffer>,
    pub is_running: Arc<AtomicBool>,

    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,

    /// `(raw_cpal_index, device_name)` pairs so skipped unreadable devices never shift selection.
    pub input_devices: Vec<(usize, String)>,
    pub output_devices: Vec<(usize, String)>,
    pub selected_input_pos: usize,
    pub selected_output_pos: usize,
}

impl AudioEngine {
    pub fn new(initial_cfg: DspConfig, preferred_in: &str, preferred_out: &str) -> Self {
        let mut engine = Self {
            params: Arc::new(AtomicAudioParams::new(initial_cfg)),
            passthrough_ring: Arc::new(LockFreeRingBuffer::new(32768)),
            playback: Arc::new(PlaybackMailbox::new()),
            record_ring: Arc::new(LockFreeRingBuffer::new(1 << 20)), // ~21 seconds ring chunk buffer drained by UI
            is_running: Arc::new(AtomicBool::new(false)),
            input_stream: None,
            output_stream: None,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            selected_input_pos: 0,
            selected_output_pos: 0,
        };
        engine.refresh_devices(preferred_in, preferred_out);
        engine
    }

    pub fn refresh_devices(&mut self, preferred_in: &str, preferred_out: &str) {
        let host = cpal::default_host();
        self.input_devices.clear();
        self.output_devices.clear();
        self.selected_input_pos = 0;
        self.selected_output_pos = 0;

        let default_in = host
            .default_input_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();
        let default_out = host
            .default_output_device()
            .and_then(|d| d.name().ok())
            .unwrap_or_default();

        if let Ok(devs) = host.input_devices() {
            for (raw_idx, d) in devs.enumerate() {
                if let Ok(name) = d.name() {
                    self.input_devices.push((raw_idx, name));
                }
            }
        }
        if let Ok(devs) = host.output_devices() {
            for (raw_idx, d) in devs.enumerate() {
                if let Ok(name) = d.name() {
                    self.output_devices.push((raw_idx, name));
                }
            }
        }

        self.selected_input_pos = pick_device_pos(&self.input_devices, preferred_in, &default_in);
        self.selected_output_pos =
            pick_device_pos(&self.output_devices, preferred_out, &default_out);

        // With no saved output preference, auto-select VB-Cable's "CABLE Input" when present.
        // A saved preference is never overridden.
        if preferred_out.is_empty() {
            if let Some(p) = self
                .output_devices
                .iter()
                .position(|(_, n)| is_cable_input_name(n))
            {
                self.selected_output_pos = p;
            }
        }
    }

    pub fn selected_input_name(&self) -> String {
        self.input_devices
            .get(self.selected_input_pos)
            .map(|(_, n)| n.clone())
            .unwrap_or_default()
    }

    pub fn selected_output_name(&self) -> String {
        self.output_devices
            .get(self.selected_output_pos)
            .map(|(_, n)| n.clone())
            .unwrap_or_default()
    }

    pub fn is_virtual_mic_selected(&self) -> bool {
        is_virtual_mic_name(&self.selected_output_name())
    }

    pub fn stop(&mut self) {
        self.is_running.store(false, Ordering::Relaxed);
        // Dropping the streams joins the audio threads, so the ring/mailbox are consumer-free below.
        self.input_stream = None;
        self.output_stream = None;
        self.passthrough_ring.clear();
        self.playback.reset();
        self.params.is_playing.store(false, Ordering::Relaxed);
        self.params.latency_measured.store(false, Ordering::Relaxed);
        AtomicAudioParams::store_f32(&self.params.input_level, 0.0);
        AtomicAudioParams::store_f32(&self.params.output_level, 0.0);
        AtomicAudioParams::store_f32(&self.params.measured_latency_ms, 0.0);
        AtomicAudioParams::store_f32(&self.params.input_latency_ms, -1.0);
    }

    pub fn start(&mut self) -> Result<String, String> {
        self.stop();

        let host = cpal::default_host();
        let in_raw_idx = self
            .input_devices
            .get(self.selected_input_pos)
            .map(|(idx, _)| *idx);
        let in_device = in_raw_idx
            .and_then(|idx| host.input_devices().ok()?.nth(idx))
            .or_else(|| host.default_input_device())
            .ok_or_else(|| "Mikrofon input tidak ditemukan".to_string())?;

        let out_raw_idx = self
            .output_devices
            .get(self.selected_output_pos)
            .map(|(idx, _)| *idx);
        let out_device = out_raw_idx
            .and_then(|idx| host.output_devices().ok()?.nth(idx))
            .or_else(|| host.default_output_device())
            .ok_or_else(|| "Perangkat output audio tidak ditemukan".to_string())?;

        let in_supported = in_device
            .default_input_config()
            .map_err(|e| format!("Gagal membaca konfigurasi mic: {e}"))?;
        let out_supported = out_device
            .default_output_config()
            .map_err(|e| format!("Gagal membaca konfigurasi output: {e}"))?;

        let in_sr = in_supported.sample_rate().0;
        let out_sr = out_supported.sample_rate().0;
        let in_channels = in_supported.channels().max(1) as usize;
        let out_channels = out_supported.channels().max(1) as usize;
        let in_fmt = in_supported.sample_format();
        let out_fmt = out_supported.sample_format();

        let in_config: cpal::StreamConfig = in_supported.into();
        let out_config: cpal::StreamConfig = out_supported.into();

        self.params.in_sample_rate.store(in_sr, Ordering::Relaxed);
        self.params.out_sample_rate.store(out_sr, Ordering::Relaxed);
        self.passthrough_ring.clear();

        let params_in = Arc::clone(&self.params);
        let pass_ring_in = Arc::clone(&self.passthrough_ring);
        let rec_ring_in = Arc::clone(&self.record_ring);

        // Callback-owned state: zero locks and zero per-callback allocations!
        let mut dsp_chain = RealtimeDspChain::new(in_sr, params_in.load_dsp_config());
        let mut resampler = StatefulResampler::new(in_sr, out_sr);
        let mut mono_buf = Vec::<f32>::with_capacity(4096);
        let mut dsp_buf = Vec::<f32>::with_capacity(4096);
        let mut resampled_buf = Vec::<f32>::with_capacity(4096);
        let mut smoothed_in_lvl = 0.0_f32;

        let mut process_mono_in = move |mono: &[f32], latency_ms: f32| {
            let cfg = params_in.load_dsp_config();
            dsp_chain.update_config(cfg);
            AtomicAudioParams::store_f32(&params_in.input_latency_ms, latency_ms);

            if params_in.is_recording.load(Ordering::Relaxed) {
                let dropped = rec_ring_in.push_slice(mono);
                if dropped > 0 {
                    params_in.record_overflow.store(true, Ordering::Relaxed);
                }
            }

            let pass_enabled = params_in.live_passthrough.load(Ordering::Relaxed);
            dsp_buf.clear();
            let mut peak = 0.0_f32;
            for &s in mono {
                let y = dsp_chain.process_sample(s);
                peak = peak.max(y.abs());
                if pass_enabled {
                    dsp_buf.push(y);
                }
            }
            smoothed_in_lvl = smoothed_in_lvl * 0.72 + peak * 0.28;
            AtomicAudioParams::store_f32(&params_in.input_level, smoothed_in_lvl);

            if pass_enabled {
                resampler.process_into(&dsp_buf, &mut resampled_buf);
                pass_ring_in.push_slice(&resampled_buf);
            }
        };

        let in_err = |err| eprintln!("Audio input stream error: {err}");
        let in_stream = match in_fmt {
            cpal::SampleFormat::F32 => in_device.build_input_stream(
                &in_config,
                move |data: &[f32], info: &cpal::InputCallbackInfo| {
                    mono_buf.clear();
                    for frame in data.chunks_exact(in_channels) {
                        mono_buf.push(frame.iter().sum::<f32>() / in_channels as f32);
                    }
                    process_mono_in(&mono_buf, input_latency_ms(info));
                },
                in_err,
                None,
            ),
            cpal::SampleFormat::I16 => in_device.build_input_stream(
                &in_config,
                move |data: &[i16], info: &cpal::InputCallbackInfo| {
                    mono_buf.clear();
                    for frame in data.chunks_exact(in_channels) {
                        let sum: f32 = frame.iter().map(|&v| v as f32 / 32768.0).sum();
                        mono_buf.push(sum / in_channels as f32);
                    }
                    process_mono_in(&mono_buf, input_latency_ms(info));
                },
                in_err,
                None,
            ),
            cpal::SampleFormat::U16 => in_device.build_input_stream(
                &in_config,
                move |data: &[u16], info: &cpal::InputCallbackInfo| {
                    mono_buf.clear();
                    for frame in data.chunks_exact(in_channels) {
                        let sum: f32 = frame
                            .iter()
                            .map(|&v| (v as f32 - 32768.0) / 32768.0)
                            .sum();
                        mono_buf.push(sum / in_channels as f32);
                    }
                    process_mono_in(&mono_buf, input_latency_ms(info));
                },
                in_err,
                None,
            ),
            _ => return Err("Format sampel mikrofon tidak didukung".to_string()),
        }
        .map_err(|e| format!("Gagal membuka mikrofon: {e}"))?;

        let mut mixer = OutputMixer::new(
            Arc::clone(&self.params),
            Arc::clone(&self.passthrough_ring),
            Arc::clone(&self.playback),
            out_sr,
        );

        let out_err = |err| eprintln!("Audio output stream error: {err}");
        let out_stream = match out_fmt {
            cpal::SampleFormat::F32 => out_device.build_output_stream(
                &out_config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / out_channels;
                    mixer.output_latency_ms = output_latency_ms(info);
                    let mono = mixer.fill(frames);
                    for (i, frame) in data.chunks_exact_mut(out_channels).enumerate() {
                        frame.fill(mono[i]);
                    }
                },
                out_err,
                None,
            ),
            cpal::SampleFormat::I16 => out_device.build_output_stream(
                &out_config,
                move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / out_channels;
                    mixer.output_latency_ms = output_latency_ms(info);
                    let mono = mixer.fill(frames);
                    for (i, frame) in data.chunks_exact_mut(out_channels).enumerate() {
                        let s = (mono[i].clamp(-1.0, 1.0) * 32767.0) as i16;
                        frame.fill(s);
                    }
                },
                out_err,
                None,
            ),
            cpal::SampleFormat::U16 => out_device.build_output_stream(
                &out_config,
                move |data: &mut [u16], info: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / out_channels;
                    mixer.output_latency_ms = output_latency_ms(info);
                    let mono = mixer.fill(frames);
                    for (i, frame) in data.chunks_exact_mut(out_channels).enumerate() {
                        let s = ((mono[i].clamp(-1.0, 1.0) * 32767.0) + 32768.0) as u16;
                        frame.fill(s);
                    }
                },
                out_err,
                None,
            ),
            _ => return Err("Format sampel perangkat output tidak didukung".to_string()),
        }
        .map_err(|e| format!("Gagal membuka output audio: {e}"))?;

        in_stream.play().map_err(|e| format!("Gagal memulai stream mic: {e}"))?;
        if let Err(e) = out_stream.play() {
            drop(in_stream);
            return Err(format!("Gagal memulai stream output: {e}"));
        }

        // Only mark running after BOTH streams successfully started
        self.input_stream = Some(in_stream);
        self.output_stream = Some(out_stream);
        self.is_running.store(true, Ordering::Relaxed);

        Ok(format!(
            "Audio WASAPI aktif (Mic {in_sr} Hz -> Out {out_sr} Hz)"
        ))
    }

    /// Resamples `samples` to the output rate and hands the clip to the output callback.
    /// Any clip currently playing is faded out (~5 ms) and replaced; clips of any length work.
    pub fn queue_playback(&self, samples: &[f32], src_sr: u32) {
        let out_sr = self.params.out_sample_rate.load(Ordering::Relaxed);
        let resampled = crate::dsp::resample_batch(samples, src_sr, out_sr);
        let clip: Arc<[f32]> = Arc::from(resampled);
        self.playback.post(PlaybackCmd::Play(clip));
    }

    /// Stops the current clip with a short fade-out.
    pub fn stop_playback(&self) {
        self.playback.post(PlaybackCmd::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_mic_name_matcher() {
        assert!(is_virtual_mic_name("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(is_virtual_mic_name("cable input"));
        assert!(is_virtual_mic_name("Voicemeeter Input"));
        assert!(is_virtual_mic_name("Some Virtual Device"));
        assert!(is_virtual_mic_name("Voxara Mic"));
        assert!(is_virtual_mic_name("VOXARA mic (renamed)"));
        assert!(!is_virtual_mic_name("Speakers (Realtek Audio)"));
        assert!(!is_virtual_mic_name(""));
        assert!(is_cable_input_name("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(!is_cable_input_name("CABLE Output (VB-Audio Virtual Cable)"));
        assert!(!is_cable_input_name("Voxara Mic"));
    }

    const SR: u32 = 48000;

    fn make_mixer(pass_on: bool) -> (OutputMixer, Arc<LockFreeRingBuffer>, Arc<PlaybackMailbox>) {
        let params = Arc::new(AtomicAudioParams::new(DspConfig::default()));
        params.live_passthrough.store(pass_on, Ordering::Relaxed);
        let ring = Arc::new(LockFreeRingBuffer::new(32768));
        let mb = Arc::new(PlaybackMailbox::new());
        let mixer = OutputMixer::new(params, Arc::clone(&ring), Arc::clone(&mb), SR);
        (mixer, ring, mb)
    }

    #[test]
    fn test_lock_free_ring_buffer_wraparound() {
        let rb = LockFreeRingBuffer::new(64);
        assert_eq!(rb.capacity(), 64);
        for round in 0..5 {
            for i in 0..40 {
                assert!(rb.push((round * 100 + i) as f32));
            }
            for i in 0..40 {
                assert_eq!(rb.pop(), Some((round * 100 + i) as f32));
            }
        }
        assert_eq!(rb.pop(), None);
    }

    #[test]
    fn test_push_slice_returns_dropped_count() {
        let rb = LockFreeRingBuffer::new(64);
        let data = vec![1.0_f32; 100];
        assert_eq!(rb.push_slice(&data), 36);
        assert_eq!(rb.len(), 64);
        assert_eq!(rb.push_slice(&[0.5]), 1);
        for _ in 0..10 {
            rb.pop();
        }
        assert_eq!(rb.push_slice(&data[..20]), 10);
        assert_eq!(rb.push_slice(&[]), 0);
    }

    #[test]
    fn test_atomic_params_round_trip() {
        let mut cfg = DspConfig::default();
        cfg.nasal_freq_hz = 315.0;
        cfg.deesser_strength = 0.72;
        let params = AtomicAudioParams::new(cfg);
        assert!(!params.live_passthrough.load(Ordering::Relaxed));
        let loaded = params.load_dsp_config();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn test_mixer_stop_fades_then_silent() {
        let (mut mixer, _ring, mb) = make_mixer(false);
        mb.post(PlaybackCmd::Play(Arc::from(vec![0.5_f32; SR as usize * 2])));
        let first: Vec<f32> = mixer.fill(480).to_vec();
        assert!(first.iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert!(mixer.params.is_playing.load(Ordering::Relaxed));

        mb.post(PlaybackCmd::Stop);
        let fade_len = mixer.fade_len;
        let out: Vec<f32> = mixer.fill(480).to_vec();
        // Fade-out is monotonically non-increasing and starts near the clip level.
        assert!(out[0] > 0.45);
        for w in out[..fade_len].windows(2) {
            assert!(w[1] <= w[0] + 1e-6);
        }
        assert!(out[fade_len..].iter().all(|&s| s == 0.0));
        let more: Vec<f32> = mixer.fill(480).to_vec();
        assert!(more.iter().all(|&s| s == 0.0));
        assert!(!mixer.params.is_playing.load(Ordering::Relaxed));
        // Old clip is handed back for freeing on the UI side on the next poll.
        mixer.fill(16);
        assert!(mb.slot.lock().unwrap().retired.is_some());
    }

    #[test]
    fn test_mixer_replace_fades_old_clip_then_plays_new() {
        let (mut mixer, _ring, mb) = make_mixer(false);
        mb.post(PlaybackCmd::Play(Arc::from(vec![0.7_f32; 10000])));
        mixer.fill(100);
        mb.post(PlaybackCmd::Play(Arc::from(vec![0.25_f32; 1000])));
        let fade_len = mixer.fade_len;
        let out: Vec<f32> = mixer.fill(2000).to_vec();
        assert!(out[fade_len].abs() < 0.3);
        let new_samples = out.iter().filter(|&&s| (s - 0.25).abs() < 1e-6).count();
        assert_eq!(new_samples, 1000);
    }

    #[test]
    fn test_mixer_plays_50s_clip_fully() {
        let (mut mixer, _ring, mb) = make_mixer(false);
        let len = SR as usize * 50;
        mb.post(PlaybackCmd::Play(Arc::from(vec![0.25_f32; len])));
        let mut non_silent = 0usize;
        for _ in 0..(len / 480 + 20) {
            non_silent += mixer.fill(480).iter().filter(|&&s| s != 0.0).count();
        }
        assert_eq!(non_silent, len);
        assert!(!mixer.params.is_playing.load(Ordering::Relaxed));
    }

    #[test]
    fn test_mixer_ducking_is_smooth() {
        let (mut mixer, ring, mb) = make_mixer(true);
        let target = mixer.target_samples;
        let mut gains: Vec<f32> = Vec::new();
        let frames = 480;
        let run = |mixer: &mut OutputMixer, gains: &mut Vec<f32>| {
            let q = ring.len();
            if q < target {
                ring.push_slice(&vec![0.5_f32; target - q]);
            }
            gains.extend(mixer.fill(frames).iter().map(|&s| s / 0.5));
        };
        // Settle the passthrough path first.
        for _ in 0..5 {
            run(&mut mixer, &mut gains);
        }
        gains.clear();
        // Zero-valued clip so the output is exactly live * duck_gain.
        mb.post(PlaybackCmd::Play(Arc::from(vec![0.0_f32; SR as usize / 5])));
        for _ in 0..60 {
            run(&mut mixer, &mut gains);
        }
        let max_step = gains
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_step < 0.01, "ducking step too large: {max_step}");
        let min_gain = gains.iter().cloned().fold(f32::MAX, f32::min);
        assert!(min_gain < 0.2, "ducking never reached target: {min_gain}");
        assert!(*gains.last().unwrap() > 0.95, "ducking did not release");
    }

    #[test]
    fn test_mixer_underrun_reprefills() {
        let (mut mixer, ring, _mb) = make_mixer(true);
        ring.push_slice(&vec![0.3_f32; mixer.prefill_samples + 10]);
        let out: Vec<f32> = mixer.fill(2000).to_vec();
        assert!(!mixer.prefilled, "should drop back to prefill after underrun");
        assert!(out[mixer.prefill_samples + 50..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn test_mixer_hard_discard_when_queue_huge() {
        let (mut mixer, ring, _mb) = make_mixer(true);
        ring.push_slice(&vec![0.3_f32; 20000]);
        mixer.fill(16);
        assert!(ring.len() <= mixer.hard_max_samples);
    }

    #[test]
    fn test_pick_device_pos_fallbacks() {
        let devs = vec![
            (0, "A".to_string()),
            (1, "B".to_string()),
            (2, "C".to_string()),
        ];
        assert_eq!(pick_device_pos(&devs, "C", "B"), 2);
        assert_eq!(pick_device_pos(&devs, "missing", "B"), 1);
        assert_eq!(pick_device_pos(&devs, "", "B"), 1);
        assert_eq!(pick_device_pos(&devs, "missing", "also missing"), 0);
        assert_eq!(pick_device_pos(&[], "x", "y"), 0);
    }

    #[test]
    fn test_recorder_collects_all_samples_in_order() {
        let params = Arc::new(AtomicAudioParams::new(DspConfig::default()));
        let ring = Arc::new(LockFreeRingBuffer::new(1 << 16));
        let rec = Recorder::start(Arc::clone(&ring), Arc::clone(&params));
        assert!(params.is_recording.load(Ordering::Relaxed));
        let total = 50_000usize;
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        for chunk in data.chunks(1000) {
            assert_eq!(ring.push_slice(chunk), 0);
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(rec.captured_len() > 0);
        let (out, overflow) = rec.finish();
        assert!(!overflow);
        assert!(!params.is_recording.load(Ordering::Relaxed));
        assert_eq!(out, data);
    }

    #[test]
    fn test_recorder_reports_overflow_flag() {
        let params = Arc::new(AtomicAudioParams::new(DspConfig::default()));
        let ring = Arc::new(LockFreeRingBuffer::new(64));
        let rec = Recorder::start(Arc::clone(&ring), Arc::clone(&params));
        params.record_overflow.store(true, Ordering::Relaxed);
        let (_out, overflow) = rec.finish();
        assert!(overflow);
    }
}
