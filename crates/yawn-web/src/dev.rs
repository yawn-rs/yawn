use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

type Hook = Box<dyn Fn(&Path) + Send + Sync>;

static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

/// Calls `hook` with the path of the file each time a file under a `[dev] watch` directory changes,
/// before the browser reloads
pub fn on_data_change(hook: impl Fn(&Path) + Send + Sync + 'static) {
    HOOKS.lock().unwrap().push(Box::new(hook));
}

pub(crate) fn data_changed(path: &Path) {
    for hook in HOOKS.lock().unwrap().iter() {
        hook(path);
    }
}

#[derive(Default)]
struct Literals {
    path: Option<PathBuf>,
    stamp: Option<SystemTime>,
    values: HashMap<String, &'static str>,
    leaked: HashMap<String, &'static str>,
}

static LITERALS: RwLock<Option<Literals>> = RwLock::new(None);

pub(crate) fn refresh_literals() {
    let mut guard = LITERALS.write().unwrap();
    let table = guard.get_or_insert_with(|| {
        let dev = std::env::var("YAWN_DEV").is_ok_and(|v| v != "0");
        let path = dev
            .then(|| std::env::var("YAWN_DEV_DIR").ok())
            .flatten()
            .map(|d| PathBuf::from(d).join("literals-server.json"));
        Literals { path, ..Default::default() }
    });
    let Some(path) = table.path.clone() else {
        return;
    };
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if stamp == table.stamp {
        return;
    }
    table.stamp = stamp;
    let read: HashMap<String, String> =
        std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut values = HashMap::with_capacity(read.len());
    for (key, value) in read {
        let leaked =
            *table.leaked.entry(value.clone()).or_insert_with(|| Box::leak(value.into_boxed_str()));
        values.insert(key, leaked);
    }
    table.values = values;
}

pub(crate) fn server_literal(kind: &str, value: &'static str) -> &'static str {
    let guard = LITERALS.read().unwrap();
    let Some(table) = guard.as_ref().filter(|t| !t.values.is_empty()) else {
        return value;
    };
    table.values.get(&format!("{kind}:{value}")).copied().unwrap_or(value)
}

static NATIVE: Mutex<(Option<SystemTime>, Vec<String>)> = Mutex::new((None, Vec::new()));

/// Loads the route libraries `yawn dev` built since the last request (`native.json` in
/// `YAWN_DEV_DIR`). A library registers the component bodies it holds when it is loaded (see
/// `yawn::dynamic`), so the server serves the new code without restarting.
#[cfg(unix)]
pub(crate) fn refresh_native() {
    if !std::env::var("YAWN_DEV").is_ok_and(|v| v != "0") {
        return;
    }
    let Ok(dir) = std::env::var("YAWN_DEV_DIR") else { return };
    let path = PathBuf::from(dir).join("native.json");
    let Some(stamp) = std::fs::metadata(&path).and_then(|m| m.modified()).ok() else { return };
    let mut state = NATIVE.lock().unwrap_or_else(|e| e.into_inner());
    if state.0 == Some(stamp) {
        return;
    }
    state.0 = Some(stamp);
    let Some(list) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
    else {
        return;
    };
    for file in list {
        if state.1.contains(&file) {
            continue;
        }
        let Ok(c) = std::ffi::CString::new(file.clone()) else { continue };
        // the handle is never closed: bodies the library registered may still be running
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            let err = unsafe { libc::dlerror() };
            let why = if err.is_null() {
                String::new()
            } else {
                unsafe { std::ffi::CStr::from_ptr(err) }.to_string_lossy().into_owned()
            };
            eprintln!("yawn: could not load {file}: {why}");
        }
        state.1.push(file);
    }
}

#[cfg(not(unix))]
pub(crate) fn refresh_native() {}
