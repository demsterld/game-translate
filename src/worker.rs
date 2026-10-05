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
/// How many recent lines are remembered to recognize a line that OCR reads again.
const RECENT_PHRASES: usize = 8;
/// Shortest start of a line (in letters and digits) that counts as "the same line, cut off".
const MIN_PREFIX: usize = 12;

pub enum Cmd {
    SetRegion(Region),
    ToggleLive,
    Once,
    Vision,
}

pub enum Event {
    /// A new line: appended to the feed.
    Translation(Entry),
    /// A better reading of the line already at the bottom of the feed: replaces it.
    Update(Entry),
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
    recent: VecDeque<Phrase>,
    /// Whether the bottom of the feed shows `recent.back()` and may be updated in place.
    last_is_live: bool,
}

/// A line of dialogue shown in the feed, identified by its text without the speaker name.
struct Phrase {
    body: Vec<char>,
    heading: Option<String>,
    requests: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    /// Same line, OCR noise aside.
    Same,
    /// The line got longer: a typewriter animation finished or more text became readable.
    Grown,
    /// Only the beginning of the line was read.
    Partial,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    New,
    /// Re-translate and replace the line at this index of `recent`.
    Update(usize),
    Skip,
}

fn decide(recent: &VecDeque<Phrase>, last_is_live: bool, body: &[char], heading: Option<&str>, max_requests: u32) -> Decision {
    let found = recent.iter().enumerate().rev().find_map(|(i, p)| compare(body, &p.body).map(|m| (i, m)));
    let Some((i, m)) = found else { return Decision::New };
    // An older line that reappears (OCR picked up a fading frame) is already in the feed.
    if !last_is_live || i + 1 != recent.len() {
        return Decision::Skip;
    }
    let p = &recent[i];
    let better = match m {
        Match::Grown => true,
        Match::Same => {
            let heading_changed = heading.is_some_and(|h| p.heading.as_deref() != Some(h));
            (heading_changed || body.len() > p.body.len()) && p.requests < max_requests
        }
        Match::Partial => false,
    };
    if better { Decision::Update(i) } else { Decision::Skip }
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
            recent: VecDeque::new(),
            last_is_live: false,
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
                let src = Entry::from_paragraphs(&paragraphs);
                self.last_text = src.key();
                self.translate_new(src)?;
            }
            Cmd::Vision => {
                let frame = capture::capture(self.region()?)?;
                self.ui.send(Event::Status("Gemini Vision: перевожу...".into()));
                let png = capture::to_png(&frame)?;
                let translation = self.translator()?.translate_image(&png)?;
                self.last_is_live = false;
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
            self.translate_live(&self.candidate.clone())?;
        }
        Ok(())
    }

    /// OCR of the same dialogue box alternates between readings (with and without the speaker
    /// name, with a letter lost), so the text is matched against recent lines first: a new line
    /// is appended, a better reading of the current one replaces it, anything else is ignored.
    fn translate_live(&mut self, paragraphs: &[String]) -> Result<()> {
        let src = Entry::from_paragraphs(paragraphs);
        self.last_text = src.key();
        let body = key(&src.body());
        match decide(&self.recent, self.last_is_live, &body, src.heading(), self.cfg.max_requests_per_phrase) {
            Decision::New => self.translate_new(src),
            Decision::Skip => {
                crate::log::write(format_args!("skip, already shown: {src:?}"));
                Ok(())
            }
            Decision::Update(i) => {
                let src = match (src.heading(), &self.recent[i].heading) {
                    (None, Some(h)) => src.with_heading(h),
                    _ => src,
                };
                let (translation, requested) = self.lookup(&src)?;
                let phrase = &mut self.recent[i];
                if body.len() >= phrase.body.len() {
                    phrase.body = body;
                }
                if let Some(h) = src.heading() {
                    phrase.heading = Some(h.to_string());
                }
                phrase.requests += u32::from(requested);
                self.context.pop_back();
                self.push_context(src.key());
                self.ui.send(Event::Update(translation));
                Ok(())
            }
        }
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

    /// Translates `src` and appends it to the feed as a new line.
    fn translate_new(&mut self, src: Entry) -> Result<()> {
        let (translation, requested) = self.lookup(&src)?;
        self.recent.push_back(Phrase {
            body: key(&src.body()),
            heading: src.heading().map(str::to_string),
            requests: u32::from(requested),
        });
        if self.recent.len() > RECENT_PHRASES {
            self.recent.pop_front();
        }
        self.last_is_live = true;
        self.push_context(src.key());
        self.ui.send(Event::Translation(translation));
        Ok(())
    }

    /// Returns the translation and whether the translator had to be called for it.
    fn lookup(&mut self, src: &Entry) -> Result<(Entry, bool)> {
        let text = src.key();
        if let Some(t) = self.cache.get(&text) {
            return Ok((t.clone(), false));
        }
        crate::log::write(format_args!("translate: {src:?}"));
        let context: Vec<String> = self.context.iter().cloned().collect();
        let t = self.translator()?.translate_entry(src, &context)?;
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear();
        }
        self.cache.insert(text, t.clone());
        Ok((t, true))
    }

    fn push_context(&mut self, text: String) {
        if self.context.back() != Some(&text) {
            self.context.push_back(text);
            if self.context.len() > CONTEXT_LINES {
                self.context.pop_front();
            }
        }
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
    same_keys(&key(a), &key(b))
}

