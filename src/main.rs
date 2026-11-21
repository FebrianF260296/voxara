#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio_engine;
mod dsp;
mod voice_profile;

use audio_engine::{is_cable_input_name, is_virtual_mic_name, AtomicAudioParams, AudioEngine, Recorder};
use dsp::{
    auto_calibrate_dsp_from_sample, load_mono_wav, process_cleft_palate_audio, save_mono_wav,
    AudioMetrics, DspConfig, BAND_CENTERS_HZ,
};
use eframe::egui;
use std::sync::{atomic::Ordering, mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use voice_profile::{app_path, fnv1a64, scene_wav_rel_path, AppSettings};

/// How long settings changes are debounced before they are written to disk.
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(700);
/// Appended to a status message when the record ring overflowed during a recording.
const OVERFLOW_WARNING: &str = "Peringatan: Sebagian audio hilang karena buffer rekaman penuh.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveTab {
    MegafonMeeting,
    StudioTeleprompter,
    CalibrationPreset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordMode {
    None,
    Studio,
    Calibration,
    SoundboardSlot(usize),
    TeleprompterScene(usize),
}

enum WorkerResult {
    /// Studio DSP (or export) finished. `gen` identifies the studio take it was computed from.
    StudioProcessed {
        processed: Vec<f32>,
        metrics: AudioMetrics,
        cfg: DspConfig,
        gen: u64,
        status: String,
    },
    ExportFinished(String),
    Calibrated {
        cfg: DspConfig,
        f0: f32,
        bands: [f32; 8],
        status: String,
    },
    SlotSaved(String),
    SceneProcessed {
        key: u64,
        audio: Vec<f32>,
        sr: u32,
        msg: String,
    },
    /// A worker panicked; still counts as that job's single result.
    JobFailed(String),
}

struct TeleprompterScene {
    /// `fnv1a64(text.trim())`; identifies the scene (and its cached WAV) across reloads.
    key: u64,
    text: String,
    recorded_audio: Vec<f32>,
    sample_rate: u32,
}

struct VoxaraApp {
    engine: AudioEngine,
    settings: AppSettings,
    active_tab: ActiveTab,

    live_passthrough_ui: bool,
    record_mode: RecordMode,
    record_deadline: Option<Instant>,
    /// Background recorder that drains the record ring on its own thread.
    recorder: Option<Recorder>,
    /// Input sample rate captured when the current recording started.
    record_sr: u32,
    status_message: String,

    // Studio recording (owned purely by UI/worker thread, never locked in audio callback)
    studio_raw: Vec<f32>,
    studio_processed: Vec<f32>,
    studio_sr: u32,
    studio_metrics: AudioMetrics,
    /// DSP settings `studio_processed` was computed with (None = not processed yet).
    studio_processed_cfg: Option<DspConfig>,
    /// Incremented whenever `studio_raw` is replaced; lets stale worker results be ignored.
    studio_gen: u64,

    // Multi-scene teleprompter voiceover (100% user's own clarified voice)
    script_input: String,
    scenes: Vec<TeleprompterScene>,
    active_scene_idx: usize,
    /// Script snapshot waiting for the user to confirm dropping recorded scenes.
    pending_script: Option<String>,

    // Async worker channel for non-blocking DSP, calibration & WAV export
    worker_tx: mpsc::Sender<WorkerResult>,
    worker_rx: mpsc::Receiver<WorkerResult>,
    /// Number of spawned workers whose result has not been received yet.
    pending_jobs: usize,

    // Debounced settings persistence
    settings_dirty: bool,
    last_settings_change: Instant,
}

impl VoxaraApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (settings, load_warning) = AppSettings::load();
        let engine = AudioEngine::new(
            settings.dsp_cfg,
            &settings.selected_input_name,
            &settings.selected_output_name,
        );
        let (worker_tx, worker_rx) = mpsc::channel();
        let script_input = settings.script_text.clone();
        let scenes = Self::build_scenes(&Self::script_texts(&script_input), &[]);

        let mut app = Self {
            engine,
            settings,
            active_tab: ActiveTab::MegafonMeeting,
            live_passthrough_ui: false,
            record_mode: RecordMode::None,
            record_deadline: None,
            status_message: load_warning.unwrap_or_else(|| {
                "Siap. Passthrough Megafon mati secara default agar aman dari feedback.".to_string()
            }),
            recorder: None,
            record_sr: 48000,
            studio_raw: Vec::new(),
            studio_processed: Vec::new(),
            studio_sr: 48000,
            studio_metrics: AudioMetrics::default(),
            studio_processed_cfg: None,
            studio_gen: 0,
            script_input,
            scenes,
            active_scene_idx: 0,
            pending_script: None,
            worker_tx,
            worker_rx,
            pending_jobs: 0,
            settings_dirty: false,
            last_settings_change: Instant::now(),
        };
        app.persist_auto_selected_output();
        app
    }

    /// If the engine auto-selected VB-Cable's "CABLE Input" because no output preference was
    /// saved, persist that choice. Returns true when the saved preference changed.
    fn persist_auto_selected_output(&mut self) -> bool {
        if !self.settings.selected_output_name.is_empty() {
            return false;
        }
        let name = self.engine.selected_output_name();
        if !is_cable_input_name(&name) {
            return false;
        }
        self.settings.selected_output_name = name;
        self.save_settings_checked();
        true
    }

    /// Splits a script into trimmed, non-empty lines (one per scene).
    fn script_texts(script: &str) -> Vec<String> {
        let mut res: Vec<String> = script
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if res.is_empty() {
            res.push("Silakan tulis naskah terlebih dahulu.".to_string());
        }
        res
    }

    /// Builds scenes for `texts`. Audio is reused from `old` when the scene key is unchanged,
    /// otherwise loaded from the scene's cached WAV if one exists.
    fn build_scenes(texts: &[String], old: &[TeleprompterScene]) -> Vec<TeleprompterScene> {
        texts
            .iter()
            .map(|text| {
                let key = fnv1a64(text.trim());
                let mut scene = TeleprompterScene {
                    key,
                    text: text.clone(),
                    recorded_audio: Vec::new(),
                    sample_rate: 48000,
                };
                if let Some(prev) = old
                    .iter()
                    .find(|s| s.key == key && !s.recorded_audio.is_empty())
                {
                    scene.recorded_audio = prev.recorded_audio.clone();
                    scene.sample_rate = prev.sample_rate;
                } else {
                    let path = app_path(&scene_wav_rel_path(text));
                    if path.exists() {
                        if let Ok((audio, sr)) = load_mono_wav(&path) {
                            scene.recorded_audio = audio;
                            scene.sample_rate = sr;
                        }
                    }
                }
                scene
            })
            .collect()
    }

    /// Relative WAV paths of recorded scenes that a script made of `texts` would drop.
    fn dropped_scene_paths(&self, texts: &[String]) -> Vec<String> {
        let new_keys: Vec<u64> = texts.iter().map(|t| fnv1a64(t.trim())).collect();
        let mut paths: Vec<String> = Vec::new();
        for scene in &self.scenes {
            if scene.recorded_audio.is_empty() || new_keys.contains(&scene.key) {
                continue;
            }
            let rel = scene_wav_rel_path(&scene.text);
            if !paths.contains(&rel) {
                paths.push(rel);
            }
        }
        paths
    }

    /// Asks to replace the scene list by the script text; applies right away unless recorded
    /// scenes would be lost (then a confirmation row is shown).
    fn request_script_reload(&mut self) {
        let script = self.script_input.clone();
        let dropped = self.dropped_scene_paths(&Self::script_texts(&script));
        if dropped.is_empty() {
            self.pending_script = None;
            self.apply_script(&script);
        } else {
            self.pending_script = Some(script);
        }
    }

    /// Replaces the scene list and deletes the WAV files of the scenes that were dropped.
    fn apply_script(&mut self, script: &str) {
        let texts = Self::script_texts(script);
        let dropped = self.dropped_scene_paths(&texts);
        let old = std::mem::take(&mut self.scenes);
        self.scenes = Self::build_scenes(&texts, &old);
        self.active_scene_idx = 0;

        let mut errors: Vec<String> = Vec::new();
        for rel in &dropped {
            if let Err(e) = std::fs::remove_file(app_path(rel)) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!("{rel}: {e}"));
                }
            }
        }
        self.status_message = if errors.is_empty() {
            format!("Naskah dimuat: {} scene.", self.scenes.len())
        } else {
            format!(
                "Naskah dimuat, tetapi gagal menghapus file scene lama: {}",
                errors.join("; ")
            )
        };
    }

    fn unique_stamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn save_settings_checked(&mut self) {
        self.settings_dirty = false;
        if let Err(err) = self.settings.save() {
            self.status_message = err;
        }
    }

    /// Schedules a debounced settings save (see `update`).
    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.last_settings_change = Instant::now();
    }

    /// Runs `job` on a worker thread. Exactly one result is always sent back, even if the
    /// job panics, so `pending_jobs` cannot get stuck.
    fn spawn_job<F>(&mut self, job: F)
    where
        F: FnOnce() -> WorkerResult + Send + 'static,
    {
        let tx = self.worker_tx.clone();
        self.pending_jobs += 1;
        thread::spawn(move || {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(
                |_| WorkerResult::JobFailed("Pemrosesan audio gagal secara tak terduga.".to_string()),
            );
            let _ = tx.send(res);
        });
    }

    fn ensure_audio_started(&mut self) -> bool {
        if self.engine.is_running.load(Ordering::Relaxed) {
            return true;
        }
        match self.engine.start() {
            Ok(msg) => {
                self.status_message = msg;
                true
            }
            Err(err) => {
                self.status_message = err;
                self.live_passthrough_ui = false;
                self.engine.params.live_passthrough.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    fn start_recording(&mut self, mode: RecordMode, auto_secs: Option<u64>) {
        if self.record_mode != RecordMode::None {
            self.status_message =
                "Rekaman lain sedang berjalan. Selesaikan dulu sebelum mulai yang baru.".to_string();
            return;
        }
        if !self.ensure_audio_started() {
            return;
        }
        self.record_sr = self.engine.params.in_sample_rate.load(Ordering::Relaxed);
        self.recorder = Some(Recorder::start(
            Arc::clone(&self.engine.record_ring),
            Arc::clone(&self.engine.params),
        ));
        self.record_mode = mode;
        self.record_deadline = auto_secs.map(|s| Instant::now() + Duration::from_secs(s));
        self.status_message = match mode {
            RecordMode::Calibration => {
                "Merekam kalibrasi 5 detik... Silakan berbicara secara alami.".to_string()
            }
            RecordMode::SoundboardSlot(idx) => format!(
                "Merekam Soundboard #{}: ucapkan kalimat Anda sekarang...",
                idx + 1
            ),
            RecordMode::TeleprompterScene(idx) => {
                format!("Merekam Scene #{}: baca teks teleprompter di layar...", idx + 1)
            }
            RecordMode::Studio => "Merekam suara di Studio... Klik Selesai jika sudah.".to_string(),
            RecordMode::None => String::new(),
        };
    }

    fn finish_recording(&mut self) {
        let mode = self.record_mode;
        let had_deadline = self.record_deadline.take().is_some();
        self.record_mode = RecordMode::None;
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        let (mut captured, overflowed) = recorder.finish();

        let sr = self.record_sr.max(8000);
        if captured.len() < (sr as usize / 4) {
            self.status_message = "Rekaman terlalu pendek, coba ulangi.".to_string();
            return;
        }
        // If the app was minimized past the auto deadline, keep only the first 5 seconds.
        if mode == RecordMode::Calibration && had_deadline {
            captured.truncate(sr as usize * 5);
        }
        let suffix = if overflowed {
            format!(" {OVERFLOW_WARNING}")
        } else {
            String::new()
        };

        match mode {
            RecordMode::Calibration => {
                self.status_message = "Menganalisis hasil kalibrasi...".to_string();
                self.spawn_job(move || {
                    let (cfg, f0, bands) = auto_calibrate_dsp_from_sample(&captured, sr);
                    WorkerResult::Calibrated {
                        cfg,
                        f0,
                        bands,
                        status: format!(
                            "Auto-Kalibrasi selesai! Puncak sengau: {:.0} Hz | Pitch F0: {:.1} Hz{suffix}",
                            cfg.nasal_freq_hz, f0
                        ),
                    }
                });
            }
            RecordMode::SoundboardSlot(idx) => {
                let Some(slot) = self.settings.soundboard_slots.get(idx) else {
                    return;
                };
                let title = slot.title.clone();
                let path = app_path(&slot.wav_rel_path);
                let cfg = self.settings.dsp_cfg;
                self.status_message = "Memproses rekaman soundboard...".to_string();
                self.spawn_job(move || {
                    let (clean, _) = process_cleft_palate_audio(&captured, sr, &cfg);
                    let msg = match save_mono_wav(&path, &clean, sr) {
                        Ok(_) => format!(
                            "Soundboard \"{title}\" berhasil direkam & diperjelas ({:.1} dtk).{suffix}",
                            clean.len() as f32 / sr as f32
                        ),
                        Err(e) => format!("Gagal simpan slot: {e}{suffix}"),
                    };
                    WorkerResult::SlotSaved(msg)
                });
            }
            RecordMode::TeleprompterScene(idx) => {
                let Some(scene) = self.scenes.get(idx) else {
                    return;
                };
                let key = scene.key;
                let path = app_path(&scene_wav_rel_path(&scene.text));
                let cfg = self.settings.dsp_cfg;
                self.status_message = format!("Memproses Scene #{}...", idx + 1);
                self.spawn_job(move || {
                    let (clean, _) = process_cleft_palate_audio(&captured, sr, &cfg);
                    let dur = clean.len() as f32 / sr as f32;
                    let msg = match save_mono_wav(&path, &clean, sr) {
                        Ok(_) => format!(
                            "Scene #{} selesai direkam & diperjelas ({dur:.1} dtk).{suffix}",
                            idx + 1
                        ),
                        Err(e) => format!(
                            "Scene #{} diproses, tetapi gagal disimpan ke disk: {e}{suffix}",
                            idx + 1
                        ),
                    };
                    WorkerResult::SceneProcessed {
                        key,
                        audio: clean,
                        sr,
                        msg,
                    }
                });
            }
            RecordMode::Studio => {
                self.studio_raw = captured;
                self.studio_sr = sr;
                self.studio_gen = self.studio_gen.wrapping_add(1);
                self.studio_processed.clear();
                self.studio_processed_cfg = None;
                self.studio_metrics = AudioMetrics::default();
                self.trigger_studio_dsp_worker(suffix);
            }
            RecordMode::None => {}
        }
    }

    /// Processes the current studio take with the current DSP settings on a worker thread.
    /// `suffix` is appended to the resulting status message.
    fn trigger_studio_dsp_worker(&mut self, suffix: String) {
        if self.studio_raw.is_empty() {
            self.status_message = "Belum ada audio di Studio.".to_string();
            return;
        }
        let raw = self.studio_raw.clone();
        let sr = self.studio_sr;
        let cfg = self.settings.dsp_cfg;
        let gen = self.studio_gen;
        self.spawn_job(move || {
            let (processed, metrics) = process_cleft_palate_audio(&raw, sr, &cfg);
            let dur = raw.len() as f32 / sr.max(1) as f32;
            WorkerResult::StudioProcessed {
                processed,
                metrics,
                cfg,
                gen,
                status: format!("Selesai memproses {dur:.1} detik audio dengan Rust DSP.{suffix}"),
            }
        });
    }

    /// Processes the studio take with the CURRENT DSP settings, saves it as a WAV and returns
    /// the processed audio so the studio state stays in sync with what was exported.
    fn trigger_studio_export_worker(&mut self) {
        if self.studio_raw.is_empty() {
            self.status_message = "Belum ada audio di Studio.".to_string();
            return;
        }
        let raw = self.studio_raw.clone();
        let sr = self.studio_sr;
        let cfg = self.settings.dsp_cfg;
        let gen = self.studio_gen;
        let path = app_path(&format!("output/rekaman_jelas_{}.wav", Self::unique_stamp()));
        self.spawn_job(move || {
            let (processed, metrics) = process_cleft_palate_audio(&raw, sr, &cfg);
            let status = match save_mono_wav(&path, &processed, sr) {
                Ok(_) => format!("Tersimpan: {}", path.display()),
                Err(e) => format!("Gagal ekspor: {e}"),
            };
            WorkerResult::StudioProcessed {
                processed,
                metrics,
                cfg,
                gen,
                status,
            }
        });
    }

    /// Pushes the DSP settings to the realtime engine (no disk write).
    fn apply_dsp_live(&mut self) {
        self.engine.params.store_dsp_config(self.settings.dsp_cfg);
    }

    /// Pushes the DSP settings to the engine and saves immediately (presets, reset).
    fn sync_dsp_to_engine(&mut self) {
        self.apply_dsp_live();
        self.save_settings_checked();
    }

    fn draw_level_bar(ui: &mut egui::Ui, label: &str, level: f32, accent: egui::Color32) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(label).small().color(egui::Color32::LIGHT_GRAY));
            let (rect, _) = ui.allocate_exact_size(egui::vec2(105.0, 11.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(30, 34, 42));
            let clamped = (level * 3.0).clamp(0.0, 1.0);
            let fill_w = rect.width() * clamped;
            if fill_w > 1.0 {
                let fill_rect =
                    egui::Rect::from_min_size(rect.min, egui::vec2(fill_w, rect.height()));
                painter.rect_filled(fill_rect, 3.0, accent);
            }
        });
    }

    fn render_dsp_sliders(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.breath_compensator, 0.0..=1.0)
                    .text("Penstabil Napas Kalimat Panjang (AGC)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.denasal_strength, 0.0..=1.0)
                    .text("Peredam Sengau Adaptif"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.nasal_freq_hz, 210.0..=380.0)
                    .text("Frekuensi Sengau (Hz)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.oral_formant_boost, 0.0..=1.0)
                    .text("Kejernihan Vokal Mulut (F1/F2)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.consonant_boost, 0.0..=1.0)
                    .text("Penguat Konsonan (P/T/K/S)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.deesser_strength, 0.0..=1.0)
                    .text("De-Esser (Anti-Desis Tajam)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.air_leak_gate, 0.0..=1.0)
                    .text("Air-Gate (Redam Napas Jeda)"),
            )
            .changed();
        changed |= ui
            .add(
                egui::Slider::new(&mut self.settings.dsp_cfg.output_gain_db, -6.0..=12.0)
                    .text("Volume Output (dB)"),
            )
            .changed();

        if changed {
            self.apply_dsp_live();
            self.mark_settings_dirty();
        }
    }

    fn render_megafon_tab(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            // Left: Audio Routing & Live Articulation Megaphone
            cols[0].group(|ui| {
                ui.heading("1. Perangkat Audio & Virtual Mic");
                ui.add_space(4.0);

                let mut device_changed = false;
                ui.horizontal(|ui| {
                    ui.label("Mikrofon Input:");
                    let cur_in = self.engine.selected_input_name();
                    egui::ComboBox::from_id_salt("mic_select")
                        .selected_text(if cur_in.is_empty() {
                            "Pilih Mikrofon".to_string()
                        } else {
                            cur_in
                        })
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for pos in 0..self.engine.input_devices.len() {
                                let name = self.engine.input_devices[pos].1.clone();
                                if ui
                                    .selectable_value(&mut self.engine.selected_input_pos, pos, &name)
                                    .clicked()
                                {
                                    self.settings.selected_input_name = name;
                                    device_changed = true;
                                }
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("Output / Virtual Mic:");
                    let cur_out = self.engine.selected_output_name();
                    egui::ComboBox::from_id_salt("out_select")
                        .selected_text(if cur_out.is_empty() {
                            "Pilih Output".to_string()
                        } else {
                            cur_out
                        })
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for pos in 0..self.engine.output_devices.len() {
                                let name = self.engine.output_devices[pos].1.clone();
                                if ui
                                    .selectable_value(&mut self.engine.selected_output_pos, pos, &name)
                                    .clicked()
                                {
                                    self.settings.selected_output_name = name;
                                    device_changed = true;
                                }
                            }
                        });
                });

                if device_changed {
                    self.save_settings_checked();
                    if self.engine.is_running.load(Ordering::Relaxed) {
                        match self.engine.start() {
                            Ok(m) => self.status_message = m,
                            Err(e) => {
                                self.status_message = e;
                                self.live_passthrough_ui = false;
                                self.engine.params.live_passthrough.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let is_running = self.engine.is_running.load(Ordering::Relaxed);
                    let lbl = if is_running {
                        "Matikan Mesin Audio"
                    } else {
                        "Aktifkan Mesin Audio"
                    };
                    let col = if is_running {
                        egui::Color32::from_rgb(175, 55, 55)
                    } else {
                        egui::Color32::from_rgb(35, 140, 85)
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(lbl).color(egui::Color32::WHITE).strong(),
                            )
                            .fill(col),
                        )
                        .clicked()
                    {
                        if is_running {
                            self.live_passthrough_ui = false;
                            self.engine.params.live_passthrough.store(false, Ordering::Relaxed);
                            self.engine.stop();
                            self.status_message = "Mesin audio dimatikan.".to_string();
                        } else {
                            let _ = self.ensure_audio_started();
                        }
                    }

                    if ui.button("Refresh Perangkat").clicked() {
                        self.engine.refresh_devices(
                            &self.settings.selected_input_name,
                            &self.settings.selected_output_name,
                        );
                        if self.persist_auto_selected_output()
                            && self.engine.is_running.load(Ordering::Relaxed)
                        {
                            match self.engine.start() {
                                Ok(m) => self.status_message = m,
                                Err(e) => {
                                    self.status_message = e;
                                    self.live_passthrough_ui = false;
                                    self.engine
                                        .params
                                        .live_passthrough
                                        .store(false, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                });

                if !self
                    .engine
                    .output_devices
                    .iter()
                    .any(|(_, n)| is_virtual_mic_name(n))
                {
                    ui.colored_label(
                        egui::Color32::from_rgb(130, 215, 255),
                        "VB-Cable belum terpasang. Unduh gratis dari vb-audio.com/Cable, lalu klik Refresh Perangkat.",
                    );
                }

                if ui
                    .button("Ganti Nama Mic jadi \"Voxara Mic\"")
                    .clicked()
                {
                    // Opens Windows Sound settings on the Recording tab; nothing is renamed by the app.
                    if let Err(e) = std::process::Command::new("control")
                        .args(["mmsys.cpl,,1"])
                        .spawn()
                    {
                        self.status_message = format!("Gagal membuka pengaturan Sound Windows: {e}");
                    }
                }
                egui::CollapsingHeader::new("Cara ganti nama").show(ui, |ui| {
                    ui.label("1) Di tab Recording, klik kanan \"CABLE Output\" → Properties.");
                    ui.label("2) Ganti nama menjadi \"Voxara Mic\" → OK.");
                    ui.label("3) Di Zoom/Meet/Teams pilih \"Voxara Mic\" sebagai mikrofon.");
                    ui.label(
                        egui::RichText::new(
                            "Windows mungkin tetap menampilkan \"(VB-Audio Virtual Cable)\" di belakang nama — itu dari driver dan normal.",
                        )
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Jangan ganti nama \"CABLE Input\" kecuali Anda juga memilihnya ulang di Voxara.",
                        )
                        .small()
                        .color(egui::Color32::GRAY),
                    );
                });

                ui.separator();
                ui.heading("2. Megafon Live (100% Suara Asli Anda)");

                if !self.engine.is_virtual_mic_selected() {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 90),
                        "Peringatan Feedback: Output saat ini bukan Virtual Mic. Pakai headphone atau pilih CABLE Input (VB-Cable) sebelum menyalakan Megafon Live.",
                    );
                }

                if ui
                    .checkbox(
                        &mut self.live_passthrough_ui,
                        "Nyalakan Megafon Live (Kirim suara jernih kontinu ke Output)",
                    )
                    .changed()
                {
                    if self.live_passthrough_ui {
                        if self.ensure_audio_started() {
                            self.engine.params.live_passthrough.store(true, Ordering::Relaxed);
                        }
                    } else {
                        self.engine.params.live_passthrough.store(false, Ordering::Relaxed);
                    }
                }

                ui.add_space(6.0);
                self.render_dsp_sliders(ui);

                ui.separator();
                ui.label(
                    egui::RichText::new(&self.status_message)
                        .small()
                        .color(egui::Color32::from_rgb(130, 215, 255)),
                );
            });

            // Right: User's Real-Voice Meeting Soundboard
            cols[1].group(|ui| {
                ui.heading("3. Soundboard Suara Asli Saya (Meeting 1-Klik)");
                ui.label(
                    egui::RichText::new(
                        "Rekam kalimat rapat memakai suara Anda sendiri (otomatis diperjelas DSP). Klik Putar saat meeting tanpa harus mengulang kalimat yang sama.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
                ui.add_space(6.0);

                let slot_count = self.settings.soundboard_slots.len();
                let idle = self.record_mode == RecordMode::None;
                let mut changed_titles = false;

                egui::ScrollArea::vertical().max_height(430.0).show(ui, |ui| {
                    for idx in 0..slot_count {
                        let wav_path = app_path(&self.settings.soundboard_slots[idx].wav_rel_path);
                        let has_file = wav_path.exists();
                        let is_rec_this = self.record_mode == RecordMode::SoundboardSlot(idx);

                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("#{}", idx + 1));
                                if ui
                                    .add(
                                        egui::TextEdit::singleline(
                                            &mut self.settings.soundboard_slots[idx].title,
                                        )
                                        .desired_width(205.0),
                                    )
                                    .changed()
                                {
                                    changed_titles = true;
                                }

                                if !is_rec_this {
                                    ui.add_enabled_ui(idle, |ui| {
                                        if ui.button("Rekam").clicked() {
                                            self.start_recording(
                                                RecordMode::SoundboardSlot(idx),
                                                None,
                                            );
                                        }
                                    });
                                } else if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("Simpan")
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(egui::Color32::from_rgb(190, 50, 50)),
                                    )
                                    .clicked()
                                {
                                    self.finish_recording();
                                }

                                ui.add_enabled_ui(has_file, |ui| {
                                    if ui.button("Putar ke Meeting").clicked()
                                        && self.ensure_audio_started()
                                    {
                                        match load_mono_wav(&wav_path) {
                                            Ok((samples, sr)) => {
                                                self.engine.queue_playback(&samples, sr);
                                                self.status_message = format!(
                                                    "Memutar soundboard \"{}\"...",
                                                    self.settings.soundboard_slots[idx].title
                                                );
                                            }
                                            Err(e) => self.status_message = e,
                                        }
                                    }
                                });
                            });
                        });
                        ui.add_space(3.0);
                    }
                });

                if changed_titles {
                    self.mark_settings_dirty();
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Hentikan Pemutaran").clicked() {
                        self.engine.stop_playback();
                    }
                    if self.engine.params.is_playing.load(Ordering::Relaxed) {
                        ui.label("Memutar…");
                    }
                });
            });
        });
    }

    fn render_studio_teleprompter_tab(&mut self, ui: &mut egui::Ui) {
        let idle = self.record_mode == RecordMode::None;
        let rec_scene = matches!(self.record_mode, RecordMode::TeleprompterScene(_));
        ui.columns(2, |cols| {
            // Left: Studio Recorder & Audio Comparison
            cols[0].group(|ui| {
                ui.heading("Studio Rekaman & Pembersih Audio");
                ui.add_space(6.0);

                let dur_secs = match (&self.recorder, self.record_mode) {
                    (Some(rec), RecordMode::Studio) => {
                        rec.captured_len() as f32 / self.record_sr.max(1) as f32
                    }
                    _ => self.studio_raw.len() as f32 / self.studio_sr.max(1) as f32,
                };
                ui.horizontal(|ui| {
                    let is_rec = self.record_mode == RecordMode::Studio;
                    if !is_rec {
                        ui.add_enabled_ui(idle, |ui| {
                            if ui.button("Mulai Rekam Suara").clicked() {
                                self.start_recording(RecordMode::Studio, None);
                            }
                        });
                    } else if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Selesai Rekam").color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(190, 50, 50)),
                        )
                        .clicked()
                    {
                        self.finish_recording();
                    }

                    ui.add_enabled_ui(self.pending_jobs == 0, |ui| {
                        if ui.button("Terapkan Ulang DSP").clicked() {
                            self.trigger_studio_dsp_worker(String::new());
                        }
                    });
                    ui.label(format!("Durasi: {dur_secs:.1} dtk"));
                });

                ui.add_space(6.0);
                self.render_dsp_sliders(ui);

                ui.separator();
                ui.label(format!(
                    "Keseimbangan Spektrum -> Sengau: {:.1} dB | Konsonan: {:+.1} dB | F0: {:.1} Hz",
                    self.studio_metrics.nasal_band_delta_db,
                    self.studio_metrics.consonant_band_delta_db,
                    self.studio_metrics.estimated_f0_hz
                ));
                if !self.studio_raw.is_empty()
                    && self.pending_jobs == 0
                    && self.studio_processed_cfg != Some(self.settings.dsp_cfg)
                {
                    ui.label(
                        egui::RichText::new(
                            "Hasil DSP belum sesuai slider — klik Terapkan Ulang DSP",
                        )
                        .small()
                        .color(egui::Color32::from_rgb(255, 200, 90)),
                    );
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!self.studio_raw.is_empty(), |ui| {
                        if ui.button("Putar Asli").clicked() && self.ensure_audio_started() {
                            self.engine.queue_playback(&self.studio_raw, self.studio_sr);
                        }
                    });
                    ui.add_enabled_ui(!self.studio_processed.is_empty(), |ui| {
                        if ui.button("Putar Hasil Jelas (DSP)").clicked()
                            && self.ensure_audio_started()
                        {
                            self.engine
                                .queue_playback(&self.studio_processed, self.studio_sr);
                        }
                    });
                    ui.add_enabled_ui(
                        !self.studio_raw.is_empty() && self.pending_jobs == 0,
                        |ui| {
                            if ui.button("Ekspor WAV Jelas").clicked() {
                                self.trigger_studio_export_worker();
                            }
                        },
                    );
                    if ui.button("Stop Putar").clicked() {
                        self.engine.stop_playback();
                    }
                    if self.engine.params.is_playing.load(Ordering::Relaxed) {
                        ui.label("Memutar…");
                    }
                });
            });

            // Right: Teleprompter & Multi-Scene Content Voiceover (100% User's Own Voice)
            cols[1].group(|ui| {
                ui.heading("Teleprompter & Voiceover Konten (Suara Asli)");
                ui.label(
                    egui::RichText::new(
                        "Tulis naskah per baris (scene), rekam suara Anda sambil membaca teleprompter, lalu gabungkan jadi satu master .WAV.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!rec_scene, |ui| {
                        if ui.button("Muat Naskah ke Daftar Scene").clicked() {
                            self.request_script_reload();
                        }
                    });
                    ui.label(format!("Total {} scene", self.scenes.len()));
                });

                if let Some(script) = self.pending_script.clone() {
                    let dropped = self
                        .dropped_scene_paths(&Self::script_texts(&script))
                        .len();
                    if dropped == 0 {
                        // Nothing to lose anymore (e.g. the scenes were re-recorded); just apply.
                        self.pending_script = None;
                        self.apply_script(&script);
                    } else {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 200, 90),
                                format!("{dropped} scene yang sudah direkam akan dihapus."),
                            );
                            ui.add_enabled_ui(!rec_scene, |ui| {
                                if ui.button("Ya, terapkan naskah baru").clicked() {
                                    self.pending_script = None;
                                    self.apply_script(&script);
                                }
                                if ui.button("Batal").clicked() {
                                    self.pending_script = None;
                                }
                            });
                        });
                    }
                }

                if ui
                    .add(
                        egui::TextEdit::multiline(&mut self.script_input)
                            .desired_rows(4)
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                {
                    self.settings.script_text = self.script_input.clone();
                    self.mark_settings_dirty();
                }

                ui.separator();
                if self.active_scene_idx >= self.scenes.len() {
                    self.active_scene_idx = 0;
                }
                let active_idx = self.active_scene_idx;

                ui.add_enabled_ui(!rec_scene, |ui| {
                    ui.horizontal(|ui| {
                        for idx in 0..self.scenes.len() {
                            let has_audio = !self.scenes[idx].recorded_audio.is_empty();
                            let label = if has_audio {
                                format!("Scene {} [OK]", idx + 1)
                            } else {
                                format!("Scene {}", idx + 1)
                            };
                            ui.selectable_value(&mut self.active_scene_idx, idx, label);
                        }
                    });
                });

                // Large readable Teleprompter card
                egui::Frame::group(ui.style())
                    .fill(egui::Color32::from_rgb(24, 29, 38))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(&self.scenes[active_idx].text)
                                .size(17.0)
                                .color(egui::Color32::WHITE),
                        );
                    });

                ui.add_space(4.0);
                let is_rec_scene = self.record_mode == RecordMode::TeleprompterScene(active_idx);
                ui.horizontal(|ui| {
                    if !is_rec_scene {
                        ui.add_enabled_ui(idle, |ui| {
                            if ui.button("Rekam Scene Ini").clicked() {
                                self.start_recording(
                                    RecordMode::TeleprompterScene(active_idx),
                                    None,
                                );
                            }
                        });
                    } else if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Selesai Rekam Scene")
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(190, 50, 50)),
                        )
                        .clicked()
                    {
                        self.finish_recording();
                    }

                    let has_rec = !self.scenes[active_idx].recorded_audio.is_empty();
                    ui.add_enabled_ui(has_rec, |ui| {
                        if ui.button("Putar Scene").clicked() && self.ensure_audio_started() {
                            let audio = self.scenes[active_idx].recorded_audio.clone();
                            let sr = self.scenes[active_idx].sample_rate;
                            self.engine.queue_playback(&audio, sr);
                        }
                    });
                    if self.engine.params.is_playing.load(Ordering::Relaxed) {
                        ui.label("Memutar…");
                    }
                });

                ui.separator();
                if ui
                    .add(
                        egui::Slider::new(&mut self.settings.voiceover_pause_ms, 100..=1500)
                            .text("Jeda Antar Scene (ms)"),
                    )
                    .changed()
                {
                    self.mark_settings_dirty();
                }

                let any_recorded = self.scenes.iter().any(|s| !s.recorded_audio.is_empty());
                ui.add_enabled_ui(any_recorded && self.pending_jobs == 0, |ui| {
                    if ui.button("Gabung & Ekspor Semua Scene (.WAV)").clicked() {
                        let pause_ms = self.settings.voiceover_pause_ms;
                        let target_sr = 48000_u32;
                        let scenes_data: Vec<(Vec<f32>, u32)> = self
                            .scenes
                            .iter()
                            .filter(|s| !s.recorded_audio.is_empty())
                            .map(|s| (s.recorded_audio.clone(), s.sample_rate))
                            .collect();
                        let stamp = Self::unique_stamp();
                        let out_path = app_path(&format!("output/voiceover_master_{stamp}.wav"));
                        self.spawn_job(move || {
                            let pause_samples = (target_sr as u64 * pause_ms as u64 / 1000) as usize;
                            let mut master = Vec::new();
                            for (audio, sr) in &scenes_data {
                                let resampled = dsp::resample_batch(audio, *sr, target_sr);
                                master.extend_from_slice(&resampled);
                                master.extend(std::iter::repeat(0.0_f32).take(pause_samples));
                            }
                            let msg = match save_mono_wav(&out_path, &master, target_sr) {
                                Ok(_) => format!(
                                    "Master Voiceover disimpan ({} scene, {:.1} dtk): {}",
                                    scenes_data.len(),
                                    master.len() as f32 / target_sr as f32,
                                    out_path.display()
                                ),
                                Err(e) => format!("Gagal ekspor master: {e}"),
                            };
                            WorkerResult::ExportFinished(msg)
                        });
                    }
                });
            });
        });
    }

    fn render_calibration_preset_tab(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |cols| {
            cols[0].group(|ui| {
                ui.heading("Auto-Kalibrasi Akustik Pribadi (5 Detik)");
                ui.label(
                    egui::RichText::new(
                        "Rekam 5 detik suara alami Anda (bacalah kalimat panjang apa saja). Voxara otomatis mencari frekuensi puncak dengung sengau (215–375 Hz) dan defisit konsonan Anda.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
                ui.add_space(8.0);

                if self.record_mode == RecordMode::Calibration {
                    ui.label(
                        egui::RichText::new("Sedang merekam 5 detik... Silakan berbicara.")
                            .strong()
                            .color(egui::Color32::YELLOW),
                    );
                } else {
                    let idle = self.record_mode == RecordMode::None;
                    ui.add_enabled_ui(idle, |ui| {
                        if ui.button("Mulai Auto-Kalibrasi (5 Detik)").clicked() {
                            self.start_recording(RecordMode::Calibration, Some(5));
                        }
                    });
                }

                ui.add_space(8.0);
                ui.label(format!(
                    "Status Kalibrasi: {}",
                    if self.settings.calibrated {
                        "Terkalibrasi"
                    } else {
                        "Default (Belum dikalibrasi)"
                    }
                ));
                ui.label(format!(
                    "Frekuensi Sengau Terdeteksi: {:.0} Hz | Pitch Dasar F0: {:.1} Hz",
                    self.settings.dsp_cfg.nasal_freq_hz, self.settings.calibrated_f0_hz
                ));

                ui.add_space(8.0);
                ui.label(egui::RichText::new("Profil Spektrum Vokal 8-Pita:").small());
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(24, 27, 34));
                let bar_w = rect.width() / 8.0;
                for (i, &db) in self.settings.band_spectrum_db.iter().enumerate() {
                    let norm = ((db + 10.0) / 20.0).clamp(0.05, 1.0);
                    let h = rect.height() * 0.72 * norm;
                    let x = rect.min.x + i as f32 * bar_w + 4.0;
                    let y = rect.max.y - h - 16.0;
                    painter.rect_filled(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(bar_w - 8.0, h)),
                        2.0,
                        egui::Color32::from_rgb(70, 175, 255),
                    );
                    painter.text(
                        egui::pos2(x + (bar_w - 8.0) * 0.5, rect.max.y - 3.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{}Hz", BAND_CENTERS_HZ[i] as u32),
                        egui::FontId::proportional(9.5),
                        egui::Color32::LIGHT_GRAY,
                    );
                }
            });

            cols[1].group(|ui| {
                ui.heading("Preset Cepat & Penjelasan Parameter");
                ui.add_space(6.0);

                if ui.button("Preset 1: Kalimat Panjang & Presentasi (Seimbang)").clicked() {
                    self.settings.dsp_cfg = DspConfig {
                        denasal_strength: 0.78,
                        nasal_freq_hz: self.settings.dsp_cfg.nasal_freq_hz,
                        oral_formant_boost: 0.75,
                        consonant_boost: 0.78,
                        deesser_strength: 0.65,
                        air_leak_gate: 0.35,
                        breath_compensator: 0.85,
                        output_gain_db: 2.0,
                    };
                    self.sync_dsp_to_engine();
                    self.status_message = "Preset Kalimat Panjang diterapkan.".to_string();
                }
                if ui.button("Preset 2: Redam Sengau Kuat").clicked() {
                    self.settings.dsp_cfg = DspConfig {
                        denasal_strength: 0.94,
                        nasal_freq_hz: self.settings.dsp_cfg.nasal_freq_hz,
                        oral_formant_boost: 0.85,
                        consonant_boost: 0.82,
                        deesser_strength: 0.65,
                        air_leak_gate: 0.50,
                        breath_compensator: 0.75,
                        output_gain_db: 2.5,
                    };
                    self.sync_dsp_to_engine();
                    self.status_message = "Preset Redam Sengau Kuat diterapkan.".to_string();
                }
                if ui.button("Preset 3: Penegas Konsonan P/T/K/S").clicked() {
                    self.settings.dsp_cfg = DspConfig {
                        denasal_strength: 0.75,
                        nasal_freq_hz: self.settings.dsp_cfg.nasal_freq_hz,
                        oral_formant_boost: 0.70,
                        consonant_boost: 0.95,
                        deesser_strength: 0.75,
                        air_leak_gate: 0.40,
                        breath_compensator: 0.80,
                        output_gain_db: 2.0,
                    };
                    self.sync_dsp_to_engine();
                    self.status_message = "Preset Penegas Konsonan diterapkan.".to_string();
                }
                if ui.button("Reset ke Default").clicked() {
                    self.settings.dsp_cfg = DspConfig::default();
                    self.sync_dsp_to_engine();
                    self.status_message = "Pengaturan DSP dikembalikan ke default.".to_string();
                }

                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "Catatan Realistis:\n• Filter DSP meredam dengung hidung dan mengangkat transien konsonan serta volume napas kalimat panjang secara real-time.\n• Untuk huruf tekanan penuh yang memang hilang secara fisik, bicaralah lebih dekat ke mikrofon (8–12 cm) sambil mengaktifkan Penstabil Napas & Penguat Konsonan.",
                    )
                    .small()
                    .color(egui::Color32::LIGHT_GRAY),
                );
            });
        });
    }
}

