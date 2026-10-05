use std::cell::Cell;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::config::Config;
use crate::text::Entry;

/// After a Gemini failure in auto mode, Google Translate is used directly for this long.
const GEMINI_RETRY_AFTER: Duration = Duration::from_secs(60);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Engine {
    Auto,
    Gemini,
    Google,
}

pub struct Translator {
    client: reqwest::blocking::Client,
    engine: Engine,
    api_key: Option<String>,
    model: String,
    target: String,
    target_code: String,
    thinking: String,
    thinking_supported: Cell<bool>,
    text_timeout: Duration,
    gemini_down_until: Cell<Option<Instant>>,
}

impl Translator {
    pub fn new(cfg: &Config) -> Result<Self> {
        let engine = match cfg.engine.trim().to_lowercase().as_str() {
            "auto" | "" => Engine::Auto,
            "gemini" => Engine::Gemini,
            "google" => Engine::Google,
            other => bail!("неизвестный engine = \"{other}\" в config.toml (нужно auto, gemini или google)"),
        };
        let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30));
        if !cfg.proxy.trim().is_empty() {
            builder = builder.proxy(reqwest::Proxy::all(cfg.proxy.trim()).context("неверный proxy в config.toml")?);
        }
        let api_key = std::env::var("GEMINI_API_KEY").ok().filter(|k| !k.trim().is_empty());
        Ok(Self {
            client: builder.build()?,
            engine,
            api_key,
            model: cfg.model.clone(),
            target: cfg.target_language.clone(),
            target_code: cfg.target_code.clone(),
            thinking: cfg.gemini_thinking.trim().to_string(),
            thinking_supported: Cell::new(true),
            text_timeout: Duration::from_millis(cfg.gemini_timeout_ms.max(1000)),
            gemini_down_until: Cell::new(None),
        })
    }

    /// Translates every block of `src`, keeping headings and paragraphs in place.
    pub fn translate_entry(&self, src: &Entry, context: &[String]) -> Result<Entry> {
        let texts = src.texts();
        let translated = match self.engine {
            Engine::Gemini => self.gemini_blocks(&texts, context)?,
            Engine::Google => self.google_blocks(&texts)?,
            Engine::Auto => {
                if self.gemini_down_until.get().is_some_and(|t| Instant::now() < t) {
                    self.google_blocks(&texts)?
                } else {
                    match self.gemini_blocks(&texts, context) {
                        Ok(t) => t,
                        Err(gemini_err) => {
                            crate::log::write(format_args!("gemini failed, using google: {gemini_err:#}"));
                            self.gemini_down_until.set(Some(Instant::now() + GEMINI_RETRY_AFTER));
                            self.google_blocks(&texts)
                                .with_context(|| format!("Gemini тоже недоступен: {gemini_err:#}"))?
                        }
                    }
                }
            }
        };
        Ok(src.with_texts(translated))
    }

    /// One request with the blocks on separate lines; if Google merges or splits lines, every
    /// block is translated on its own.
    fn google_blocks(&self, texts: &[String]) -> Result<Vec<String>> {
        let joined = self.google_text(&texts.join("\n"))?;
        let lines: Vec<String> =
            joined.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        if lines.len() == texts.len() {
            return Ok(lines);
        }
        texts.iter().map(|t| self.google_text(t)).collect()
    }

    fn google_text(&self, text: &str) -> Result<String> {
        let url = reqwest::Url::parse_with_params(
            "https://translate.googleapis.com/translate_a/single",
            &[("client", "gtx"), ("sl", "auto"), ("tl", self.target_code.as_str()), ("dt", "t"), ("q", text)],
        )?;
        let resp = self.client.get(url).send().context("Google Translate недоступен")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Google Translate {status}");
        }
        let value: Value = resp.json().context("неверный ответ Google Translate")?;
        let text: String = value[0]
            .as_array()
            .map(|chunks| chunks.iter().filter_map(|c| c[0].as_str()).collect())
            .unwrap_or_default();
        if text.trim().is_empty() {
            bail!("Google Translate вернул пустой перевод");
        }
        Ok(text.trim().to_string())
    }

    fn gemini_blocks(&self, texts: &[String], context: &[String]) -> Result<Vec<String>> {
        let system = format!(
            "You translate video game text into {}. The input is a JSON array of text blocks from one screen: \
             speaker names, quest titles and dialog paragraphs. Return a JSON array with exactly one translated \
             string per input block, in the same order. The text comes from OCR and may contain recognition \
             errors or stray symbols: silently fix obvious OCR mistakes. Translate or transliterate character \
             names consistently, and keep terms and tone consistent with the previous lines.",
            self.target
        );
        let mut prompt = String::new();
        if !context.is_empty() {
            prompt.push_str("Previous lines (context only, do not translate):\n");
            for line in context {
                prompt.push_str(line);
                prompt.push('\n');
            }
            prompt.push_str("\nTranslate:\n");
        }
        prompt.push_str(&serde_json::to_string(texts)?);

        let schema = json!({ "type": "ARRAY", "items": { "type": "STRING" } });
        let raw = self.generate(&system, vec![json!({ "text": prompt })], self.text_timeout, Some(schema))?;
        let out: Vec<String> = serde_json::from_str(&raw).with_context(|| format!("Gemini вернул не JSON: {raw}"))?;
        Ok(out.into_iter().map(|s| s.trim().to_string()).collect())
    }

    pub fn translate_image(&self, png: &[u8]) -> Result<Entry> {
        let system = format!(
            "The image is a fragment of a video game screen. Read all text on it and translate it into {}. \
             Preserve the reading order. Put each speaker name or title on its own line prefixed with \"# \", \
             and separate paragraphs with a blank line. Output only the translation, without comments.",
            self.target
        );
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let raw = self.generate(
            &system,
            vec![json!({ "inline_data": { "mime_type": "image/png", "data": data } })],
            IMAGE_TIMEOUT,
            None,
        )?;
        Ok(Entry::from_marked(&raw))
    }

    fn generate(&self, system: &str, parts: Vec<Value>, timeout: Duration, schema: Option<Value>) -> Result<String> {
        let key = self.api_key.as_deref().ok_or_else(|| anyhow!("не задана переменная окружения GEMINI_API_KEY"))?;
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent", self.model);
        let mut generation = json!({ "temperature": 0.2 });
        let with_thinking = !self.thinking.is_empty() && self.thinking_supported.get();
        if with_thinking {
            generation["thinkingConfig"] = json!({ "thinkingLevel": self.thinking });
        }
        if let Some(schema) = &schema {
            generation["responseMimeType"] = json!("application/json");
            generation["responseSchema"] = schema.clone();
        }
        let body = json!({
            "systemInstruction": { "parts": [{ "text": system }] },
            "contents": [{ "role": "user", "parts": parts }],
            "generationConfig": generation,
        });

        let resp = self
            .client
            .post(url)
            .timeout(timeout)
            .header("x-goog-api-key", key)
            .json(&body)
            .send()
            .map_err(|e| if e.is_timeout() { anyhow!("Gemini не ответил за {} с", timeout.as_secs_f32()) } else { e.into() })
            .context("Gemini недоступен")?;
        let status = resp.status();
        let value: Value = resp.json().context("неверный ответ Gemini")?;
        if !status.is_success() {
            let msg = value["error"]["message"].as_str().unwrap_or("unknown error");
            if with_thinking && status == reqwest::StatusCode::BAD_REQUEST && msg.contains("hinking") {
                crate::log::write(format_args!("model {} rejected thinking level: {msg}", self.model));
                self.thinking_supported.set(false);
                return self.generate(system, parts, timeout, schema);
            }
            let reasons: Vec<&str> = value["error"]["details"]
                .as_array()
                .map(|d| d.iter().filter_map(|x| x["reason"].as_str()).collect())
                .unwrap_or_default();
            bail!("Gemini {status} [{}]: {msg}", reasons.join(", "));
        }

        let text: String = value["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect())
            .unwrap_or_default();
        if text.trim().is_empty() {
            let reason = value["candidates"][0]["finishReason"].as_str().unwrap_or("empty response");
            bail!("Gemini не вернул перевод ({reason})");
        }
        Ok(text.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::text::BlockKind;

    fn translate_with(engine: &str) -> Entry {
        let mut cfg = Config { engine: engine.into(), ..Config::default() };
        if let Ok(model) = std::env::var("GEMINI_MODEL") {
            cfg.model = model;
        }
        let src = Entry::from_paragraphs(&[
            "Arkyria".into(),
            "I... I can't take it! I trained harder. Endured more. Bled more!".into(),
        ]);
        let out = Translator::new(&cfg).unwrap().translate_entry(&src, &[]).unwrap();
        println!("{engine}: {out:#?}");
        assert_eq!(out.blocks.iter().map(|b| b.kind).collect::<Vec<_>>(), [BlockKind::Heading, BlockKind::Text]);
        assert!(out.blocks.iter().all(|b| b.text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))));
        out
    }

    #[test]
    #[ignore = "calls the Gemini API"]
    fn translates_with_gemini() {
        translate_with("gemini");
    }

    #[test]
    #[ignore = "calls Google Translate"]
    fn translates_with_google() {
        translate_with("google");
    }

    #[test]
    #[ignore = "calls Gemini and/or Google Translate"]
    fn translates_with_auto() {
        translate_with("auto");
    }
}
