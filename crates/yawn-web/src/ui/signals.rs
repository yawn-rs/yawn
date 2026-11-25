use super::ambient::{enter, with};
use std::fmt;
use yawn_reactivity as r;

/// A writable reactive value
pub struct Sig<T: 'static>(r::Signal<T>);

impl<T: 'static> Clone for Sig<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: 'static> Copy for Sig<T> {}

/// A derived reactive value
pub struct Memo<T: 'static>(r::Memo<T>);

impl<T: 'static> Clone for Memo<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: 'static> Copy for Memo<T> {}

impl<T: Clone + 'static> Sig<T> {
    /// The current value; inside an effect or memo this subscribes to changes
    #[track_caller]
    pub fn get(self) -> T {
        with(|rt| rt.get(self.0))
    }

    /// The current value without subscribing
    #[track_caller]
    pub fn peek(self) -> T {
        with(|rt| rt.peek(self.0))
    }
}

impl<T: 'static> Sig<T> {
    #[track_caller]
    pub fn set(self, value: T) {
        // a handler, timer or answer from the network may come after what it writes to is gone: that's no reason to stop the page
        with(|rt| {
            if rt.in_handler() {
                let _ = rt.try_set(self.0, value);
            } else {
                rt.set(self.0, value);
            }
        });
    }

    /// [`update`](Self::update) that does nothing when the signal is already gone: for a cleanup,
    /// which may run after the scope that owns the signal was dropped
    pub fn try_update(self, f: impl FnOnce(&mut T)) {
        let Some(mut taken) = with(|rt| rt.take_signal(self.0, true)) else {
            return;
        };
        taken.update::<T, _>(f);
        with(|rt| rt.put_signal(taken, true));
    }

    /// Mutates in place
    #[track_caller]
    pub fn update(self, f: impl FnOnce(&mut T)) {
        let taken = with(|rt| {
            if rt.in_handler() {
                rt.take_signal(self.0, true)
            } else {
                Some(rt.take_signal(self.0, true).expect("write to a disposed signal"))
            }
        });
        let Some(mut taken) = taken else { return };
        taken.update::<T, _>(f);
        with(|rt| rt.put_signal(taken, true));
    }

    /// Whether the signal still exists: a signal made inside a component is gone with it
    pub fn is_alive(self) -> bool {
        let Some(taken) = with(|rt| rt.take_signal(self.0, false)) else {
            return false;
        };
        with(|rt| rt.put_signal(taken, false));
        true
    }

    pub(crate) fn try_encode(self) -> Option<String> {
        let taken = with(|rt| rt.take_signal(self.0, false))?;
        let out = taken.read::<T, _>(|v| super::keep::encode(v));
        with(|rt| rt.put_signal(taken, false));
        out
    }

    /// Reads by reference, subscribing like `get`
    #[track_caller]
    pub fn with<O>(self, f: impl FnOnce(&T) -> O) -> O {
        let taken = with(|rt| rt.take_signal(self.0, false)).expect("read of a disposed signal");
        let out = taken.read::<T, _>(f);
        with(|rt| rt.put_signal(taken, false));
        out
    }
}

impl<T: Clone + 'static> Memo<T> {
    /// The current value without subscribing
    #[track_caller]
    pub fn peek(self) -> T {
        with(|rt| rt.peek(self.0))
    }

    /// The current value; inside an effect or memo this subscribes to changes
    #[track_caller]
    pub fn get(self) -> T {
        with(|rt| rt.get(self.0))
    }
}

impl<T: fmt::Display + 'static> fmt::Display for Sig<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with(|v| v.fmt(f))
    }
}

impl<T: fmt::Display + Clone + 'static> fmt::Display for Memo<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(f)
    }
}

/// A new reactive value: `let count = signal(0);`
#[track_caller]
pub fn signal<T: PartialEq + 'static>(value: T) -> Sig<T> {
    if super::keep::active() && super::keep::supported::<T>() {
        let initial = super::keep::encode(&value);
        let (key, saved) =
            super::keep::claim::<T>(std::panic::Location::caller(), initial.as_deref());
        let made = Sig(with(|rt| rt.signal(saved.unwrap_or(value))));
        super::keep::register(key, Box::new(move || made.try_encode()), initial);
        return made;
    }
    Sig(with(|rt| rt.signal(value)))
}

