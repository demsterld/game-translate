#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod config;
mod log;
mod ocr;
mod overlay;
mod selector;
mod text;
mod translate;
mod worker;

use anyhow::Result;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MB_ICONERROR, MB_OK, MSG, MessageBoxW, PostQuitMessage, TranslateMessage,
    WM_HOTKEY,
};
use windows::core::HSTRING;

use worker::{Cmd, Event};

#[derive(Clone, Copy)]
enum Action {
    Select,
    ToggleLive,
    Once,
    Vision,
    Hide,
    Move,
    ResetPosition,
    Quit,
}

const HOTKEYS: &[(Action, char)] = &[
    (Action::Select, 'R'),
    (Action::ToggleLive, 'T'),
    (Action::Once, 'S'),
    (Action::Vision, 'G'),
    (Action::Hide, 'H'),
    (Action::Move, 'W'),
    (Action::ResetPosition, 'N'),
    (Action::Quit, 'Q'),
];

const HELP: &str = "Game Translate\n\
    Ctrl+Alt+R — выбрать область\n\
    Ctrl+Alt+T — живой перевод вкл/выкл\n\
    Ctrl+Alt+S — перевести один раз\n\
    Ctrl+Alt+G — перевести картинкой через Gemini Vision\n\
    Ctrl+Alt+H — скрыть/показать окно\n\
    Ctrl+Alt+W — переместить окно / закрепить\n\
    Ctrl+Alt+N — вернуть окно к области\n\
    Ctrl+Alt+Q — выход";

fn main() {
    if let Err(e) = run() {
        unsafe {
            MessageBoxW(None, &HSTRING::from(format!("{e:#}")), &HSTRING::from("Game Translate"), MB_OK | MB_ICONERROR);
        }
    }
}

fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut cfg = config::load()?;
    let (cmd_tx, events) = worker::spawn(cfg.clone(), unsafe { GetCurrentThreadId() });

    overlay::init(&cfg)?;
    let busy = register_hotkeys();
    if busy.is_empty() {
        overlay::set_status(HELP, cfg.region, false);
    } else {
        let warning = format!("{HELP}\n\nЗаняты другой программой и не работают: {}", busy.join(", "));
        overlay::set_status(&warning, cfg.region, false);
    }

    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_HOTKEY => {
                let Some(&(action, _)) = HOTKEYS.get(msg.wParam.0) else { continue };
                let cmd = match action {
                    Action::Select => {
                        if let Err(e) = selector::start() {
                            overlay::set_status(&format!("Ошибка: {e:#}"), cfg.region, false);
                        }
                        None
                    }
                    Action::ToggleLive => Some(Cmd::ToggleLive),
                    Action::Once => Some(Cmd::Once),
                    Action::Vision => Some(Cmd::Vision),
                    Action::Hide => {
                        overlay::toggle_visible();
                        None
                    }
                    Action::Move => {
                        if let Some(rect) = overlay::toggle_move() {
                            cfg.overlay_rect = Some(rect);
                            save_with_status(&cfg, "Окно закреплено");
                        }
                        None
                    }
                    Action::ResetPosition => {
                        overlay::reset_position();
                        cfg.overlay_rect = None;
                        save_with_status(&cfg, "Окно снова следует за областью");
                        None
                    }
                    Action::Quit => {
                        unsafe { PostQuitMessage(0) };
                        None
                    }
                };
                if let Some(cmd) = cmd {
                    let _ = cmd_tx.send(cmd);
                }
            }
            worker::WM_WORKER => {
                while let Ok(ev) = events.try_recv() {
                    match ev {
                        Event::Translation(entry) => overlay::push_entry(entry, cfg.region),
                        Event::Status(s) => overlay::set_status(&s, cfg.region, true),
                        Event::Error(e) => overlay::set_status(&e, cfg.region, false),
                        Event::Live(on) => overlay::set_status(
                            if on { "Живой перевод: ВКЛ" } else { "Живой перевод: ВЫКЛ" },
                            cfg.region,
                            true,
                        ),
                    }
                }
            }
            selector::WM_SELECTED => {
                if let Some(region) = selector::take() {
                    cfg.region = Some(region);
                    if let Err(e) = config::save(&cfg) {
                        overlay::set_status(&format!("Не удалось сохранить настройки: {e:#}"), cfg.region, false);
                    }
                    let _ = cmd_tx.send(Cmd::SetRegion(region));
                    overlay::set_status(
                        &format!(
                            "Область {}x{} выбрана. Ctrl+Alt+S — перевести, Ctrl+Alt+T — живой режим",
                            region.w, region.h
                        ),
                        cfg.region,
                        true,
                    );
                }
            }
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }

    unsafe {
        for id in 0..HOTKEYS.len() {
            let _ = UnregisterHotKey(None, id as i32);
        }
    }
    Ok(())
}

fn save_with_status(cfg: &config::Config, done: &str) {
    match config::save(cfg) {
        Ok(()) => overlay::set_status(done, cfg.region, true),
        Err(e) => overlay::set_status(&format!("Не удалось сохранить настройки: {e:#}"), cfg.region, false),
    }
}

/// Registers all hotkeys and returns the ones another program already owns.
fn register_hotkeys() -> Vec<String> {
    let mods: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    let mut busy = Vec::new();
    for (id, &(_, key)) in HOTKEYS.iter().enumerate() {
        if unsafe { RegisterHotKey(None, id as i32, mods, key as u32) }.is_err() {
            busy.push(format!("Ctrl+Alt+{key}"));
        }
    }
    busy
}
