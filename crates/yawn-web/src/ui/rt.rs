use super::ambient::{self, enter, with};
use super::backend::{Backend, Event, Handler, NodeId};
use super::signals::{batch, effect, memo, on_cleanup, untracked};
use super::view::{AttrValue, Child, IntoChild, Parent};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::rc::Rc;
use yawn_reactivity::{Runtime, Scope};

pub use super::view::{Frag, View};

thread_local! {
    static ROOT_SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
    static BACKEND: RefCell<Option<Box<dyn Backend>>> = const { RefCell::new(None) };
    /// Node ids created by the build in progress; released when its owner is disposed
    static REGIONS: RefCell<Vec<Vec<NodeId>>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn with_backend<R>(f: impl FnOnce(&mut dyn Backend) -> R) -> R {
    let (mut f, mut out) = (Some(f), None);
    with_backend_dyn(&mut |b| out = Some((f.take().expect("runs once"))(b)));
    out.expect("ran")
}

#[inline(never)]
fn with_backend_dyn(f: &mut dyn FnMut(&mut dyn Backend)) {
    BACKEND.with(|b| {
        let mut b = b.try_borrow_mut().expect("yawn: the document backend is already in use");
        f(b.as_mut().expect("yawn: no document: mount an app first").as_mut())
    })
}

fn track(id: NodeId) -> NodeId {
    REGIONS.with(|r| {
        if let Some(top) = r.borrow_mut().last_mut() {
            top.push(id);
        }
    });
    id
}

fn in_region<R>(f: impl FnOnce() -> R) -> R {
    region_begin();
    let out = f();
    region_end();
    out
}

#[inline(never)]
fn region_begin() {
    REGIONS.with(|r| r.borrow_mut().push(Vec::new()));
}

#[inline(never)]
fn region_end() {
    let ids = REGIONS.with(|r| r.borrow_mut().pop()).unwrap_or_default();
    if !ids.is_empty() {
        on_cleanup(move || with_backend(|b| b.release(&ids)));
    }
}

pub fn add(parent: impl Parent, view: impl Into<View>) {
    parent.add(view.into());
}

/// Returns `props`. The generated `<List items={v} />` passes the marker its `List::__default_sep`
/// calls took, so the `T` of `List<T>` they need is the one `items` decides
pub fn with_defaults<C>(_: std::marker::PhantomData<C>, props: C) -> C {
    props
}

pub fn element(tag: &str) -> NodeId {
    track(with_backend(|b| b.create_element(tag)))
}

/// A subtree of literals only, from markup the `.rsx` compiler wrote
pub fn template(html: &'static str) -> NodeId {
    #[cfg(target_arch = "wasm32")]
    let original = html;
    let html = dev_literal("template", html);
    let node = track(with_backend(|b| b.create_template(html)));
    #[cfg(target_arch = "wasm32")]
    with_backend(|b| b.track_literal("template", original, html, node));
    node
}

/// A subtree of literals with some places left to fill in: returns the root and the nodes `paths`
/// lead to; the generated code binds the dynamic parts to them
pub fn template_holes<const N: usize>(
    html: &'static str,
    paths: &'static [&'static [u8]; N],
) -> [NodeId; N] {
    #[cfg(target_arch = "wasm32")]
    let original = html;
    let html = dev_literal("template", html);
    let mut out = [NodeId(0); N];
    with_backend(|b| b.create_template_holes(html, paths, &mut out));
    #[cfg(target_arch = "wasm32")]
    if let Some(index) = paths.iter().position(|p| p.is_empty()) {
        with_backend(|b| b.track_literal("template", original, html, out[index]));
    }
    for id in out {
        track(id);
    }
    out
}

pub fn text(text: &str) -> NodeId {
    track(with_backend(|b| b.create_text(text)))
}

/// A text node for a literal of the source (what generated code uses)
pub fn text_lit(text: &'static str) -> NodeId {
    #[cfg(target_arch = "wasm32")]
    let original = text;
    let text = dev_literal("text", text);
    let node = track(with_backend(|b| b.create_text_lit(text)));
    #[cfg(target_arch = "wasm32")]
    with_backend(|b| b.track_literal("text", original, text, node));
    node
}

fn comment() -> NodeId {
    track(with_backend(|b| b.create_comment()))
}

/// Text that follows the signals `f` reads
pub fn text_fmt(f: impl FnMut() -> String + 'static) -> NodeId {
    text_fmt_dyn(Box::new(f))
}

