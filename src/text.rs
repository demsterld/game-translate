#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Speaker name, quest title and similar short labels.
    Heading,
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
}

/// One translated screen of text: what the overlay shows as a single item of the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub blocks: Vec<Block>,
}

const HEADING_MAX_WORDS: usize = 6;

impl Entry {
    pub fn from_paragraphs(paragraphs: &[String]) -> Self {
        let blocks = paragraphs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let has_more = i + 1 < paragraphs.len();
                let cleaned = strip_icon_noise(p);
                if has_more && looks_like_heading(&cleaned) {
                    Block { kind: BlockKind::Heading, text: cleaned }
                } else {
                    Block { kind: BlockKind::Text, text: p.clone() }
                }
            })
            .collect();
        Self { blocks }
    }

    pub fn heading(&self) -> Option<&str> {
        self.blocks.first().filter(|b| b.kind == BlockKind::Heading).map(|b| b.text.as_str())
    }

    /// Everything except the leading heading: the part that identifies a line of dialogue.
    pub fn body(&self) -> String {
        let skip = usize::from(self.heading().is_some());
        self.blocks[skip..].iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    /// Same entry with `heading` in front, replacing the existing heading if any.
    pub fn with_heading(&self, heading: &str) -> Self {
        let skip = usize::from(self.heading().is_some());
        let mut blocks = vec![Block { kind: BlockKind::Heading, text: heading.to_string() }];
        blocks.extend_from_slice(&self.blocks[skip..]);
        Self { blocks }
    }

    pub fn plain(text: String) -> Self {
        Self { blocks: vec![Block { kind: BlockKind::Text, text }] }
    }

    /// Lines prefixed with `# ` become headings, blank lines separate paragraphs.
    pub fn from_marked(text: &str) -> Self {
        let mut blocks: Vec<Block> = Vec::new();
        let mut paragraph_open = false;
        for line in text.lines().map(str::trim) {
            if line.is_empty() {
                paragraph_open = false;
            } else if let Some(heading) = line.strip_prefix("# ").or_else(|| line.strip_prefix('#')) {
                blocks.push(Block { kind: BlockKind::Heading, text: heading.trim().to_string() });
                paragraph_open = false;
            } else {
                match blocks.last_mut() {
                    Some(last) if paragraph_open => {
                        last.text.push(' ');
                        last.text.push_str(line);
                    }
                    _ => blocks.push(Block { kind: BlockKind::Text, text: line.to_string() }),
                }
                paragraph_open = true;
            }
        }
        Self { blocks }
    }

    /// Source text used for change detection, the translation cache and the prompt context.
    pub fn key(&self) -> String {
        self.blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub fn texts(&self) -> Vec<String> {
        self.blocks.iter().map(|b| b.text.clone()).collect()
    }

    /// Same structure as `self` with the texts replaced, or a single text block when the
    /// translator returned a different number of pieces.
    pub fn with_texts(&self, texts: Vec<String>) -> Self {
        if texts.len() != self.blocks.len() {
            return Self::plain(texts.join("\n"));
        }
        let blocks = self.blocks.iter().zip(texts).map(|(b, text)| Block { kind: b.kind, text }).collect();
        Self { blocks }
    }
}

fn looks_like_heading(p: &str) -> bool {
    let words = p.split_whitespace().count();
    let first_upper = p.chars().find(|c| c.is_alphabetic()).is_some_and(char::is_uppercase);
    let ends_sentence = p.trim_end().ends_with(['.', '!', '?', '…', ',', ';', ':']);
    (1..=HEADING_MAX_WORDS).contains(&words) && first_upper && !ends_sentence
}

/// Games often draw an icon or portrait frame right before the speaker name, and OCR reads it
/// as a couple of junk characters glued to the name: "T*Chimera", "73?Chimera", "eeChimera".
fn strip_icon_noise(p: &str) -> String {
    let p = p.trim();
    let first_len = p.find(char::is_whitespace).unwrap_or(p.len());
    let (first, rest) = p.split_at(first_len);

    let mut word = first;
    if let Some(i) = word.rfind(|c: char| !c.is_alphanumeric() && c != '\'') {
        let cut = i + word[i..].chars().next().map_or(1, char::len_utf8);
        if word[..cut].chars().count() <= 4 && cut < word.len() {
            word = &word[cut..];
        }
    }

    // A capitalized name preceded by one or two stray letters: "SChimera", "tChimera".
    let chars: Vec<char> = word.chars().collect();
    let start = (1..=2).find(|&i| {
        chars.len() > i + 1 && chars[i].is_uppercase() && chars[i + 1].is_lowercase() && chars[..i].iter().all(|c| c.is_alphabetic())
    });
    if let Some(i) = start {
        let prefix: String = chars[..i].iter().collect();
        if prefix != "Mc" {
            word = &word[chars[..i].iter().map(|c| c.len_utf8()).sum::<usize>()..];
        }
    }

    format!("{word}{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(e: &Entry) -> Vec<BlockKind> {
        e.blocks.iter().map(|b| b.kind).collect()
    }

    #[test]
    fn speaker_name_becomes_heading() {
        let e = Entry::from_paragraphs(&[
            "Arkyria".into(),
            "I... I can't take it! I trained harder. Endured more. Bled more!".into(),
        ]);
        assert_eq!(kinds(&e), [BlockKind::Heading, BlockKind::Text]);
    }

    #[test]
    fn quest_title_becomes_heading() {
        let e = Entry::from_paragraphs(&[
            "Dawn Breaks on Dark Tides".into(),
            "Join the crowd, toward the kingdom rising in the dark waves...".into(),
        ]);
        assert_eq!(kinds(&e), [BlockKind::Heading, BlockKind::Text]);
    }

    #[test]
    fn short_exclamation_is_text() {
        let e = Entry::from_paragraphs(&["Run!".into(), "They are coming for us.".into()]);
        assert_eq!(kinds(&e), [BlockKind::Text, BlockKind::Text]);
    }

    #[test]
    fn single_paragraph_is_text() {
        assert_eq!(kinds(&Entry::from_paragraphs(&["Arkyria".into()])), [BlockKind::Text]);
    }

    #[test]
    fn marked_text_is_parsed() {
        let e = Entry::from_marked("# Аркирия\nЯ больше не могу!\nЯ тренировалась.\n\nВторой абзац.");
        assert_eq!(kinds(&e), [BlockKind::Heading, BlockKind::Text, BlockKind::Text]);
        assert_eq!(e.blocks[1].text, "Я больше не могу! Я тренировалась.");
    }

    #[test]
    fn icon_noise_is_stripped_from_speaker_name() {
        for noisy in ["T*Chimera", "<Chimera", "73?Chimera", "ey*Chimera", "e\"tChimera", "SChimera", "eeChimera", "@G-Chimera", "..?Chimera"] {
            let e = Entry::from_paragraphs(&[noisy.into(), "Leviathan hides its message.".into()]);
            assert_eq!(e.heading(), Some("Chimera"), "{noisy}");
        }
    }

    #[test]
    fn real_names_are_kept() {
        for name in ["McGregor", "O'Neil", "Jean-Luc", "Chimera", "Old Man Willow"] {
            let e = Entry::from_paragraphs(&[name.into(), "Hello there.".into()]);
            assert_eq!(e.heading(), Some(name));
        }
    }

    #[test]
    fn body_and_heading_are_split() {
        let e = Entry::from_paragraphs(&["Chimera".into(), "Line one.".into(), "Line two.".into()]);
        assert_eq!(e.body(), "Line one.\nLine two.");
        let plain = Entry::from_paragraphs(&["Line one.".into()]);
        assert_eq!(plain.with_heading("Chimera").heading(), Some("Chimera"));
        assert_eq!(e.with_heading("Leks").blocks.len(), 3);
    }

    #[test]
    fn mismatched_translation_falls_back_to_plain() {
        let e = Entry::from_paragraphs(&["Arkyria".into(), "Hello there.".into()]);
        assert_eq!(e.with_texts(vec!["Аркирия Привет.".into()]), Entry::plain("Аркирия Привет.".into()));
    }
}
