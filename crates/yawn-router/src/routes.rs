//! The route table: patterns, nesting and matching.

use crate::router::PreloadFn;
use std::rc::Rc;
use yawn::ui::{Component, View};

/// One segment of a pattern
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    /// `users`
    Static(String),
    /// `:id`
    Param(String),
    /// `*rest`: everything that's left, possibly nothing
    Splat(String),
}

/// A path pattern such as `/users/:id` or `/docs/*path`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    /// The segments in order
    pub segs: Vec<Seg>,
}

impl Pattern {
    /// Reads `/users/:id` or `/docs/*path` (`*` alone is `*rest`)
    pub fn parse(pattern: &str) -> Pattern {
        let mut segs = Vec::new();
        for s in pattern.split('/').filter(|s| !s.is_empty()) {
            if let Some(name) = s.strip_prefix(':') {
                segs.push(Seg::Param(name.to_string()));
            } else if let Some(name) = s.strip_prefix('*') {
                segs.push(Seg::Splat(if name.is_empty() {
                    "rest".into()
                } else {
                    name.to_string()
                }));
                break;
            } else {
                segs.push(Seg::Static(s.to_string()));
            }
        }
        Pattern { segs }
    }

    fn rank(&self) -> Vec<u8> {
        let mut r: Vec<u8> = self
            .segs
            .iter()
            .map(|s| match s {
                Seg::Static(_) => 2,
                Seg::Param(_) => 1,
                Seg::Splat(_) => 0,
            })
            .collect();
        if !matches!(self.segs.last(), Some(Seg::Splat(_))) {
            r.push(3);
        }
        r
    }

    fn match_prefix(&self, path: &[String]) -> Option<(usize, Vec<(String, String)>)> {
        let mut params = Vec::new();
        for (i, seg) in self.segs.iter().enumerate() {
            match seg {
                Seg::Static(s) => {
                    if path.get(i)? != s {
                        return None;
                    }
                }
                Seg::Param(name) => params.push((name.clone(), path.get(i)?.clone())),
                Seg::Splat(name) => {
                    params.push((name.clone(), path[i.min(path.len())..].join("/")));
                    return Some((path.len(), params));
                }
            }
        }
        Some((self.segs.len(), params))
    }
}

type Build = Rc<dyn Fn() -> View>;
type Params = Vec<(String, String)>;

/// One function per route component, never inlined: it is the point where a build can cut a
/// route's code out of the main module
#[inline(never)]
fn route_entry<C: Component + Default + 'static>() -> View {
    C::default().render()
}

/// What the router calls to build a route: in a build that splits the wasm into route modules, a
/// route whose module has not arrived yet shows nothing and appears when it has
fn gated<C: Component + Default + 'static>() -> View {
    #[cfg(target_arch = "wasm32")]
    {
        let key = std::any::type_name::<C>();
        if !yawn::dom::split_ready(key) {
            let ready = yawn::ui::signal(false);
            yawn::dom::split_load(key, move || yawn::ui::run(|| ready.set(true)));
            return yawn::ui::rt::switch(
                move || ready.get(),
                move |loaded| if loaded { route_entry::<C>() } else { View::empty() },
            );
        }
    }
    route_entry::<C>()
}

fn builder<C: Component + Default + 'static>() -> Build {
    Rc::new(gated::<C>)
}

enum Entry {
    Page {
        pattern: Pattern,
        build: Build,
        key: &'static str,
        preload: Option<PreloadFn>,
    },
    Layout {
        pattern: Pattern,
        build: Build,
        key: &'static str,
        children: Routes,
        preload: Option<PreloadFn>,
    },
}

/// A table of routes. Pages show for their path; a layout wraps the routes inside it and places
/// the matching one where it renders `<Outlet />`.
///
/// ```ignore
/// Routes::new()
///     .layout::<Shell>("", |r| r
///         .at::<Home>("/")
///         .at::<User>("/users/:id")
///         .not_found::<Missing>())
/// ```
///
/// Components used as pages and layouts take no props (they're built with `Default`).
#[derive(Default)]
pub struct Routes {
    entries: Vec<Entry>,
    pub(crate) base: String,
    pub(crate) transitions: bool,
}

impl Routes {
    /// Moves between pages inside the browser's view transition, so CSS can animate the change
    /// (`::view-transition-old(root)` and `::view-transition-new(root)`)
    pub fn transitions(mut self) -> Routes {
        self.transitions = true;
        self
    }

    /// An empty table
    pub fn new() -> Routes {
        Routes::default()
    }

    /// Serves the table under a path prefix: with `.base("/pl")` the address bar shows `/pl/guilds`
    /// while routes are written (and matched) as `/guilds`; links and navigation add the prefix
    /// themselves
    pub fn base(mut self, base: &str) -> Routes {
        self.base = base.trim_end_matches('/').to_string();
        self
    }

