use anyhow::{Result, bail};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDC, ReleaseDC, SRCCOPY, SelectObject,
};

use crate::config::Region;

/// Top-down 32-bit BGRA image.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub fn capture(r: Region) -> Result<Frame> {
    if r.w <= 0 || r.h <= 0 {
        bail!("empty region");
    }
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.w,
                biHeight: -r.h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let result = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0).and_then(|bmp| {
            let old = SelectObject(mem, bmp.into());
            let blit = BitBlt(mem, 0, 0, r.w, r.h, Some(screen), r.x, r.y, SRCCOPY | CAPTUREBLT);
            let len = (r.w * r.h * 4) as usize;
            let mut bgra = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            blit?;
            for px in bgra.as_chunks_mut::<4>().0 {
                px[3] = 255;
            }
            Ok(bgra)
        });

        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);

        Ok(Frame { width: r.w as u32, height: r.h as u32, bgra: result? })
    }
}

/// Coarse grayscale sample of the frame used to detect changes cheaply.
pub fn signature(f: &Frame) -> Vec<u8> {
    const STEP: usize = 3;
    let (w, h) = (f.width as usize, f.height as usize);
    let mut out = Vec::with_capacity((w / STEP + 1) * (h / STEP + 1));
    for y in (0..h).step_by(STEP) {
        for x in (0..w).step_by(STEP) {
            let i = (y * w + x) * 4;
            let (b, g, r) = (f.bgra[i] as u32, f.bgra[i + 1] as u32, f.bgra[i + 2] as u32);
            out.push(((r * 77 + g * 150 + b * 29) >> 8) as u8);
        }
    }
    out
}

/// Share of samples whose brightness changed noticeably.
pub fn difference(a: &[u8], b: &[u8]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 1.0;
    }
    let changed = a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > 40).count();
    changed as f32 / a.len() as f32
}

pub fn to_png(f: &Frame) -> Result<Vec<u8>> {
    let mut rgba = f.bgra.clone();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, f.width, f.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(out)
}