/// A signal that belongs to the application: it survives navigation and is never disposed
/// while the application runs. Declare it in a `thread_local!` and it's made on first use (again if the application was replaced):
///
/// ```ignore
/// thread_local! { static COUNT: GlobalSig<i32> = GlobalSig::new(|| 0); }
/// COUNT.with(|c| c.set(c.get() + 1));
/// ```
pub struct GlobalSig<T: PartialEq + Clone + 'static> {
    init: fn() -> T,
    slot: std::cell::Cell<Option<(u32, Sig<T>)>>,
}

impl<T: PartialEq + Clone + 'static> GlobalSig<T> {
    pub const fn new(init: fn() -> T) -> Self {
        GlobalSig { init, slot: std::cell::Cell::new(None) }
    }

    /// The signal itself, made now when there is none for the running application
    pub fn sig(&self) -> Sig<T> {
        let rt_id = with(|rt| rt.id());
        if let Some((id, sig)) = self.slot.get()
            && id == rt_id
        {
            return sig;
        }
        let made = Sig(with(|rt| rt.signal_global((self.init)())));
        self.slot.set(Some((rt_id, made)));
        made
    }

    #[track_caller]
    pub fn get(&self) -> T {
        self.sig().get()
    }

    #[track_caller]
    pub fn peek(&self) -> T {
        self.sig().peek()
    }

    #[track_caller]
    pub fn set(&self, value: T) {
        self.sig().set(value);
    }
}

/// A signal that notifies on every `set`, even when the value is equal
#[track_caller]
pub fn signal_always<T: 'static>(value: T) -> Sig<T> {
    Sig(with(|rt| rt.signal_with(value, |_, _| false)))
}

/// A value computed from other signals and cached: `let double = memo(move || count.get() * 2);`
#[track_caller]
pub fn memo<T: PartialEq + 'static>(mut f: impl FnMut() -> T + 'static) -> Memo<T> {
    Memo(with(|rt| {
        rt.memo(move |rt| {
            let _g = enter(rt);
            f()
        })
    }))
}

/// Runs `f` now and again whenever a signal or memo it read changes
#[track_caller]
pub fn effect(f: impl FnMut() + 'static) {
    effect_dyn(Box::new(f));
}

// The closures of a view are all different types. The functions they're passed to box them and
// pass them on to code that isn't generic, which the compiler then builds once.
#[inline(never)]
fn effect_dyn(mut f: Box<dyn FnMut()>) {
    with(|rt| {
        rt.effect(move |rt| {
            let _g = enter(rt);
            f()
        })
    });
}

/// Defers effects until `f` returns, so several `set`s cause one update
#[track_caller]
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    with(|rt| {
        rt.batch(|rt| {
            let _g = enter(rt);
            f()
        })
    })
}

/// Runs `f` without subscribing to what it reads
#[track_caller]
pub fn untracked<R>(f: impl FnOnce() -> R) -> R {
    let (mut f, mut out) = (Some(f), None);
    untracked_dyn(&mut || out = Some((f.take().expect("runs once"))()));
    out.expect("ran")
}

#[inline(never)]
fn untracked_dyn(f: &mut dyn FnMut()) {
    with(|rt| {
        rt.untracked(|rt| {
            let _g = enter(rt);
            f()
        })
    })
}

/// Runs `f` when the enclosing component, branch or row is disposed or the effect re-runs
#[track_caller]
pub fn on_cleanup(f: impl FnOnce() + 'static) {
    on_cleanup_dyn(Box::new(f));
}

#[inline(never)]
fn on_cleanup_dyn(f: Box<dyn FnOnce()>) {
    with(|rt| {
        rt.on_cleanup_rt(move |rt| {
            let _g = enter(rt);
            f()
        })
    });
}

/// Number of live reactive nodes (signals, memos, effects, scopes)
pub fn node_count() -> usize {
    with(|rt| rt.node_count())
}
