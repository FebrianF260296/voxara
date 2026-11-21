use crate::dsp::DspConfig;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const SETTINGS_REL_PATH: &str = "data/app_settings.json";

/// Resolves relative paths alongside `voxara.exe` so launching from any working directory works cleanly.
pub fn app_base_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // If running via `cargo run` inside target/debug or target/release, walk back to project root if `Cargo.toml` exists
            let candidate = parent.to_path_buf();
            if candidate.join("Cargo.toml").exists() || !candidate.ends_with("debug") && !candidate.ends_with("release") {
                return candidate;
            }
            if let Some(grand) = candidate.parent().and_then(|p| p.parent()) {
                if grand.join("Cargo.toml").exists() {
                    return grand.to_path_buf();
                }
            }
            return candidate;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn app_path(rel: &str) -> PathBuf {
    app_base_dir().join(rel)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundboardSlot {
    pub title: String,
    pub wav_rel_path: String,
}

/// Default teleprompter script shown on first launch.
pub const DEFAULT_SCRIPT: &str = "Halo semuanya, hari ini saya ingin menjelaskan progres terbaru proyek kita.\nBagian pertama sudah berjalan lancar dan sekarang kita masuk ke tahap pengujian akhir.\nTerima kasih atas perhatiannya, silakan beri masukan jika ada hal yang perlu diperbaiki.";

/// Persisted settings. Struct-level `serde(default)` lets files written by older
/// versions (missing newer fields) still load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub dsp_cfg: DspConfig,
    pub calibrated_f0_hz: f32,
    pub band_spectrum_db: [f32; 8],
    pub calibrated: bool,
    pub selected_input_name: String,
    pub selected_output_name: String,
    pub voiceover_pause_ms: u32,
    pub soundboard_slots: Vec<SoundboardSlot>,
    pub script_text: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            dsp_cfg: DspConfig::default(),
            calibrated_f0_hz: 145.0,
            band_spectrum_db: [0.0; 8],
            calibrated: false,
            selected_input_name: String::new(),
            selected_output_name: String::new(),
            voiceover_pause_ms: 400,
            soundboard_slots: vec![
                SoundboardSlot {
                    title: "Salam Pembuka Meeting".to_string(),
                    wav_rel_path: "data/slot_1.wav".to_string(),
                },
                SoundboardSlot {
                    title: "Saya Setuju / Lanjut".to_string(),
                    wav_rel_path: "data/slot_2.wav".to_string(),
                },
                SoundboardSlot {
                    title: "Mohon Tunggu Sebentar".to_string(),
                    wav_rel_path: "data/slot_3.wav".to_string(),
                },
                SoundboardSlot {
                    title: "Bisa Tolong Diulangi?".to_string(),
                    wav_rel_path: "data/slot_4.wav".to_string(),
                },
                SoundboardSlot {
                    title: "Izin Menambahkan Poin".to_string(),
                    wav_rel_path: "data/slot_5.wav".to_string(),
                },
                SoundboardSlot {
                    title: "Terima Kasih Banyak".to_string(),
                    wav_rel_path: "data/slot_6.wav".to_string(),
                },
            ],
            script_text: DEFAULT_SCRIPT.to_string(),
        }
    }
}

/// FNV-1a 64-bit hash over the UTF-8 bytes of `text`.
pub fn fnv1a64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Relative path of the cached WAV for a teleprompter scene with the given text.
pub fn scene_wav_rel_path(text: &str) -> String {
    format!("data/scenes/{:016x}.wav", fnv1a64(text.trim()))
}

/// Appends `suffix` to the full file name (e.g. `app_settings.json` -> `app_settings.json.tmp`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

impl AppSettings {
    /// Loads settings from the default location. Returns the settings and an optional
    /// user-facing warning (set when a corrupt file was backed up and defaults were used).
    pub fn load() -> (Self, Option<String>) {
        Self::load_from(&app_path(SETTINGS_REL_PATH))
    }

    /// Loads settings from `path`. Missing file gives defaults and no warning. A file that
    /// exists but cannot be parsed is renamed to `<path>.bak` (replacing an older backup)
    /// and defaults are returned together with a warning.
    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("Pengaturan tidak bisa dibaca ({e}), memakai nilai default.")),
                )
            }
        };
        match serde_json::from_str::<AppSettings>(&text) {
            Ok(cfg) => (cfg, None),
            Err(_) => {
                let bak = with_suffix(path, ".bak");
                let _ = fs::remove_file(&bak);
                let warning = match fs::rename(path, &bak) {
                    Ok(()) => format!(
                        "Pengaturan rusak, dicadangkan ke {SETTINGS_REL_PATH}.bak dan dikembalikan ke default."
                    ),
                    Err(e) => format!(
                        "Pengaturan rusak dan gagal dicadangkan ({e}); dikembalikan ke default."
                    ),
                };
                (Self::default(), Some(warning))
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&app_path(SETTINGS_REL_PATH))
    }

    /// Atomic save: writes `<path>.tmp` first, then renames it over `path`.
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Gagal membuat folder data: {e}"))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = with_suffix(path, ".tmp");
        fs::write(&tmp, json).map_err(|e| format!("Gagal menyimpan pengaturan: {e}"))?;
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Gagal menyimpan pengaturan: {e}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("voxara_vp_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fnv1a64_known_vectors() {
        assert_eq!(fnv1a64(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64("a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn scene_path_uses_trimmed_text() {
        assert_eq!(scene_wav_rel_path("  a \n"), "data/scenes/af63dc4c8601ec8c.wav");
    }

    #[test]
    fn deserializes_without_newer_fields() {
        let mut v = serde_json::to_value(AppSettings::default()).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("script_text");
        obj.remove("soundboard_slots");
        obj.insert("calibrated_f0_hz".into(), serde_json::json!(180.0));
        let cfg: AppSettings = serde_json::from_value(v).unwrap();
        let def = AppSettings::default();
        assert_eq!(cfg.script_text, def.script_text);
        assert_eq!(cfg.soundboard_slots, def.soundboard_slots);
        assert_eq!(cfg.calibrated_f0_hz, 180.0);
    }

    #[test]
    fn save_load_round_trip_is_atomic() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("data").join("app_settings.json");
        let mut s = AppSettings::default();
        s.script_text = "Naskah uji".to_string();
        s.voiceover_pause_ms = 777;
        s.save_to(&path).unwrap();
        assert!(!with_suffix(&path, ".tmp").exists());
        // Saving again must replace the existing file.
        s.voiceover_pause_ms = 778;
        s.save_to(&path).unwrap();
        assert!(!with_suffix(&path, ".tmp").exists());
        let (loaded, warn) = AppSettings::load_from(&path);
        assert!(warn.is_none());
        assert_eq!(loaded, s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_gives_defaults_without_warning() {
        let dir = temp_dir("missing");
        let (cfg, warn) = AppSettings::load_from(&dir.join("nope.json"));
        assert_eq!(cfg, AppSettings::default());
        assert!(warn.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let dir = temp_dir("corrupt");
        let path = dir.join("app_settings.json");
        let bak = with_suffix(&path, ".bak");
        fs::write(&bak, "old backup").unwrap();
        fs::write(&path, "{ not json").unwrap();
        let (cfg, warn) = AppSettings::load_from(&path);
        assert_eq!(cfg, AppSettings::default());
        assert!(warn.unwrap().contains("rusak"));
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(&bak).unwrap(), "{ not json");
        let _ = fs::remove_dir_all(&dir);
    }
}