impl eframe::App for VoxaraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(deadline) = self.record_deadline {
            if Instant::now() >= deadline {
                self.finish_recording();
            }
        }

        while let Ok(res) = self.worker_rx.try_recv() {
            self.pending_jobs = self.pending_jobs.saturating_sub(1);
            match res {
                WorkerResult::StudioProcessed {
                    processed,
                    metrics,
                    cfg,
                    gen,
                    status,
                } => {
                    // Ignore audio computed from a take that has since been replaced.
                    if gen == self.studio_gen {
                        self.studio_processed = processed;
                        self.studio_metrics = metrics;
                        self.studio_processed_cfg = Some(cfg);
                    }
                    self.status_message = status;
                }
                WorkerResult::ExportFinished(msg)
                | WorkerResult::SlotSaved(msg)
                | WorkerResult::JobFailed(msg) => {
                    self.status_message = msg;
                }
                WorkerResult::Calibrated {
                    cfg,
                    f0,
                    bands,
                    status,
                } => {
                    self.settings.dsp_cfg = cfg;
                    self.settings.calibrated_f0_hz = f0;
                    self.settings.band_spectrum_db = bands;
                    self.settings.calibrated = true;
                    self.engine.params.store_dsp_config(cfg);
                    // Set the status first so a save error (if any) stays visible.
                    self.status_message = status;
                    self.save_settings_checked();
                }
                WorkerResult::SceneProcessed {
                    key,
                    audio,
                    sr,
                    msg,
                } => {
                    // Scenes with identical text share one key (and one WAV file).
                    for scene in self.scenes.iter_mut().filter(|s| s.key == key) {
                        scene.recorded_audio = audio.clone();
                        scene.sample_rate = sr;
                    }
                    self.status_message = msg;
                }
            }
        }

        if self.settings_dirty {
            let elapsed = self.last_settings_change.elapsed();
            if elapsed >= SETTINGS_SAVE_DEBOUNCE {
                self.save_settings_checked();
            } else {
                ctx.request_repaint_after(SETTINGS_SAVE_DEBOUNCE - elapsed);
            }
        }

        let in_lvl = AtomicAudioParams::load_f32(&self.engine.params.input_level);
        let out_lvl = AtomicAudioParams::load_f32(&self.engine.params.output_level);
        let lat_ms = AtomicAudioParams::load_f32(&self.engine.params.measured_latency_ms);

        if self.engine.is_running.load(Ordering::Relaxed)
            || self.record_mode != RecordMode::None
            || self.pending_jobs > 0
        {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        egui::TopBottomPanel::top("voxara_header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(
                    egui::RichText::new("VOXARA STUDIO")
                        .strong()
                        .color(egui::Color32::from_rgb(100, 200, 255)),
                );
                let lat_text = if self.engine.is_running.load(Ordering::Relaxed) {
                    if self.engine.params.latency_measured.load(Ordering::Relaxed) {
                        format!("Latensi Terukur: ~{lat_ms:.1} ms (WASAPI)")
                    } else {
                        format!("Latensi Perkiraan: ~{lat_ms:.1} ms")
                    }
                } else {
                    "Audio Nonaktif (Pure Rust Engine)".to_string()
                };
                ui.label(egui::RichText::new(lat_text).small().color(egui::Color32::GRAY));
                ui.separator();

                Self::draw_level_bar(ui, "Mic", in_lvl, egui::Color32::from_rgb(65, 210, 130));
                Self::draw_level_bar(
                    ui,
                    "Out",
                    out_lvl,
                    egui::Color32::from_rgb(80, 175, 255),
                );
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.active_tab,
                    ActiveTab::MegafonMeeting,
                    "Megafon Live & Meeting",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    ActiveTab::StudioTeleprompter,
                    "Studio Rekaman & Voiceover Konten",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    ActiveTab::CalibrationPreset,
                    "Auto-Kalibrasi & Preset",
                );
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            ActiveTab::MegafonMeeting => self.render_megafon_tab(ui),
            ActiveTab::StudioTeleprompter => self.render_studio_teleprompter_tab(ui),
            ActiveTab::CalibrationPreset => self.render_calibration_preset_tab(ui),
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.settings_dirty {
            self.save_settings_checked();
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 680.0])
            .with_min_inner_size([880.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Voxara Studio — Real-Time Voice & Articulation Engine",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(VoxaraApp::new(cc)))
        }),
    )
}
