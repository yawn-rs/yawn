//! The router component, `<Outlet />`, `<Link />` and the hooks.

use crate::history::{BaseHistory, History, default_history, with_base};
use crate::location::Location;
use crate::routes::{Arena, Matched, Routes};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use yawn::data::Source;
use yawn::ui::rt::{self, switch};
use yawn::ui::{self, Children, Component, Event, Memo, Sig, View, memo, on_cleanup, signal};

struct State {
    history: Rc<dyn History>,
    location: Sig<Location>,
    matched: Memo<Matched>,
    arena: Rc<Arena>,
    base: String,
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    transitions: bool,
    nav: Cell<u64>,
    /// Bumped by every prefetch and navigation: a delayed prefetch runs only if it is still the latest
    prefetching: Cell<u64>,
    pending: Sig<bool>,
}

#[cfg(target_arch = "wasm32")]
const PRELOAD_WAIT_MS: i32 = 1500;
/// How long the pointer rests on a [`Link`] before its page is prefetched
const PREFETCH_HOVER_MS: i32 = 100;

impl State {
    #[cfg(target_arch = "wasm32")]
    fn preload_for(&self, url: &str) -> Vec<String> {
        let path_end = url.find(['?', '#']).unwrap_or(url.len());
        let (path, rest) = url.split_at(path_end);
        let stripped = match path.strip_prefix(&self.base) {
            Some(r) if !self.base.is_empty() && r.is_empty() => "/".to_string(),
            Some(r) if !self.base.is_empty() && r.starts_with('/') => r.to_string(),
            _ => path.to_string(),
        };
        let location = Location::parse(&format!("{stripped}{rest}"));
        let target = self.arena.resolve(&location.segments());
        let current = self.matched.peek();
        let aimed = Target {
            params: Params(memo({
                let target = target.clone();
                move || target.clone()
            })),
            query: Query(signal(location.clone())),
        };
        let mut names = Vec::new();
        for (depth, id) in target.chain.iter().enumerate() {
            let unchanged = current.chain.get(depth) == Some(id) && current.params == target.params;
            if !unchanged {
                yawn::dom::split_load(self.arena.nodes[*id].key, || {});
            }
            let Some(preload) = self.arena.nodes[*id].preload else {
                continue;
            };
            if !unchanged {
                for source in ui::untracked(|| preload(&aimed)) {
                    source.preload();
                    names.push(source.name().to_string());
                }
            }
        }
        names
    }