#[inline(never)]
fn text_fmt_dyn(mut f: Box<dyn FnMut() -> String>) -> NodeId {
    let node = text("");
    let mut last: Option<String> = None;
    effect(move || {
        let s = f();
        if last.as_deref() != Some(s.as_str()) {
            with_backend(|b| b.set_text(node, &s));
            last = Some(s);
        }
    });
    node
}

/// `{expr}` in a view
pub fn dyn_child<C: IntoChild>(mut f: impl FnMut() -> C + 'static) -> View {
    dyn_child_dyn(Box::new(move || f().into_child()))
}

#[inline(never)]
fn dyn_child_dyn(mut f: Box<dyn FnMut() -> Child>) -> View {
    match untracked(&mut f) {
        Child::View(v) => v,
        Child::Text(first) => {
            let node = text(&first);
            let mut last = first;
            effect(move || {
                if let Child::Text(s) = f()
                    && s != last
                {
                    with_backend(|b| b.set_text(node, &s));
                    last = s;
                }
            });
            View::One(node)
        }
    }
}

pub fn attr(node: NodeId, name: &str, value: &str) {
    let value = crate::assets::for_attr(value);
    with_backend(|b| b.set_attr(node, name, &value));
}

/// An attribute whose name and value are literals of the source (what generated code uses)
pub fn attr_lit(node: NodeId, name: &'static str, value: &'static str) {
    #[cfg(target_arch = "wasm32")]
    let original = value;
    #[cfg(target_arch = "wasm32")]
    let value = crate::web::dev_attr_literal(name, value);
    #[cfg(all(feature = "server", not(target_arch = "wasm32")))]
    let value = crate::dev::server_literal(&format!("attr:{name}"), value);
    if value.starts_with("/assets/") {
        attr(node, name, value);
    } else {
        with_backend(|b| b.set_attr_lit(node, name, value));
    }
    #[cfg(target_arch = "wasm32")]
    if crate::web::has_dev_literals() {
        with_backend(|b| b.track_literal(&format!("attr:{name}"), original, value, node));
    }
}

pub fn bind_attr<V: AttrValue>(
    node: NodeId,
    name: &'static str,
    mut f: impl FnMut() -> V + 'static,
) {
    bind_attr_dyn(node, name, Box::new(move || f().attr_value()));
}

#[inline(never)]
fn bind_attr_dyn(node: NodeId, name: &'static str, mut f: Box<dyn FnMut() -> Option<String>>) {
    let mut last: Option<Option<String>> = None;
    effect(move || {
        let v = f();
        if last.as_ref() != Some(&v) {
            with_backend(|b| match &v {
                Some(s) => b.set_attr(node, name, &crate::assets::for_attr(s)),
                None => b.remove_attr(node, name),
            });
            last = Some(v);
        }
    });
}

/// Sets a DOM property such as an input's `value`
pub fn bind_prop<V: AttrValue>(
    node: NodeId,
    name: &'static str,
    mut f: impl FnMut() -> V + 'static,
) {
    bind_prop_dyn(node, name, Box::new(move || f().attr_value().unwrap_or_default()));
}

#[inline(never)]
fn bind_prop_dyn(node: NodeId, name: &'static str, mut f: Box<dyn FnMut() -> String>) {
    effect(move || {
        let v = f();
        with_backend(|b| b.set_prop(node, name, &v));
    });
}

/// `ref={...}`: points the handle at the element until its owner is disposed
pub fn bind_ref(node: NodeId, target: super::NodeRef) {
    target.set(node);
    on_cleanup(move || target.clear());
}

pub fn bind_class(node: NodeId, class: &'static str, f: impl FnMut() -> bool + 'static) {
    bind_class_dyn(node, class, Box::new(f));
}

#[inline(never)]
fn bind_class_dyn(node: NodeId, class: &'static str, mut f: Box<dyn FnMut() -> bool>) {
    let mut last = None;
    effect(move || {
        let on = f();
        if last != Some(on) {
            with_backend(|b| b.set_class(node, class, on));
            last = Some(on);
        }
    });
}

/// Handlers can be `|| ...` or `|event| ...`
pub trait IntoHandler<Marker> {
    fn into_handler(self) -> Box<dyn FnMut(Event)>;
}
pub struct NoEvent;
pub struct WithEvent;

impl<F: FnMut() + 'static> IntoHandler<NoEvent> for F {
    fn into_handler(mut self) -> Box<dyn FnMut(Event)> {
        Box::new(move |_| self())
    }
}
impl<F: FnMut(Event) + 'static> IntoHandler<WithEvent> for F {
    fn into_handler(self) -> Box<dyn FnMut(Event)> {
        Box::new(self)
    }
}