/// Letters and digits only, lowercased: what is left after OCR noise in punctuation and spacing.
fn key(s: &str) -> Vec<char> {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn same_keys(a: &[char], b: &[char]) -> bool {
    if a == b {
        return true;
    }
    if a.is_empty() || b.is_empty() || a.len().abs_diff(b.len()) > 2 {
        return false;
    }
    let max = a.len().max(b.len());
    levenshtein(a, b) * 10 <= max
}

fn compare(new: &[char], old: &[char]) -> Option<Match> {
    if same_keys(new, old) {
        Some(Match::Same)
    } else if new.len() > old.len() && similar_prefix(old, new) {
        Some(Match::Grown)
    } else if new.len() < old.len() && similar_prefix(new, old) {
        Some(Match::Partial)
    } else {
        None
    }
}

/// `short` reads like the beginning of `long`, allowing a few OCR misreads.
fn similar_prefix(short: &[char], long: &[char]) -> bool {
    short.len() >= MIN_PREFIX && levenshtein(short, &long[..short.len()]) * 6 <= short.len()
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
    use super::*;

    /// Feeds OCR readings through `decide` the way `translate_live` does and returns how many
    /// feed entries were created and how many translation requests were made.
    fn replay(readings: &[&[&str]], max_requests: u32) -> (usize, u32) {
        let mut recent = VecDeque::new();
        let (mut entries, mut requests) = (0, 0);
        for reading in readings {
            let paragraphs: Vec<String> = reading.iter().map(|s| s.to_string()).collect();
            let src = Entry::from_paragraphs(&paragraphs);
            let body = key(&src.body());
            match decide(&recent, true, &body, src.heading(), max_requests) {
                Decision::New => {
                    entries += 1;
                    requests += 1;
                    recent.push_back(Phrase { body, heading: src.heading().map(str::to_string), requests: 1 });
                }
                Decision::Update(i) => {
                    requests += 1;
                    let p: &mut Phrase = &mut recent[i];
                    if body.len() >= p.body.len() {
                        p.body = body;
                    }
                    if let Some(h) = src.heading() {
                        p.heading = Some(h.to_string());
                    }
                    p.requests += 1;
                }
                Decision::Skip => {}
            }
        }
        (entries, requests)
    }

    const LEVIATHAN: &[&[&str]] = &[
        &["Chimera", "Leviathan hides Its message in another dimension."],
        &["Leviathan hides Its message in another dimension."],
        &["e*Chimera", "Leviathan hides Its message in another dimension."],
        &["Leviathan hides Its message in another dimension."],
        &["<Chimera", "Leviathan hides Its message in another dimension."],
        &["Leviathan hides Its message in another dimension."],
        &["Chimera", "Leviathan hides Its message in another dimensio ."],
        &["e\"tChimera", "Leviathan hides Its message in another dimension."],
        &["SChimera", "Leviathan hides Its message in another dimension."],
        &["73?Chimera", "Leviathan hides Its message in another dimension."],
        &["Leviathan hides Its message in another dimension."],
    ];

    const LAMB: &[&[&str]] = &[
        &["The Sentinel's power you bear. Its hour has come, little lam ."],
        &["..?Chimera", "The Sentinel's power you bear. Its hour has come, little lamb."],
        &["The Sentinel's power you bear. Its hour has come, little lam ."],
        &["<*Chimera", "The Sentinel's power you bear. Its hour has come, little lamb."],
        &["The Sentinel's power you bear. Its hour come,"],
        &["The Sentinel's power you bear. Its hour has come, little lam ."],
        &["T*Chimera", "The Sentinel's power you bear. Its hour has come, little lam ."],
        &["The Sentinel's power you bear. Its hour has come, little lamb."],
        &["T*Chimera", "The Sentinel's power you bear. Its hour has come, little lamb."],
    ];

    #[test]
    fn flickering_line_is_one_entry() {
        assert_eq!(replay(LEVIATHAN, 3), (1, 1));
    }

    #[test]
    fn speaker_name_and_fixed_letter_update_in_place() {
        let (entries, requests) = replay(LAMB, 3);
        assert_eq!(entries, 1);
        assert!(requests <= 3, "{requests} requests");
    }

    #[test]
    fn request_limit_is_respected() {
        assert_eq!(replay(LAMB, 1), (1, 1));
    }

    #[test]
    fn dialogue_moves_on() {
        let both: Vec<&[&str]> = LEVIATHAN.iter().chain(LAMB).copied().collect();
        assert_eq!(replay(&both, 3).0, 2);
    }

    #[test]
    fn typewriter_text_grows_in_place() {
        let readings: &[&[&str]] = &[
            &["The Sentinel's power you bear. Its hour has c0"],
            &["The Sentinel's power you bear. Its hour has come, little lamb."],
        ];
        assert_eq!(replay(readings, 1), (1, 2));
    }

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
