//! Replaces the body of a component while the page runs (dev builds only).
//!
//! With `[dev] dynamic` a generated component renders through [`entry`]: when a body was
//! [`register`]ed under the component's type name it runs that one, otherwise the body compiled
//! into the app does. A module loaded later (see `yawn dev`) registers new bodies, so editing a
//! component rebuilds that module and not the app.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn bodies() -> &'static Mutex<HashMap<String, usize>> {
    static BODIES: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    BODIES.get_or_init(Default::default)
}

#[doc(hidden)]
#[inline(never)]
pub fn register_raw(name: &str, body: usize) {
    bodies().lock().unwrap_or_else(|e| e.into_inner()).insert(name.to_string(), body);
}

#[doc(hidden)]
#[inline(never)]
pub fn entry_raw(name: &str) -> Option<usize> {
    bodies().lock().unwrap_or_else(|e| e.into_inner()).get(name).copied()
}

/// Makes `body` the one `T` renders with from now on
pub fn register<T>(name: &str, body: fn(T) -> crate::ui::View) {
    register_raw(name, body as usize);
}

/// The body registered for `T`, if a loaded module replaced it
pub fn entry<T>(name: &str) -> Option<fn(T) -> crate::ui::View> {
    // SAFETY: only `register` stores values, and it stores a `fn(T) -> View` under the name of
    // exactly that `T`.
    entry_raw(name).map(|p| unsafe { std::mem::transmute::<usize, fn(T) -> crate::ui::View>(p) })
}

/// Memory for the data and stack of a module loaded later (see `yawn dev`)
#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
#[unsafe(no_mangle)]
pub extern "C" fn __yawn_alloc(size: usize, align: usize) -> *mut u8 {
    // SAFETY: the loader passes a non-zero size and a power of two alignment
    unsafe { std::alloc::alloc(std::alloc::Layout::from_size_align_unchecked(size, align)) }
}