/// Adds an event listener that's removed when the owner (component, branch, row) is disposed
pub fn listen<M>(node: NodeId, event: &str, handler: impl IntoHandler<M>) {
    listen_dyn(node, event, handler.into_handler());
}

#[inline(never)]
fn listen_dyn(node: NodeId, event: &str, mut h: Box<dyn FnMut(Event)>) {
    let owner = ambient::capture_owner();
    let wrapped: Handler = Rc::new(RefCell::new(move |ev: Event| {
        ambient::run_in_owner(owner, || batch(|| h(ev)));
    }));
    let id = with_backend(|b| b.listen(node, event, wrapped));
    on_cleanup(move || with_backend(|b| b.unlisten(id)));
}

/// `if`/`else`
pub fn show(
    cond: impl FnMut() -> bool + 'static,
    then: impl FnMut() -> View + 'static,
    otherwise: impl FnMut() -> View + 'static,
) -> View {
    show_dyn(Box::new(cond), Box::new(then), Box::new(otherwise))
}

#[inline(never)]
fn show_dyn(
    cond: Box<dyn FnMut() -> bool>,
    mut then: Box<dyn FnMut() -> View>,
    mut otherwise: Box<dyn FnMut() -> View>,
) -> View {
    switch_dyn(cond, Box::new(move |on| if on { then() } else { otherwise() }))
}

/// A region that's rebuilt when `key` changes: `build(key)` runs once per distinct value, the
/// previous build's effects, listeners and nodes are disposed first
pub fn switch<K: PartialEq + Clone + 'static>(
    key: impl FnMut() -> K + 'static,
    build: impl FnMut(K) -> View + 'static,
) -> View {
    switch_dyn(Box::new(key), Box::new(build))
}

fn switch_dyn<K: PartialEq + Clone + 'static>(
    key: Box<dyn FnMut() -> K>,
    mut build: Box<dyn FnMut(K) -> View>,
) -> View {
    let start = comment();
    let end = comment();
    let initial: Rc<RefCell<Vec<NodeId>>> = Rc::default();
    let first_nodes = initial.clone();
    let k = memo(key);
    let mut first = true;
    effect(move || {
        let key = k.get();
        untracked(|| {
            let nodes = in_region(|| build(key)).into_nodes();
            if first {
                // Not attached yet: the nodes are returned as part of the view.
                first = false;
                *first_nodes.borrow_mut() = nodes;
            } else {
                with_backend(|b| {
                    b.remove_between(start, end);
                    for n in nodes {
                        b.insert_before(end, n);
                    }
                });
            }
        });
    });
    let mut nodes = vec![start];
    nodes.append(&mut initial.borrow_mut());
    nodes.push(end);
    View::Many(nodes)
}

enum RowNodes {
    One(NodeId),
    /// First and last sibling, anchors included
    Range(NodeId, NodeId),
}

impl RowNodes {
    fn first(&self) -> NodeId {
        match *self {
            RowNodes::One(n) | RowNodes::Range(n, _) => n,
        }
    }

    fn remove(&self, b: &mut dyn Backend) {
        match *self {
            RowNodes::One(n) => b.remove(n),
            RowNodes::Range(s, e) => b.remove_range(s, e),
        }
    }
}

type KeyId = u128;

fn key_id<K: Hash>(key: &K) -> KeyId {
    use std::hash::Hasher;
    let mut a = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut a);
    let mut b = std::collections::hash_map::DefaultHasher::new();
    0x9e37_79b9_7f4a_7c15u64.hash(&mut b);
    key.hash(&mut b);
    (u128::from(a.finish()) << 64) | u128::from(b.finish())
}

struct Row {
    key: KeyId,
    scope: Scope,
    nodes: RowNodes,
}

struct List {
    rows: Vec<Row>,
}

/// What the list core needs from the typed side; the rest of the list machinery is not generic
trait Rows {
    /// Reads the items, subscribing to what they read
    fn fetch(&mut self);
    fn keys(&self) -> Vec<KeyId>;
    fn build(&mut self, i: usize) -> View;
}

struct Typed<T, K, Kf, I, R> {
    items: I,
    key: Kf,
    row: R,
    slots: Vec<Option<T>>,
    marker: std::marker::PhantomData<fn() -> K>,
}

impl<T, K, Kf, I, R> Rows for Typed<T, K, Kf, I, R>
where
    T: 'static,
    K: Hash + 'static,
    Kf: Fn(&T) -> K,
    I: FnMut() -> Vec<T>,
    R: FnMut(T) -> View,
{
    fn fetch(&mut self) {
        self.slots = (self.items)().into_iter().map(Some).collect();
    }

    fn keys(&self) -> Vec<KeyId> {
        self.slots.iter().flatten().map(|t| key_id(&(self.key)(t))).collect()
    }

    fn build(&mut self, i: usize) -> View {
        (self.row)(self.slots[i].take().expect("row built once"))
    }
}

