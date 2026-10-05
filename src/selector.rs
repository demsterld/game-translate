use std::cell::RefCell;

use anyhow::Result;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect, InvalidateRect, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetSystemMetrics, IDC_CROSS, LWA_ALPHA,
    LWA_COLORKEY, LoadCursorW, PostThreadMessageW, RegisterClassW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOW, SetForegroundWindow, SetLayeredWindowAttributes, ShowWindow,
    WM_APP, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::w;

use crate::config::Region;

pub const WM_SELECTED: u32 = WM_APP + 2;

/// Pixels painted with this color are fully transparent, which "cuts out" the selected area.
const HOLE: COLORREF = COLORREF(0x00FF00FF);
const SHADE: COLORREF = COLORREF(0x00000000);
const BORDER: COLORREF = COLORREF(0x003050FF);
const MIN_SIZE: i32 = 8;

struct State {
    origin: POINT,
    start: Option<POINT>,
    current: POINT,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static RESULT: RefCell<Option<Region>> = const { RefCell::new(None) };
}

/// Opens a full-screen dimmed layer; when the user finishes dragging, `WM_SELECTED` is posted to
/// the current thread and the region can be taken with [`take`].
pub fn start() -> Result<()> {
    if STATE.with_borrow(|s| s.is_some()) {
        return Ok(());
    }
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_CROSS)?,
            lpszClassName: w!("GameTranslateSelector"),
            ..Default::default()
        };
        RegisterClassW(&class);

        let (x, y) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (w, h) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        STATE.with_borrow_mut(|s| {
            *s = Some(State { origin: POINT { x, y }, start: None, current: POINT::default() })
        });
        RESULT.with_borrow_mut(|r| *r = None);

        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            w!("GameTranslateSelector"),
            w!("Select region"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        SetLayeredWindowAttributes(hwnd, HOLE, 110, LWA_ALPHA | LWA_COLORKEY)?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
    }
    Ok(())
}

pub fn take() -> Option<Region> {
    RESULT.with_borrow_mut(|r| r.take())
}

fn point(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xFFFF) as i16 as i32, y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32 }
}

fn selection(s: &State) -> Option<RECT> {
    let a = s.start?;
    let b = s.current;
    Some(RECT { left: a.x.min(b.x), top: a.y.min(b.y), right: a.x.max(b.x), bottom: a.y.max(b.y) })
}

unsafe fn finish(hwnd: HWND, region: Option<Region>) {
    unsafe {
        let _ = ReleaseCapture();
        STATE.with_borrow_mut(|s| *s = None);
        RESULT.with_borrow_mut(|r| *r = region);
        let _ = DestroyWindow(hwnd);
        let _ = PostThreadMessageW(GetCurrentThreadId(), WM_SELECTED, WPARAM(0), LPARAM(0));
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                STATE.with_borrow_mut(|s| {
                    if let Some(s) = s.as_mut() {
                        s.start = Some(point(lparam));
                        s.current = point(lparam);
                    }
                });
                SetCapture(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let dragging = STATE.with_borrow_mut(|s| match s.as_mut() {
                    Some(s) if s.start.is_some() => {
                        s.current = point(lparam);
                        true
                    }
                    _ => false,
                });
                if dragging {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let region = STATE.with_borrow_mut(|s| {
                    let s = s.as_mut()?;
                    s.current = point(lparam);
                    let r = selection(s)?;
                    let (w, h) = (r.right - r.left, r.bottom - r.top);
                    (w >= MIN_SIZE && h >= MIN_SIZE)
                        .then(|| Region { x: r.left + s.origin.x, y: r.top + s.origin.y, w, h })
                });
                finish(hwnd, region);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                finish(hwnd, None);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                finish(hwnd, None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);

                let shade = CreateSolidBrush(SHADE);
                FillRect(dc, &client, shade);
                let _ = DeleteObject(shade.into());

                if let Some(r) = STATE.with_borrow(|s| s.as_ref().and_then(selection)) {
                    let hole = CreateSolidBrush(HOLE);
                    FillRect(dc, &r, hole);
                    let _ = DeleteObject(hole.into());
                    let border = CreateSolidBrush(BORDER);
                    FrameRect(dc, &r, border);
                    let inner = RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 };
                    FrameRect(dc, &inner, border);
                    let _ = DeleteObject(border.into());
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
