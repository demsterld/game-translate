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
                let kind = if has_more && looks_like_heading(p) { BlockKind::Heading } else { BlockKind::Text };
                Block { kind, text: p.clone() }
            })
            .collect();
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
    fn mismatched_translation_falls_back_to_plain() {
        let e = Entry::from_paragraphs(&["Arkyria".into(), "Hello there.".into()]);
        assert_eq!(e.with_texts(vec!["Аркирия Привет.".into()]), Entry::plain("Аркирия Привет.".into()));
    }
}