/// Keyed list
pub fn each<T, K>(
    items: impl FnMut() -> Vec<T> + 'static,
    key: impl Fn(&T) -> K + 'static,
    row: impl FnMut(T) -> View + 'static,
) -> View
where
    T: 'static,
    K: Hash + 'static,
{
    each_core(Box::new(Typed {
        items,
        key,
        row,
        slots: Vec::new(),
        marker: std::marker::PhantomData,
    }))
}

#[inline(never)]
fn each_core(mut rows: Box<dyn Rows>) -> View {
    let start = comment();
    let end = comment();
    // Rows live in a scope owned by the surrounding build: they must
    // survive the effect re-running.
    let list_scope = with(|rt| rt.scope(|_| {}));
    let mut list = List { rows: Vec::new() };
    let initial: Rc<RefCell<Vec<NodeId>>> = Rc::default();
    let first_nodes = initial.clone();
    let mut first = true;
    effect(move || {
        rows.fetch();
        untracked(|| {
            let keys = rows.keys();
            let placed = reconcile(
                &mut list,
                &keys,
                &mut |i| build_row(keys[i], &mut || rows.build(i), list_scope),
                end,
                first,
            );
            if first {
                first = false;
                *first_nodes.borrow_mut() = placed;
            }
        });
    });
    let mut nodes = vec![start];
    nodes.append(&mut initial.borrow_mut());
    nodes.push(end);
    View::Many(nodes)
}

fn build_row(key: KeyId, row: &mut dyn FnMut() -> View, owner: Scope) -> (Row, Vec<NodeId>) {
    let mut built = None;
    let scope = with(|rt: &mut Runtime| {
        rt.scope_in(owner, |rt| {
            let _g = enter(rt);
            built = Some(in_region(|| match row() {
                View::One(n) => (RowNodes::One(n), vec![n]),
                View::Many(mut v) => {
                    let (s, e) = (comment(), comment());
                    v.insert(0, s);
                    v.push(e);
                    (RowNodes::Range(s, e), v)
                }
            }));
        })
    });
    let (nodes, ids) = built.expect("row built");
    (Row { key, scope, nodes }, ids)
}

fn reconcile(
    list: &mut List,
    keys: &[KeyId],
    build: &mut dyn FnMut(usize) -> (Row, Vec<NodeId>),
    end: NodeId,
    first: bool,
) -> Vec<NodeId> {
    let mut wanted: HashSet<KeyId> = HashSet::with_capacity(keys.len());
    for k in keys {
        assert!(wanted.insert(*k), "yawn: duplicate key in a `for` loop; keys must be unique");
    }
    let old_rows = std::mem::take(&mut list.rows);
    let mut by_key: HashMap<KeyId, usize> = HashMap::with_capacity(old_rows.len());
    let mut old: Vec<Option<Row>> = Vec::with_capacity(old_rows.len());
    for r in old_rows {
        if wanted.contains(&r.key) {
            by_key.insert(r.key, old.len());
            old.push(Some(r));
        } else {
            // DOM first: disposing releases the node ids.
            with_backend(|b| r.nodes.remove(b));
            with(|rt| rt.dispose(r.scope));
        }
    }

    let mut rows: Vec<Row> = Vec::with_capacity(keys.len());
    let mut old_pos: Vec<Option<usize>> = Vec::with_capacity(keys.len());
    let mut fresh: Vec<Vec<NodeId>> = Vec::with_capacity(keys.len());
    for (n, k) in keys.iter().enumerate() {
        match by_key.get(k).and_then(|&i| old[i].take().map(|r| (i, r))) {
            Some((i, r)) => {
                rows.push(r);
                old_pos.push(Some(i));
                fresh.push(Vec::new());
            }
            None => {
                let (r, ids) = build(n);
                rows.push(r);
                old_pos.push(None);
                fresh.push(ids);
            }
        }
    }

    let placed = if first {
        fresh.into_iter().flatten().collect()
    } else {
        let stay = longest_increasing(&old_pos);
        with_backend(|b| {
            for j in (0..rows.len()).rev() {
                if stay[j] {
                    continue;
                }
                let reference = rows.get(j + 1).map(|r| r.nodes.first()).unwrap_or(end);
                match (&rows[j].nodes, old_pos[j]) {
                    (RowNodes::One(n), _) => b.insert_before(reference, *n),
                    (RowNodes::Range(s, e), Some(_)) => b.move_range(*s, *e, reference),
                    (RowNodes::Range(..), None) => {
                        for n in &fresh[j] {
                            b.insert_before(reference, *n);
                        }
                    }
                }
            }
        });
        Vec::new()
    };
    list.rows = rows;
    placed
}