    fn go(&self, f: impl FnOnce() + 'static) {
        #[cfg(target_arch = "wasm32")]
        if self.transitions {
            return yawn::dom::with_view_transition(f);
        }
        f()
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
    /// Index into the matched chain of the next `<Outlet />` to be built
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
struct Depth(usize);

fn state() -> Rc<State> {
    CURRENT.with(|c| c.borrow().clone()).expect("yawn-router: no <Router> above this component")
}

/// Mounts the routes: renders the first matching route (inside its layouts) and follows the URL
pub struct Router {
    /// The route table: from `routes::table()` with folder routing, or built with [`Routes`]
    pub routes: Routes,
}

impl Component for Router {
    fn render(self) -> View {
        render_with(self.routes, default_history())
    }
}

/// [`Router`] with an explicit history, for tests and for embedding
pub fn render_with(routes: Routes, history: Rc<dyn History>) -> View {
    let base = routes.base.clone();
    let transitions = routes.transitions;
    let history: Rc<dyn History> =
        if base.is_empty() { history } else { Rc::new(BaseHistory::new(history, &base)) };
    let arena = Rc::new(routes.compile());
    let location = signal(history.current());
    let alive = Rc::new(Cell::new(true));
    {
        let (alive, history) = (alive.clone(), history.clone());
        let h = history.clone();
        h.subscribe(Box::new(move || {
            if alive.get() {
                let now = history.current();
                // changes come from outside the runtime too (back button)
                ui::run(|| location.set(now));
            }
        }));
    }
    let matched = {
        let arena = arena.clone();
        memo(move || arena.resolve(&location.get().segments()))
    };
    let st = Rc::new(State {
        history,
        location,
        matched,
        arena,
        base,
        transitions,
        nav: Cell::new(0),
        prefetching: Cell::new(0),
        pending: signal(false),
    });
    let mine = st.clone();
    let previous = CURRENT.with(|c| c.replace(Some(st)));
    on_cleanup(move || {
        alive.set(false);
        // A router built to replace this one may already be the current one: leave that alone.
        CURRENT.with(|c| {
            let mut c = c.borrow_mut();
            if c.as_ref().is_some_and(|cur| Rc::ptr_eq(cur, &mine)) {
                *c = previous;
            }
        });
    });
    #[cfg(not(target_arch = "wasm32"))]
    {
        // the server: what the matched routes need is asked for before any of them is built
        let chain = ui::untracked(st_matched_chain);
        let arena = state().arena.clone();
        let keys: Vec<&str> = chain.iter().map(|id| arena.nodes[*id].key).collect();
        yawn::ui::head::meta("yawn-routes", &keys.join("|"));
        let aimed = Target { params: Params(state().matched), query: Query(state().location) };
        for id in chain {
            if let Some(preload) = arena.nodes[id].preload {
                for source in ui::untracked(|| preload(&aimed)) {
                    source.prefetch();
                }
            }
        }
    }
    let prev_depth = DEPTH.replace(0);
    let view = outlet(0);
    DEPTH.set(prev_depth);
    view
}

#[cfg(not(target_arch = "wasm32"))]
fn st_matched_chain() -> Vec<usize> {
    state().matched.get().chain.clone()
}

fn outlet(depth: usize) -> View {
    let st = state();
    let key = st.clone();
    switch(
        move || key.matched.get().chain.get(depth).copied(),
        move |id| match id {
            None => View::empty(),
            Some(id) => {
                let build = st.arena.nodes[id].build.clone();
                yawn::context::provide(Depth(depth + 1));
                let before = DEPTH.replace(depth + 1);
                let view = build();
                DEPTH.set(before);
                view
            }
        },
    )
}

/// Where a layout shows the route below it
#[derive(Default)]
pub struct Outlet {}

impl Component for Outlet {
    fn render(self) -> View {
        outlet(yawn::context::use_context::<Depth>().map_or_else(|| DEPTH.get(), |d| d.0))
    }
}

/// A link that navigates without loading the page. It also prefetches what the page reads: after
/// the pointer rests on it for a moment, and at once on press or touch
pub struct Link {
    /// Where it goes: `/users/3`, `/about?x=1`, or an outside URL
    pub href: String,
    /// What the link shows
    pub children: Children,
}

fn trimmed(path: &str) -> &str {
    let t = path.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

impl Component for Link {
    fn render(self) -> View {
        let Link { href, children } = self;
        let a = rt::element("a");
        rt::attr(a, "href", &with_base(&state().base, &href));
        if href.starts_with('/') && !href.starts_with("//") {
            let nav = use_navigate();
            let location = use_location();
            let target = href.clone();
            let hover = target.clone();
            rt::listen(a, "mouseenter", move |_: Event| {
                nav.prefetch(&hover, Some(PREFETCH_HOVER_MS))
            });
            let press = target.clone();
            rt::listen(a, "mousedown", move |_: Event| nav.prefetch(&press, None));
            let touch = target.clone();
            rt::listen(a, "touchstart", move |_: Event| nav.prefetch(&touch, None));
            rt::listen(a, "click", move |e: Event| {
                if e.modified {
                    return;
                }
                e.prevent_default();
                nav.push(&target);
            });
            let own = Location::parse(&href).path;
            rt::bind_class(a, "active", move || trimmed(&location.get().path) == trimmed(&own));
        }
        rt::add(a, children.view());
        View::One(a)
    }
}

/// Moves around: the handle [`use_navigate`] returns
#[derive(Clone, Copy)]
pub struct Navigator;

impl Navigator {
    /// Goes to `url` (`/users/3?tab=posts`) and adds a history entry
    pub fn push(&self, url: &str) {
        let st = state();
        let ticket = st.nav.get() + 1;
        st.nav.set(ticket);
        st.prefetching.set(st.prefetching.get() + 1);
        st.pending.set(false);
        #[cfg(target_arch = "wasm32")]
        {
            let names = st.preload_for(url);
            if yawn::data::loading(&names) {
                let (h, url, moved) = (st.history.clone(), url.to_string(), st.clone());
                st.pending.set(true);
                yawn::data::when_loaded(&names, PRELOAD_WAIT_MS, move || {
                    if moved.nav.get() == ticket {
                        moved.pending.set(false);
                        moved.go(move || h.push(&url));
                    }
                });
                return;
            }
        }
        let (h, url) = (st.history.clone(), url.to_string());
        st.go(move || h.push(&url));
    }

    /// Starts downloading what the page at `url` will read, without going there: for a link that is
    /// hovered or touched, so the data is already on its way when it is clicked.
    ///
    /// With `Some(ms)` it waits that long first and gives up if another prefetch or a navigation
    /// came in the meantime, so only the last of a quick run of calls (the pointer passing over a
    /// list of links) reaches the server
    pub fn prefetch(&self, url: &str, delay_ms: Option<i32>) {
        let st = state();
        let ticket = st.prefetching.get() + 1;
        st.prefetching.set(ticket);
        #[cfg(target_arch = "wasm32")]
        match delay_ms {
            None => {
                st.preload_for(url);
            }
            Some(ms) => {
                let (url, moved) = (url.to_string(), st.clone());
                yawn::task::after(ms, move || {
                    if moved.prefetching.get() == ticket {
                        moved.preload_for(&url);
                    }
                });
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (url, delay_ms, ticket);
    }

    /// Goes to `url` in place of the current history entry
    pub fn replace(&self, url: &str) {
        state().history.replace(url);
    }

    /// Goes one entry back in the history, like the browser's back button
    pub fn back(&self) {
        state().history.back();
    }

    /// Goes one entry forward in the history
    pub fn forward(&self) {
        state().history.forward();
    }
}

/// The [`Navigator`]: `use_navigate().push("/users/3")`. Panics outside a [`Router`]
pub fn use_navigate() -> Navigator {
    // fail here, where the mistake is, and not later inside an event handler
    drop(state());
    Navigator
}

/// The current location; reading it in a view or effect follows navigation
#[derive(Clone, Copy)]
pub struct LocationHandle(Sig<Location>);

impl LocationHandle {
    /// The current location (path, query, hash)
    pub fn get(&self) -> Location {
        self.0.get()
    }
}

/// The current [`Location`] of the router above this component, as a reactive handle. Panics
/// outside a [`Router`]
pub fn use_location() -> LocationHandle {
    LocationHandle(state().location)
}

/// Path parameters of the matched routes (`:id`, `*rest`)
#[derive(Clone, Copy)]
pub struct Params(Memo<Matched>);

/// The address a route is being built for, as its `preload` sees it: the path parameters and the
/// query string
pub struct Target {
    /// The path parameters of the matched routes
    pub params: Params,
    /// The query string
    pub query: Query,
}

/// What a route's `preload` is: given the address it's being built for, the data it will read
pub type PreloadFn = fn(&Target) -> Vec<Source>;

impl Params {
    /// Value of the path parameter `name` (`:id` in `/users/:id`, or the name of a `*splat`),
    /// percent-decoded
    pub fn get(&self, name: &str) -> String {
        self.find(name).unwrap_or_default()
    }

    /// Like [`get`](Self::get), but `None` when the matched routes have no such parameter
    pub fn find(&self, name: &str) -> Option<String> {
        self.0.get().params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    /// All the parameters, in the order of the address (a layout's before its page's), to take
    /// apart: `let (id, module) = use_params().values();`
    pub fn values<T: FromParams>(&self) -> T {
        T::from_params(self.0.get().params.iter().map(|(_, v)| v.clone()).collect())
    }

    /// Several parameters at once, to take apart: `let [id, module] = use_params().take(["id",
    /// "module"]);`
    pub fn take<const N: usize>(&self, names: [&str; N]) -> [String; N] {
        names.map(|name| self.get(name))
    }
}

/// A tuple of texts that [`Params::values`] can fill, from one to six
pub trait FromParams {
    #[doc(hidden)]
    fn from_params(values: Vec<String>) -> Self;
}

macro_rules! from_params {
    ($($name:ident),+) => {
        impl FromParams for ($(from_params!(@text $name),)+) {
            fn from_params(values: Vec<String>) -> Self {
                let mut values = values.into_iter();
                ($({ let $name = values.next().unwrap_or_default(); $name },)+)
            }
        }
    };
    (@text $name:ident) => { String };
}

from_params!(a);
from_params!(a, b);
from_params!(a, b, c);
from_params!(a, b, c, d);
from_params!(a, b, c, d, e);
from_params!(a, b, c, d, e, f);

/// Path parameters of the routes that match now: `use_params().get("id")` is the text,
/// `.find("id")` an `Option` of it. Panics outside a [`Router`]
pub fn use_params() -> Params {
    Params(state().matched)
}

/// Whether a navigation is waiting for the data of the page it goes to (a progress bar can show
/// it). Panics outside a [`Router`]
pub fn use_navigating() -> Sig<bool> {
    state().pending
}

/// Query string parameters (`?tab=posts`); reading follows navigation
#[derive(Clone, Copy)]
pub struct Query(Sig<Location>);

impl Query {
    /// Value of the first `name=` in the query string, percent-decoded, or `None`
    pub fn get(&self, name: &str) -> Option<String> {
        self.0.get().query_param(name)
    }
}

/// Query string parameters of the current URL: `use_query().get("tab")`. Panics outside a
/// [`Router`]
pub fn use_query() -> Query {
    Query(state().location)
}
