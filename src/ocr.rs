use anyhow::{Result, bail};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::core::HSTRING;

use crate::capture::Frame;

pub struct Ocr {
    engine: OcrEngine,
    max_dim: u32,
}

impl Ocr {
    pub fn new(lang: &str) -> Result<Self> {
        let language = Language::CreateLanguage(&HSTRING::from(lang))?;
        if !OcrEngine::IsLanguageSupported(&language)? {
            bail!("OCR для языка {lang} не установлен: Параметры -> Время и язык -> Язык -> добавить {lang}");
        }
        let engine = OcrEngine::TryCreateFromLanguage(&language)?;
        let max_dim = OcrEngine::MaxImageDimension()?;
        Ok(Self { engine, max_dim })
    }

    /// Returns recognized paragraphs, top to bottom. Lines inside a paragraph are joined with
    /// spaces because game dialogs wrap mid-sentence.
    pub fn recognize(&self, frame: &Frame) -> Result<Vec<String>> {
        let scale = if frame.height < 400 && frame.width * 2 <= self.max_dim && frame.height * 2 <= self.max_dim {
            2
        } else {
            1
        };
        let scaled;
        let frame = if scale > 1 {
            scaled = upscale(frame, scale);
            &scaled
        } else {
            frame
        };

        let writer = DataWriter::new()?;
        writer.WriteBytes(&frame.bgra)?;
        let buffer = writer.DetachBuffer()?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            frame.width as i32,
            frame.height as i32,
        )?;

        let result = self.engine.RecognizeAsync(&bitmap)?.join()?;
        let mut lines = Vec::new();
        for line in result.Lines()? {
            let mut bounds: Option<LineBox> = None;
            for word in line.Words()? {
                let r = word.BoundingRect()?;
                let b = LineBox { top: r.Y, bottom: r.Y + r.Height };
                bounds = Some(match bounds {
                    Some(a) => LineBox { top: a.top.min(b.top), bottom: a.bottom.max(b.bottom) },
                    None => b,
                });
            }
            let text = normalize(&line.Text()?.to_string_lossy());
            if let Some(bounds) = bounds
                && !text.is_empty()
            {
                lines.push((text, bounds));
            }
        }
        Ok(group_paragraphs(lines))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LineBox {
    pub top: f32,
    pub bottom: f32,
}

/// Lines separated by a vertical gap larger than this share of the average line height start a
/// new paragraph (a speaker name above a dialog line, a quest title above its description).
const PARAGRAPH_GAP: f32 = 0.6;

pub fn group_paragraphs(mut lines: Vec<(String, LineBox)>) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    lines.sort_by(|a, b| a.1.top.total_cmp(&b.1.top));
    let avg_height = lines.iter().map(|(_, b)| b.bottom - b.top).sum::<f32>() / lines.len() as f32;

    let mut paragraphs: Vec<String> = Vec::new();
    let mut prev_bottom = f32::NEG_INFINITY;
    for (text, b) in lines {
        let gap = b.top - prev_bottom;
        match paragraphs.last_mut() {
            Some(last) if gap <= avg_height * PARAGRAPH_GAP => {
                last.push(' ');
                last.push_str(&text);
            }
            _ => paragraphs.push(text),
        }
        prev_bottom = prev_bottom.max(b.bottom);
    }
    paragraphs
}

fn upscale(f: &Frame, k: u32) -> Frame {
    let (w, h) = (f.width * k, f.height * k);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let src_row = ((y / k) * f.width * 4) as usize;
        let dst_row = (y * w * 4) as usize;
        for x in 0..w {
            let s = src_row + ((x / k) * 4) as usize;
            let d = dst_row + (x * 4) as usize;
            bgra[d..d + 4].copy_from_slice(&f.bgra[s..s + 4]);
        }
    }
    Frame { width: w, height: h, bgra }
}

pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::capture;
    use crate::config::Region;

    #[test]
    #[ignore = "reads the real screen"]
    fn recognizes_screen_text() {
        let _ = unsafe { windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED) };
        let frame = capture(Region { x: 0, y: 0, w: 1000, h: 300 }).unwrap();
        let text = Ocr::new("en-US").unwrap().recognize(&frame).unwrap();
        println!("OCR: {text:#?}");
        assert!(!text.is_empty());
    }

    fn line(text: &str, top: f32, bottom: f32) -> (String, LineBox) {
        (text.into(), LineBox { top, bottom })
    }

    #[test]
    fn speaker_name_is_separate_paragraph() {
        let paragraphs = group_paragraphs(vec![
            line("I... I can't take it! I trained harder.", 120.0, 150.0),
            line("Arkyria", 40.0, 72.0),
            line("Endured more. Bled more!", 154.0, 184.0),
        ]);
        assert_eq!(paragraphs, ["Arkyria", "I... I can't take it! I trained harder. Endured more. Bled more!"]);
    }

    #[test]
    fn wrapped_lines_stay_together() {
        let paragraphs = group_paragraphs(vec![
            line("Join the crowd, toward the", 0.0, 30.0),
            line("kingdom rising in the dark waves...", 36.0, 66.0),
        ]);
        assert_eq!(paragraphs, ["Join the crowd, toward the kingdom rising in the dark waves..."]);
    }

    /// `OCR_IMAGE=path\to\shot.png cargo test -- --ignored --nocapture recognizes_png`
    #[test]
    #[ignore = "needs OCR_IMAGE"]
    fn recognizes_png() {
        let _ = unsafe { windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED) };
        let path = std::env::var("OCR_IMAGE").expect("OCR_IMAGE");
        let mut decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let channels = info.color_type.samples();
        let mut bgra = Vec::with_capacity((info.width * info.height * 4) as usize);
        for px in buf[..info.buffer_size()].chunks_exact(channels) {
            let (r, g, b) = if channels >= 3 { (px[0], px[1], px[2]) } else { (px[0], px[0], px[0]) };
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
        let frame = Frame { width: info.width, height: info.height, bgra };
        println!("OCR: {:#?}", Ocr::new("en-US").unwrap().recognize(&frame).unwrap());
    }
}