fn longest_increasing(pos: &[Option<usize>]) -> Vec<bool> {
    let mut tails: Vec<usize> = Vec::new(); // indices into `pos`
    let mut prev: Vec<Option<usize>> = vec![None; pos.len()];
    for (i, p) in pos.iter().enumerate() {
        let Some(p) = p else { continue };
        let at = tails.partition_point(|&t| pos[t].unwrap() < *p);
        if at > 0 {
            prev[i] = Some(tails[at - 1]);
        }
        if at == tails.len() {
            tails.push(i);
        } else {
            tails[at] = i;
        }
    }
    let mut stay = vec![false; pos.len()];
    let mut cur = tails.last().copied();
    while let Some(i) = cur {
        stay[i] = true;
        cur = prev[i];
    }
    stay
}

pub(crate) fn release_document() {
    ROOT_SCOPE.with(|scope| *scope.borrow_mut() = None);
    BACKEND.with(|b| *b.borrow_mut() = None);
    REGIONS.with(|r| r.borrow_mut().clear());
    ambient::clear_root();
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn swap_backend(backend: Box<dyn Backend>) {
    BACKEND.with(|b| *b.borrow_mut() = Some(backend));
}

/// Mounts `root` into `host` of `backend` and starts the application runtime
pub fn mount<C: super::Component>(backend: Box<dyn Backend>, host: NodeId, root: C) {
    BACKEND.with(|b| *b.borrow_mut() = Some(backend));
    REGIONS.with(|r| r.borrow_mut().clear());
    ambient::install_root(Runtime::new());
    let scope = ambient::run(|| {
        with(|rt| {
            rt.scope(|rt| {
                let _g = enter(rt);
                let view = in_region(|| root.render());
                with_backend(|b| {
                    for n in view.into_nodes() {
                        b.append(host, n);
                    }
                });
            })
        })
    });
    ROOT_SCOPE.with(|root| *root.borrow_mut() = Some(scope));
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn dispose_document() {
    if let Some(scope) = ROOT_SCOPE.with(|root| root.borrow_mut().take()) {
        ambient::run(|| {
            with(|rt| {
                rt.dispose(scope);
            })
        });
    }
    release_document();
}

fn dev_literal(kind: &str, value: &'static str) -> &'static str {
    #[cfg(target_arch = "wasm32")]
    {
        crate::web::dev_literal(kind, value)
    }
    #[cfg(all(feature = "server", not(target_arch = "wasm32")))]
    {
        crate::dev::server_literal(kind, value)
    }
    #[cfg(all(not(feature = "server"), not(target_arch = "wasm32")))]
    {
        let _ = kind;
        value
    }
}

#[cfg(test)]
mod each_bench {
    use super::*;
    use crate::ui::mock;
    use crate::ui::{Component, View};

    struct Rows(super::super::signals::Sig<Vec<u32>>);

    impl Component for Rows {
        fn render(self) -> View {
            let list = self.0;
            let root = element("div");
            add(
                root,
                each(
                    move || list.get(),
                    |n| *n,
                    |n| {
                        let li = element("li");
                        add(li, text(&format!("row {n}")));
                        li.into()
                    },
                ),
            );
            root.into()
        }
    }

    #[test]
    #[ignore]
    fn timing() {
        use std::cell::Cell;
        thread_local!(static LIST: Cell<Option<super::super::signals::Sig<Vec<u32>>>> = const { Cell::new(None) });
        struct Holder;
        impl Component for Holder {
            fn render(self) -> View {
                let list = super::super::signals::signal((0..5000u32).collect::<Vec<_>>());
                LIST.with(|l| l.set(Some(list)));
                Rows(list).render()
            }
        }
        let t = std::time::Instant::now();
        let app = mock::mount(Holder);
        let create = t.elapsed();
        let list = LIST.with(|l| l.get()).unwrap();
        let t = std::time::Instant::now();
        for round in 0..20u32 {
            ambient::run(|| {
                list.set((0..5000u32).rev().collect());
                list.set(
                    (0..5000u32)
                        .map(|n| if n % 2 == 0 { n } else { n + 10_000 * (round + 1) })
                        .collect(),
                );
                list.set((0..5000u32).collect());
            });
        }
        let update = t.elapsed();
        println!(
            "create 5000 rows: {create:?}; 60 reorders/replaces of 5000: {update:?}; html {} bytes",
            app.html().len()
        );
    }
}
