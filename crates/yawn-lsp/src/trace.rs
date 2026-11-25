use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init() {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")));
    if let Some(dir) = base.map(|b| b.join("yawn-lsp")) {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(f) = File::create(dir.join("lsp.log")) {
            let _ = FILE.set(Mutex::new(f));
        }
    }
}

pub fn write(msg: &str) {
    if let Some(f) = FILE.get()
        && let Ok(mut f) = f.lock()
    {
        let _ = writeln!(f, "{msg}");
    }
}

macro_rules! trace {
    ($($t:tt)*) => { $crate::trace::write(&format!($($t)*)) };
}
pub(crate) use trace;
