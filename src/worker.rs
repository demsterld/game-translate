use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

use crate::capture::{self, Frame};
use crate::config::{Config, Region};
use crate::ocr::Ocr;
use crate::text::Entry;
use crate::translate::Translator;

pub const WM_WORKER: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

const CONTEXT_LINES: usize = 4;
const CACHE_LIMIT: usize = 1000;
const STABLE_TICKS: u32 = 2;
const MIN_LETTERS: usize = 3;

pub enum Cmd {
    SetRegion(Region),
    ToggleLive,
    Once,
    Vision,
}

pub enum Event {
    Translation(Entry),
    /// Short-lived service message.
    Status(String),
    /// Stays on screen until the next translation.
    Error(String),
    Live(bool),
}

pub fn spawn(cfg: Config, ui_thread: u32) -> (Sender<Cmd>, Receiver<Event>) {
    let (cmd_tx, cmd_rx) = channel();
    let (ev_tx, ev_rx) = channel();
    std::thread::spawn(move || {
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        Worker::new(cfg, Ui { tx: ev_tx, thread: ui_thread }).run(cmd_rx);
    });
    (cmd_tx, ev_rx)
}

struct Ui {
    tx: Sender<Event>,
    thread: u32,
}

impl Ui {
    fn send(&self, ev: Event) {
        if self.tx.send(ev).is_ok() {
            let _ = unsafe { PostThreadMessageW(self.thread, WM_WORKER, WPARAM(0), LPARAM(0)) };
        }
    }
}

struct Worker {
    cfg: Config,
    ui: Ui,
    ocr: Result<Ocr, String>,
    translator: Result<Translator, String>,
    region: Option<Region>,
    live: bool,
    last_sig: Vec<u8>,
    last_ocr: Vec<String>,
    candidate: Vec<String>,
    stable_ticks: u32,
    last_text: String,
    context: VecDeque<String>,
    cache: HashMap<String, Entry>,
}

impl Worker {
    fn new(cfg: Config, ui: Ui) -> Self {
        let ocr = Ocr::new(&cfg.ocr_language).map_err(|e| e.to_string());
        let translator = Translator::new(&cfg).map_err(|e| format!("{e:#}"));
        Self {
            region: cfg.region,
            cfg,
            ui,
            ocr,
            translator,
            live: false,
            last_sig: Vec::new(),
            last_ocr: Vec::new(),
            candidate: Vec::new(),
            stable_ticks: 0,
            last_text: String::new(),
            context: VecDeque::new(),
            cache: HashMap::new(),
        }
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        let poll = Duration::from_millis(self.cfg.poll_ms.max(100));
        loop {
            let cmd = if self.live {
                match rx.recv_timeout(poll) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return,
                }
            };

            let result = match cmd {
                Some(cmd) => self.handle(cmd),
                None => self.tick(),
            };
            if let Err(e) = result {
                crate::log::write(format_args!("error: {e:#}"));
                self.ui.send(Event::Error(format!("Ошибка: {e:#}")));
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::SetRegion(r) => {
                self.region = Some(r);
                self.reset_change_tracking();
            }
            Cmd::ToggleLive => {
                self.region()?;
                self.live = !self.live;
                self.reset_change_tracking();
                self.ui.send(Event::Live(self.live));
            }
            Cmd::Once => {
                let frame = capture::capture(self.region()?)?;
                let paragraphs = self.ocr()?.recognize(&frame)?;
                if paragraphs.is_empty() {
                    self.ui.send(Event::Status("Текст не найден. Попробуй Ctrl+Alt+G (через Gemini Vision)".into()));
                    return Ok(());
                }
                self.translate(&paragraphs)?;
            }
            Cmd::Vision => {
                let frame = capture::capture(self.region()?)?;
                self.ui.send(Event::Status("Gemini Vision: перевожу...".into()));
                let png = capture::to_png(&frame)?;
                let translation = self.translator()?.translate_image(&png)?;
                self.ui.send(Event::Translation(translation));
            }
        }
        Ok(())
    }

