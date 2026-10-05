use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "auto" (Gemini, falling back to Google Translate), "gemini" or "google".
    pub engine: String,
    /// Gemini model id, see https://ai.google.dev/gemini-api/docs/models
    pub model: String,
    /// Gemini "thinking" level ("minimal", "low", "high"); empty leaves the model default.
    /// Translation needs no reasoning, and thinking adds seconds of latency.
    pub gemini_thinking: String,
    /// How long to wait for a Gemini text translation before falling back to Google.
    pub gemini_timeout_ms: u64,
    /// Explicit proxy for Gemini requests, e.g. "http://127.0.0.1:8080".
    /// Empty means the Windows system proxy is used.
    pub proxy: String,
    /// Language the translation is produced in (free-form, goes into the Gemini prompt).
    pub target_language: String,
    /// Same language as an ISO code for Google Translate.
    pub target_code: String,
    /// BCP-47 tag of the Windows OCR language pack to use.
    pub ocr_language: String,
    /// How often the region is captured in live mode.
    pub poll_ms: u64,
    /// Share of sampled pixels that must change for a frame to count as "changed".
    pub change_threshold: f32,
    pub font_size: i32,
    /// Overlay opacity, 0-255.
    pub opacity: u8,
    /// How many recent translations the overlay feed keeps.
    pub history_size: usize,
    /// Overlay height limit as a share of the monitor height; older entries are dropped to fit.
    pub max_height_percent: i32,
    pub region: Option<Region>,
    /// Overlay position pinned with Ctrl+Alt+W; `h` is the height limit. Absent means the
    /// overlay follows the capture region.
    pub overlay_rect: Option<Region>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: "auto".into(),
            model: "gemini-flash-lite-latest".into(),
            gemini_thinking: "minimal".into(),
            gemini_timeout_ms: 6000,
            proxy: String::new(),
            target_language: "Russian".into(),
            target_code: "ru".into(),
            ocr_language: "en-US".into(),
            poll_ms: 400,
            change_threshold: 0.003,
            font_size: 22,
            opacity: 225,
            history_size: 5,
            max_height_percent: 45,
            region: None,
            overlay_rect: None,
        }
    }
}

fn path() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = path()?;
    if !path.exists() {
        let cfg = Config::default();
        save(&cfg)?;
        return Ok(cfg);
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if toml::to_string_pretty(&cfg)? != text {
        save(&cfg)?;
    }
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = path()?;
    std::fs::write(&path, toml::to_string_pretty(cfg)?).with_context(|| format!("write {}", path.display()))
}
