use std::cell::RefCell;
use std::collections::VecDeque;

use anyhow::Result;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateFontW, CreateSolidBrush,
    DEFAULT_CHARSET, DT_CALCRECT, DT_NOPREFIX, DT_WORDBREAK, DeleteDC, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE,
    FW_BOLD, FW_NORMAL, FillRect, GetMonitorInfoW, HDC, HFONT, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromRect, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, GetClientRect, HWND_TOPMOST, KillTimer, LWA_ALPHA,
    RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SetLayeredWindowAttributes, SetTimer,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::w;

use crate::config::{Config, Region};
use crate::text::{BlockKind, Entry};

const PADDING: i32 = 14;
const GAP: i32 = 6;
const MIN_WIDTH: i32 = 420;
const MAX_WIDTH: i32 = 900;
const STATUS_TIMER: usize = 1;
const STATUS_MS: u32 = 4000;

const BACKGROUND: (u8, u8, u8) = (22, 26, 32);
const HEADING: (u8, u8, u8) = (240, 200, 90);
const TEXT: (u8, u8, u8) = (240, 240, 240);
const STATUS: (u8, u8, u8) = (150, 160, 175);
const SEPARATOR: (u8, u8, u8) = (60, 66, 76);
/// Brightness of feed entries by age, newest first; older ones use the last value.
const FADE: [f32; 3] = [1.0, 0.72, 0.5];

struct State {
    hwnd: HWND,
    heading_font: HFONT,
    text_font: HFONT,
    status_font: HFONT,
    font_size: i32,
    history_size: usize,
    max_height_percent: i32,
    feed: VecDeque<Entry>,
    status: Option<String>,
    region: Option<Region>,
    visible: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn init(cfg: &Config) -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("GameTranslateOverlay"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("GameTranslateOverlay"),
            w!("Game Translate"),
            WS_POPUP,
            0,
            0,
            MIN_WIDTH,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetLayeredWindowAttributes(hwnd, COLORREF(0), cfg.opacity, LWA_ALPHA)?;
        // Keeps the overlay out of our own screen captures when it overlaps the region.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);

        let size = cfg.font_size.max(8);
        STATE.with_borrow_mut(|s| {
            *s = Some(State {
                hwnd,
                heading_font: font(size + 2, FW_BOLD.0 as i32),
                text_font: font(size, FW_NORMAL.0 as i32),
                status_font: font(size * 4 / 5, FW_NORMAL.0 as i32),
                font_size: size,
                history_size: cfg.history_size.max(1),
                max_height_percent: cfg.max_height_percent.clamp(10, 100),
                feed: VecDeque::new(),
                status: None,
                region: cfg.region,
                visible: true,
            })
        });
    }
    Ok(())
}

unsafe fn font(size: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            -size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            FF_DONTCARE.0 as u32,
            w!("Segoe UI"),
        )
    }
}

/// Appends a translation to the bottom of the feed.
pub fn push_entry(entry: Entry, region: Option<Region>) {
    with_state(|s| {
        s.feed.push_back(entry);
        while s.feed.len() > s.history_size {
            s.feed.pop_front();
        }
        s.status = None;
        s.region = region;
        unsafe {
            let _ = KillTimer(Some(s.hwnd), STATUS_TIMER);
        }
    });
}

/// Shows a service message under the feed; with `transient` it disappears after a few seconds.
pub fn set_status(text: &str, region: Option<Region>, transient: bool) {
    with_state(|s| {
        s.status = Some(text.to_string());
        s.region = region;
        unsafe {
            if transient {
                SetTimer(Some(s.hwnd), STATUS_TIMER, STATUS_MS, None);
            } else {
                let _ = KillTimer(Some(s.hwnd), STATUS_TIMER);
            }
        }
    });
}

pub fn toggle_visible() {
    with_state(|s| s.visible = !s.visible);
}

fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s.as_mut() {
            f(s);
            refresh(s);
        }
    });
}

enum Item {
    Text { font: HFONT, color: COLORREF, text: Vec<u16>, top: i32, height: i32 },
    Separator { top: i32, color: COLORREF },
}

struct Layout {
    items: Vec<Item>,
    height: i32,
}