    /// Live mode. Game backgrounds are usually animated, so stability is judged by the recognized
    /// text rather than by pixels: a line is translated once it reads the same on `STABLE_TICKS`
    /// consecutive polls (this also skips typewriter animations) and differs from the last one.
    fn tick(&mut self) -> Result<()> {
        let frame = capture::capture(self.region()?)?;
        let paragraphs = self.recognize_live(&frame)?;
        let text = paragraphs.join("\n");

        if text.chars().filter(|c| c.is_alphabetic()).count() < MIN_LETTERS {
            self.candidate.clear();
            self.stable_ticks = 0;
            return Ok(());
        }

        if same_text(&text, &self.candidate.join("\n")) {
            self.stable_ticks += 1;
        } else {
            crate::log::write(format_args!("ocr: {paragraphs:?}"));
            self.stable_ticks = 1;
        }
        self.candidate = paragraphs;

        if self.stable_ticks >= STABLE_TICKS && !same_text(&text, &self.last_text) {
            self.translate(&self.candidate.clone())?;
        }
        Ok(())
    }

    /// Skips OCR when the frame is pixel-identical to the previous one.
    fn recognize_live(&mut self, frame: &Frame) -> Result<Vec<String>> {
        let sig = capture::signature(frame);
        let unchanged =
            !self.last_sig.is_empty() && capture::difference(&sig, &self.last_sig) <= self.cfg.change_threshold;
        self.last_sig = sig;
        if !unchanged {
            self.last_ocr = self.ocr()?.recognize(frame)?;
        }
        Ok(self.last_ocr.clone())
    }

    fn translate(&mut self, paragraphs: &[String]) -> Result<()> {
        let src = Entry::from_paragraphs(paragraphs);
        let text = src.key();
        crate::log::write(format_args!("translate: {src:?}"));
        self.last_text = text.clone();
        let translation = match self.cache.get(&text) {
            Some(t) => t.clone(),
            None => {
                let context: Vec<String> = self.context.iter().cloned().collect();
                let t = self.translator()?.translate_entry(&src, &context)?;
                if self.cache.len() >= CACHE_LIMIT {
                    self.cache.clear();
                }
                self.cache.insert(text.clone(), t.clone());
                t
            }
        };

        if self.context.back() != Some(&text) {
            self.context.push_back(text);
            if self.context.len() > CONTEXT_LINES {
                self.context.pop_front();
            }
        }
        self.ui.send(Event::Translation(translation));
        Ok(())
    }

    fn reset_change_tracking(&mut self) {
        self.last_sig.clear();
        self.last_ocr.clear();
        self.candidate.clear();
        self.stable_ticks = 0;
        self.last_text.clear();
    }

    fn region(&self) -> Result<Region> {
        self.region.ok_or_else(|| anyhow!("сначала выбери область: Ctrl+Alt+R"))
    }

    fn ocr(&self) -> Result<&Ocr> {
        self.ocr.as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn translator(&self) -> Result<&Translator> {
        self.translator.as_ref().map_err(|e| anyhow!("{e}"))
    }
}

/// OCR over an animated background flickers by a character or two between polls, while a
/// typewriter animation grows the text; only the former counts as "the same text".
fn same_text(a: &str, b: &str) -> bool {
    let key = |s: &str| -> Vec<char> { s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect() };
    let (a, b) = (key(a), key(b));
    if a == b {
        return true;
    }
    if a.is_empty() || b.is_empty() || a.len().abs_diff(b.len()) > 2 {
        return false;
    }
    let max = a.len().max(b.len());
    levenshtein(&a, &b) * 10 <= max
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::same_text;

    #[test]
    fn ocr_flicker_is_same_text() {
        assert!(same_text(
            "Join the crowd, toward the kingdom rising in the dark waves...",
            "Join the crowd, toward'the kingdom rising in the dark wpves..."
        ));
    }

    #[test]
    fn growing_text_is_different() {
        assert!(!same_text("Join the crowd, toward the", "Join the crowd, toward the kingdom"));
        assert!(!same_text("Who dares to enter my", "Who dares to enter my domain?"));
    }

    #[test]
    fn different_lines_are_different() {
        assert!(!same_text("Welcome, traveler.", "Leave this place at once!"));
    }
}
