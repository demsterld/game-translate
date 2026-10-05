use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Log {
    file: Mutex<Option<File>>,
    start: Instant,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Appends a line to `game-translate.log` next to the executable (recreated on every start).
pub fn write(msg: impl std::fmt::Display) {
    let log = LOG.get_or_init(|| {
        let file = std::env::current_exe()
            .ok()
            .and_then(|exe| File::create(exe.with_file_name("game-translate.log")).ok());
        Log { file: Mutex::new(file), start: Instant::now() }
    });
    if let Ok(mut file) = log.file.lock()
        && let Some(file) = file.as_mut()
    {
        let _ = writeln!(file, "[{:>8.1}s] {msg}", log.start.elapsed().as_secs_f32());
    }
}