/// Places the feed (oldest first, newest at the bottom) and the status line in a column of
/// `width` pixels, including the outer padding.
unsafe fn layout(s: &State, width: i32) -> Layout {
    let inner = width - PADDING * 2;
    let para_gap = s.font_size / 3;
    let entry_gap = s.font_size * 2 / 3;
    let mut items = Vec::new();
    let mut y = PADDING;

    let count = s.feed.len();
    for (i, entry) in s.feed.iter().enumerate() {
        let fade = FADE[(count - 1 - i).min(FADE.len() - 1)];
        if i > 0 {
            items.push(Item::Separator { top: y + entry_gap / 2, color: rgb(SEPARATOR, 1.0) });
            y += entry_gap;
        }
        for (j, block) in entry.blocks.iter().enumerate() {
            if j > 0 {
                y += if block.kind == BlockKind::Heading { entry_gap } else { para_gap };
            }
            let (font, color) = match block.kind {
                BlockKind::Heading => (s.heading_font, rgb(HEADING, fade)),
                BlockKind::Text => (s.text_font, rgb(TEXT, fade)),
            };
            let mut text: Vec<u16> = block.text.encode_utf16().collect();
            let height = unsafe { measure(font, &mut text, inner) };
            items.push(Item::Text { font, color, text, top: y, height });
            y += height;
        }
    }

    if let Some(status) = &s.status {
        if !items.is_empty() {
            y += entry_gap;
        }
        let mut text: Vec<u16> = status.encode_utf16().collect();
        let height = unsafe { measure(s.status_font, &mut text, inner) };
        items.push(Item::Text { font: s.status_font, color: rgb(STATUS, 1.0), text, top: y, height });
        y += height;
    }

    Layout { items, height: y + PADDING }
}

/// Recomputes size and position, dropping the oldest entries that do not fit, and repaints.
fn refresh(s: &mut State) {
    unsafe {
        if !s.visible || (s.feed.is_empty() && s.status.is_none()) {
            let _ = ShowWindow(s.hwnd, SW_HIDE);
            return;
        }

        let anchor = s.region.unwrap_or(Region { x: 40, y: 40, w: 0, h: 0 });
        let monitor = monitor_rect(anchor);
        let monitor_w = monitor.right - monitor.left;
        let width = anchor.w.clamp(MIN_WIDTH, MAX_WIDTH).min(monitor_w);
        let max_height = (monitor.bottom - monitor.top) * s.max_height_percent / 100;

        let mut height = layout(s, width).height;
        while height > max_height && s.feed.len() > 1 {
            s.feed.pop_front();
            height = layout(s, width).height;
        }
        let height = height.min(max_height);

        let x = anchor.x.clamp(monitor.left, (monitor.right - width).max(monitor.left));
        let mut y = anchor.y + anchor.h + GAP;
        if y + height > monitor.bottom {
            y = anchor.y - height - GAP;
        }
        if y < monitor.top {
            y = anchor.y.clamp(monitor.top, (monitor.bottom - height).max(monitor.top));
        }

        let _ = SetWindowPos(s.hwnd, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE);
        let _ = InvalidateRect(Some(s.hwnd), None, true);
        let _ = ShowWindow(s.hwnd, SW_SHOWNOACTIVATE);
    }
}

fn rgb((r, g, b): (u8, u8, u8), fade: f32) -> COLORREF {
    let mix = |c: u8, bg: u8| (bg as f32 + (c as f32 - bg as f32) * fade).round() as u32;
    let (br, bg, bb) = BACKGROUND;
    COLORREF(mix(r, br) | (mix(g, bg) << 8) | (mix(b, bb) << 16))
}

unsafe fn monitor_rect(r: Region) -> RECT {
    unsafe {
        let rect = RECT { left: r.x, top: r.y, right: r.x + r.w.max(1), bottom: r.y + r.h.max(1) };
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        info.rcWork
    }
}

unsafe fn measure(font: HFONT, text: &mut [u16], width: i32) -> i32 {
    unsafe {
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, font.into());
        let mut rect = RECT { left: 0, top: 0, right: width, bottom: 0 };
        DrawTextW(dc, text, &mut rect, DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX);
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        rect.bottom.max(1)
    }
}

unsafe fn paint(hwnd: HWND, dc: HDC) {
    unsafe {
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let bg = CreateSolidBrush(rgb(BACKGROUND, 1.0));
        FillRect(dc, &client, bg);
        let _ = DeleteObject(bg.into());

        STATE.with_borrow(|s| {
            let Some(s) = s.as_ref() else { return };
            let layout = layout(s, client.right - client.left);
            SetBkMode(dc, TRANSPARENT);
            for item in layout.items {
                match item {
                    Item::Text { font, color, mut text, top, height } => {
                        let old = SelectObject(dc, font.into());
                        SetTextColor(dc, color);
                        let mut rect = RECT {
                            left: PADDING,
                            top,
                            right: client.right - PADDING,
                            bottom: (top + height).min(client.bottom),
                        };
                        DrawTextW(dc, &mut text, &mut rect, DT_WORDBREAK | DT_NOPREFIX);
                        SelectObject(dc, old);
                    }
                    Item::Separator { top, color } => {
                        let brush = CreateSolidBrush(color);
                        let line = RECT { left: PADDING, top, right: client.right - PADDING, bottom: top + 1 };
                        FillRect(dc, &line, brush);
                        let _ = DeleteObject(brush.into());
                    }
                }
            }
        });
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                paint(hwnd, dc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_TIMER if wparam.0 == STATUS_TIMER => {
                let _ = KillTimer(Some(hwnd), STATUS_TIMER);
                with_state(|s| s.status = None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}