    /// A page at `pattern`, relative to the layout it's in
    pub fn at<C: Component + Default + 'static>(mut self, pattern: &str) -> Routes {
        self.entries.push(Entry::Page {
            pattern: Pattern::parse(pattern),
            build: builder::<C>(),
            key: std::any::type_name::<C>(),
            preload: None,
        });
        self
    }

    /// A layout for the routes `inner` adds, under `pattern` (use `""` for none)
    pub fn layout<C: Component + Default + 'static>(
        mut self,
        pattern: &str,
        inner: impl FnOnce(Routes) -> Routes,
    ) -> Routes {
        self.entries.push(Entry::Layout {
            pattern: Pattern::parse(pattern),
            build: builder::<C>(),
            key: std::any::type_name::<C>(),
            children: inner(Routes::new()),
            preload: None,
        });
        self
    }

    /// The data the route added last will read, as sources (`loader.into()`): `f` gets the address
    /// the route is built for (parameters and query) and returns them, and the router asks for them
    /// before the route is built, on the server while the page renders and in the browser while a
    /// navigation to it waits, so the page finds its data at the first paint either way
    pub fn preload(mut self, f: PreloadFn) -> Routes {
        match self.entries.last_mut() {
            Some(Entry::Page { preload, .. }) | Some(Entry::Layout { preload, .. }) => {
                *preload = Some(f)
            }
            None => {}
        }
        self
    }

    /// The page for paths nothing else matches (inside the layout it's declared in)
    pub fn not_found<C: Component + Default + 'static>(self) -> Routes {
        self.at::<C>("/*")
    }

    pub(crate) fn compile(self) -> Arena {
        let mut arena = Arena::default();
        arena.roots = arena.add_all(self);
        arena
    }
}

pub(crate) struct Node {
    pattern: Pattern,
    pub build: Build,
    /// The component's `type_name`, which names its route module in a split build
    pub key: &'static str,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub preload: Option<PreloadFn>,
    children: Option<Vec<usize>>,
}

#[derive(Default)]
pub(crate) struct Arena {
    pub nodes: Vec<Node>,
    roots: Vec<usize>,
}

/// The routes a path matched, outermost first, and the parameters they bound
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matched {
    pub(crate) chain: Vec<usize>,
    pub(crate) params: Vec<(String, String)>,
}

impl Arena {
    fn add_all(&mut self, routes: Routes) -> Vec<usize> {
        let mut ids = Vec::new();
        for e in routes.entries {
            let node = match e {
                Entry::Page { pattern, build, key, preload } => {
                    Node { pattern, build, key, preload, children: None }
                }
                Entry::Layout { pattern, build, key, children, preload } => {
                    let kids = self.add_all(children);
                    Node { pattern, build, key, preload, children: Some(kids) }
                }
            };
            self.nodes.push(node);
            ids.push(self.nodes.len() - 1);
        }
        // Stable: equally specific routes keep the order they were declared in.
        ids.sort_by(|a, b| self.nodes[*b].pattern.rank().cmp(&self.nodes[*a].pattern.rank()));
        ids
    }

    pub fn resolve(&self, path: &[String]) -> Matched {
        match self.walk(&self.roots, path) {
            Some((chain, params)) => Matched { chain, params },
            None => Matched::default(),
        }
    }

    fn walk(&self, ids: &[usize], path: &[String]) -> Option<(Vec<usize>, Params)> {
        for &id in ids {
            let node = &self.nodes[id];
            let Some((used, mut params)) = node.pattern.match_prefix(path) else {
                continue;
            };
            match &node.children {
                None if used == path.len() => return Some((vec![id], params)),
                None => {}
                Some(kids) => {
                    if let Some((mut chain, more)) = self.walk(kids, &path[used..]) {
                        chain.insert(0, id);
                        params.extend(more);
                        return Some((chain, params));
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::Location;

    #[derive(Default)]
    struct P;
    impl Component for P {
        fn render(self) -> View {
            View::empty()
        }
    }

    fn resolve(routes: Routes, url: &str) -> (Vec<usize>, Vec<(String, String)>) {
        let m = routes.compile().resolve(&Location::parse(url).segments());
        (m.chain, m.params)
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn static_beats_param_beats_splat_whatever_the_order() {
        let r =
            || Routes::new().at::<P>("/*").at::<P>("/users/:id").at::<P>("/users/new").at::<P>("/");
        assert_eq!(resolve(r(), "/users/new").0, [2]);
        assert_eq!(resolve(r(), "/users/7"), (vec![1], p(&[("id", "7")])));
        assert_eq!(resolve(r(), "/").0, [3]);
        assert_eq!(resolve(r(), "/nope/deeper"), (vec![0], p(&[("rest", "nope/deeper")])));
    }

    #[test]
    fn layouts_nest_and_collect_params() {
        let r = Routes::new().layout::<P>("", |r| {
            r.at::<P>("/")
                .layout::<P>("/users/:id", |r| r.at::<P>("/").at::<P>("/posts/:post"))
                .not_found::<P>()
        });
        let (chain, params) = resolve(r, "/users/4/posts/9");
        assert_eq!(chain.len(), 3);
        assert_eq!(params, p(&[("id", "4"), ("post", "9")]));
    }

    #[test]
    fn unknown_paths_without_not_found_match_nothing() {
        assert_eq!(resolve(Routes::new().at::<P>("/a"), "/b"), (vec![], vec![]));
        assert_eq!(resolve(Routes::new().at::<P>("/a"), "/a/").0, [0]);
        assert_eq!(resolve(Routes::new().at::<P>("/a"), "/a/b").0, Vec::<usize>::new());
    }

    #[test]
    fn failed_layout_children_fall_through_to_the_next_route() {
        let r = Routes::new().layout::<P>("/admin", |r| r.at::<P>("/users")).at::<P>("/*");
        let (chain, _) = resolve(r, "/admin/other");
        assert_eq!(chain.len(), 1, "the catch-all page, not the admin layout");
    }
}
