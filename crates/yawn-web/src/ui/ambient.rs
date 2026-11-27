use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::ptr;
use yawn_reactivity::{Runtime, Scope};

thread_local! {
    static CURRENT: Cell<*mut Runtime> = const { Cell::new(ptr::null_mut()) };
    static ROOT: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Keeps a runtime published as the ambient one
pub struct Entered<'a> {
    prev: *mut Runtime,
    _borrow: PhantomData<&'a mut Runtime>,
}

pub fn enter(rt: &mut Runtime) -> Entered<'_> {
    let prev = CURRENT.with(|c| c.replace(rt as *mut Runtime));
    Entered { prev, _borrow: PhantomData }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.prev));
    }
}

/// Whether code is currently running inside the runtime
pub fn is_active() -> bool {
    CURRENT.with(|c| !c.get().is_null())
}

/// Runs `f` with the ambient runtime. Panics when there is none (signals used outside render,
/// effects and event handlers) or when called from inside another `with` closure
#[track_caller]
pub fn with<R>(f: impl FnOnce(&mut Runtime) -> R) -> R {
    let (mut f, mut out) = (Some(f), None);
    with_dyn(&mut |rt| out = Some((f.take().expect("runs once"))(rt)));
    out.expect("ran")
}

#[inline(never)]
#[track_caller]
fn with_dyn(f: &mut dyn FnMut(&mut Runtime)) {
    let mut taken = take();
    f(taken.runtime());
}

struct Taken(*mut Runtime);

impl Taken {
    fn runtime(&mut self) -> &mut Runtime {
        // SAFETY: the pointer comes from a live `&mut Runtime` in `enter`, whose guard outlives
        // this one, and `CURRENT` is null while `self` lives
        unsafe { &mut *self.0 }
    }
}

impl Drop for Taken {
    #[inline(never)]
    fn drop(&mut self) {
        CURRENT.with(|c| c.set(self.0));
    }
}

#[inline(never)]
#[track_caller]
fn take() -> Taken {
    let ptr = CURRENT.with(|c| c.replace(ptr::null_mut()));
    assert!(
        !ptr.is_null(),
        "yawn: no active runtime. Signals can only be used while rendering a component, in effects, \
         memos and event handlers (and not inside a closure passed to `Sig::update` or `Sig::with`)"
    );
    Taken(ptr)
}

pub(crate) fn install_root(rt: Runtime) {
    ROOT.with(|r| *r.borrow_mut() = Some(rt));
}

pub(crate) fn clear_root() {
    ROOT.with(|r| *r.borrow_mut() = None);
}

/// Whether an application is mounted, so [`run`] has a runtime to enter
pub fn is_mounted() -> bool {
    is_active() || ROOT.with(|cell| cell.try_borrow().map(|r| r.is_some()).unwrap_or(false))
}

/// [`run`] for what may outlive the page: a timer, a listener or a request answered after the
/// application was replaced or dropped (a hot reload does that)
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn run_if_mounted<R>(f: impl FnOnce() -> R) -> Option<R> {
    if is_mounted() { Some(run(f)) } else { None }
}

/// Runs `f` with the runtime entered: from inside the runtime when already running, otherwise from
/// the installed application runtime
pub fn run<R>(f: impl FnOnce() -> R) -> R {
    let (mut f, mut out) = (Some(f), None);
    run_dyn(&mut || out = Some((f.take().expect("runs once"))()));
    out.expect("ran")
}

#[inline(never)]
fn run_dyn(f: &mut dyn FnMut()) {
    if is_active() {
        with_dyn(&mut |rt| {
            let _g = enter(rt);
            f()
        });
        return;
    }
    ROOT.with(|cell| {
        let mut slot = cell.try_borrow_mut().expect("yawn: the runtime is already in use");
        let rt = slot.as_mut().expect("yawn: no app is mounted");
        let _g = enter(rt);
        f()
    })
}

/// The component, effect or branch whose code is running now: what a handler, timer or answer from
/// the network registered inside it hands to [`run_in_owner`] when it comes back later
pub fn capture_owner() -> Option<Scope> {
    if is_active() { with(|rt| rt.current_scope()) } else { None }
}

/// [`run_if_mounted`] inside `owner`, so the code is part of the component that asked for it: it
/// can start work that ends with it and register cleanups
pub fn run_in_owner<R>(owner: Option<Scope>, f: impl FnOnce() -> R) -> Option<R> {
    let (mut f, mut out) = (Some(f), None);
    run_in_owner_dyn(owner, &mut || out = Some((f.take().expect("runs once"))()));
    out
}

#[inline(never)]
fn run_in_owner_dyn(owner: Option<Scope>, f: &mut dyn FnMut()) {
    match owner {
        None => {
            if is_mounted() {
                run_dyn(f);
            }
        }
        Some(owner) => {
            if !is_mounted() {
                return;
            }
            run_dyn(&mut || {
                with_dyn(&mut |rt| {
                    let _ = rt.run_in(owner, |rt| {
                        let _g = enter(rt);
                        f()
                    });
                })
            });
        }
    }
}
