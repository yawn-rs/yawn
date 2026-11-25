use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};

pub const ELEMENT_NODE: u16 = 1;
pub const TEXT_NODE: u16 = 3;
pub const COMMENT_NODE: u16 = 8;

#[wasm_bindgen]
extern "C" {
    /// Any DOM node: element, text, comment, document fragment
    #[wasm_bindgen(js_name = Node)]
    #[derive(Clone, PartialEq)]
    pub type Node;

    #[wasm_bindgen(method, js_name = getBoundingClientRect)]
    pub fn get_bounding_client_rect(this: &Node) -> JsValue;
    #[wasm_bindgen(method, js_name = focus)]
    pub fn focus(this: &Node);
    #[wasm_bindgen(method, js_name = blur)]
    pub fn blur(this: &Node);
    #[wasm_bindgen(method, js_name = scrollIntoView)]
    pub fn scroll_into_view(this: &Node, options: &JsValue);

    #[wasm_bindgen(method, js_name = appendChild, catch)]
    pub fn append_child(this: &Node, child: &Node) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = insertBefore, catch)]
    pub fn insert_before(this: &Node, node: &Node, reference: Option<&Node>)
    -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = removeChild, catch)]
    pub fn remove_child(this: &Node, child: &Node) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = cloneNode, catch)]
    pub fn clone_node(this: &Node, deep: bool) -> Result<Node, JsValue>;
    /// `querySelector` on a node: its first descendant that matches
    #[wasm_bindgen(method, js_name = querySelector, catch)]
    pub fn query_selector_in(this: &Node, selector: &str) -> Result<Option<Node>, JsValue>;
    #[wasm_bindgen(method, js_name = hasChildNodes)]
    pub fn has_child_nodes(this: &Node) -> bool;
    #[wasm_bindgen(method)]
    pub fn contains(this: &Node, other: Option<&Node>) -> bool;
    /// `closest`: the node itself or its nearest ancestor that matches the selector (the node must
    /// be an element)
    #[wasm_bindgen(method, js_name = closest, catch)]
    pub fn closest(this: &Node, selector: &str) -> Result<Option<Node>, JsValue>;
    /// Whether the node is still in the document
    #[wasm_bindgen(method, getter, js_name = isConnected)]
    pub fn is_connected(this: &Node) -> bool;
    #[wasm_bindgen(method, js_name = splitText, catch)]
    pub fn split_text(this: &Node, offset: u32) -> Result<Node, JsValue>;

    #[wasm_bindgen(method, getter, js_name = parentNode)]
    pub fn parent_node(this: &Node) -> Option<Node>;
    #[wasm_bindgen(method, getter, js_name = firstChild)]
    pub fn first_child(this: &Node) -> Option<Node>;
    #[wasm_bindgen(method, getter, js_name = nextSibling)]
    pub fn next_sibling(this: &Node) -> Option<Node>;
    #[wasm_bindgen(method, getter, js_name = childNodes)]
    pub fn child_nodes(this: &Node) -> NodeList;
    #[wasm_bindgen(method, getter, js_name = nodeType)]
    pub fn node_type(this: &Node) -> u16;
    #[wasm_bindgen(method, getter, js_name = nodeValue)]
    pub fn node_value(this: &Node) -> Option<String>;
    #[wasm_bindgen(method, setter, js_name = nodeValue)]
    pub fn set_node_value(this: &Node, value: Option<&str>);
    #[wasm_bindgen(method, getter, js_name = tagName)]
    pub fn tag_name(this: &Node) -> String;
    #[wasm_bindgen(method, setter, js_name = textContent)]
    pub fn set_text_content(this: &Node, value: Option<&str>);
    /// `<template>`: its markup, and the parsed copy of it
    #[wasm_bindgen(method, setter, js_name = innerHTML)]
    pub fn set_inner_html(this: &Node, html: &str);
    #[wasm_bindgen(method, getter)]
    pub fn content(this: &Node) -> Node;
    /// `value` of a form control; any type, so the caller checks it
    #[wasm_bindgen(method, getter, js_name = value)]
    pub fn value(this: &Node) -> JsValue;

    #[wasm_bindgen(method, js_name = setAttribute, catch)]
    pub fn set_attribute(this: &Node, name: &str, value: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = removeAttribute, catch)]
    pub fn remove_attribute(this: &Node, name: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = getAttribute)]
    pub fn get_attribute(this: &Node, name: &str) -> Option<String>;
    #[wasm_bindgen(method, getter, js_name = classList)]
    pub fn class_list(this: &Node) -> DomTokenList;

    pub type NodeList;
    #[wasm_bindgen(method)]
    pub fn item(this: &NodeList, index: u32) -> Option<Node>;

    pub type DomTokenList;
    #[wasm_bindgen(method, catch)]
    pub fn add(this: &DomTokenList, token: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn remove(this: &DomTokenList, token: &str) -> Result<(), JsValue>;

    /// A span of the page's content; its rectangle hugs the text
    pub type Range;
    #[wasm_bindgen(method, js_name = selectNodeContents)]
    pub fn select_node_contents(this: &Range, node: &Node);
    #[wasm_bindgen(method, js_name = getBoundingClientRect)]
    pub fn range_rect(this: &Range) -> JsValue;

    #[wasm_bindgen(js_name = Document)]
    #[derive(Clone)]
    pub type Document;

    #[wasm_bindgen(method, js_name = createElement, catch)]
    pub fn create_element(this: &Document, tag: &str) -> Result<Node, JsValue>;
    #[wasm_bindgen(method, js_name = createTextNode)]
    pub fn create_text_node(this: &Document, text: &str) -> Node;
    #[wasm_bindgen(method, js_name = createRange)]
    pub fn create_range(this: &Document) -> Range;
    #[wasm_bindgen(method, js_name = createComment)]
    pub fn create_comment(this: &Document, text: &str) -> Node;
    #[wasm_bindgen(method, js_name = getElementById)]
    pub fn get_element_by_id(this: &Document, id: &str) -> Option<Node>;
    #[wasm_bindgen(method, js_name = querySelector, catch)]
    pub fn query_selector(this: &Document, selector: &str) -> Result<Option<Node>, JsValue>;
    #[wasm_bindgen(method, setter, js_name = title)]
    pub fn set_title(this: &Document, title: &str);

    /// The target of `addEventListener`: a node, the document or the window
    pub type EventTarget;
    #[wasm_bindgen(method, js_name = addEventListener, catch)]
    pub fn add_event_listener(
        this: &EventTarget,
        kind: &str,
        listener: &Closure<dyn FnMut(Event)>,
    ) -> Result<(), JsValue>;
    /// `addEventListener(kind, listener, true)`: runs in the capture phase, before the handlers of
    /// the page
    #[wasm_bindgen(method, js_name = addEventListener, catch)]
    pub fn add_event_listener_capture(
        this: &EventTarget,
        kind: &str,
        listener: &Closure<dyn FnMut(Event)>,
        capture: bool,
    ) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = removeEventListener, catch)]
    pub fn remove_event_listener_capture(
        this: &EventTarget,
        kind: &str,
        listener: &Closure<dyn FnMut(Event)>,
        capture: bool,
    ) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = removeEventListener, catch)]
    pub fn remove_event_listener(
        this: &EventTarget,
        kind: &str,
        listener: &Closure<dyn FnMut(Event)>,
    ) -> Result<(), JsValue>;

    pub type Event;
    #[wasm_bindgen(method, getter)]
    pub fn target(this: &Event) -> Option<Node>;
    #[wasm_bindgen(method, getter, js_name = key)]
    pub fn key(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = ctrlKey)]
    pub fn ctrl_key(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = metaKey)]
    pub fn meta_key(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = shiftKey)]
    pub fn shift_key(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = altKey)]
    pub fn alt_key(this: &Event) -> JsValue;
    #[wasm_bindgen(method, getter, js_name = button)]
    pub fn button(this: &Event) -> JsValue;
    #[wasm_bindgen(method, js_name = preventDefault)]
    pub fn prevent_default(this: &Event);
    #[wasm_bindgen(method, js_name = stopPropagation)]
    pub fn stop_propagation(this: &Event);

    #[derive(Clone)]
    pub type Window;
    #[wasm_bindgen(thread_local_v2, js_namespace = globalThis, js_name = window)]
    static WINDOW: Option<Window>;
    #[wasm_bindgen(method, getter)]
    pub fn document(this: &Window) -> Option<Document>;
    #[wasm_bindgen(method, getter)]
    pub fn location(this: &Window) -> Location;
    #[wasm_bindgen(method, getter, catch)]
    pub fn history(this: &Window) -> Result<History, JsValue>;

    pub type Location;
    #[wasm_bindgen(method, getter, catch)]
    pub fn pathname(this: &Location) -> Result<String, JsValue>;
    #[wasm_bindgen(method, getter, catch)]
    pub fn search(this: &Location) -> Result<String, JsValue>;
    #[wasm_bindgen(method, getter, catch)]
    pub fn hash(this: &Location) -> Result<String, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn assign(this: &Location, url: &str) -> Result<(), JsValue>;

    pub type Clipboard;
    #[wasm_bindgen(method, js_name = writeText)]
    pub fn write_text(this: &Clipboard, text: &str) -> JsValue;

    pub type History;
    #[wasm_bindgen(method, js_name = pushState, catch)]
    pub fn push_state(
        this: &History,
        state: &JsValue,
        title: &str,
        url: &str,
    ) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = replaceState, catch)]
    pub fn replace_state(
        this: &History,
        state: &JsValue,
        title: &str,
        url: &str,
    ) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn back(this: &History) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn forward(this: &History) -> Result<(), JsValue>;

    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn console_warn(message: &str);
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);

    #[wasm_bindgen(js_namespace = Reflect, js_name = get, catch)]
    pub(crate) fn reflect_get(target: &JsValue, key: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(js_namespace = Reflect, js_name = set, catch)]
    fn reflect_set(target: &JsValue, key: &str, value: &JsValue) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = queueMicrotask)]
    fn queue_microtask(callback: &Closure<dyn FnMut()>);
    #[wasm_bindgen(js_name = queueMicrotask)]
    fn queue_microtask_once(callback: &JsValue);

    #[wasm_bindgen(js_namespace = Date, js_name = now)]
    fn date_now() -> f64;

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(callback: &JsValue, milliseconds: i32) -> i32;
    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(handle: i32);

    // `fetch` and what it needs
    pub type Promise;
    #[wasm_bindgen(js_name = fetch)]
    fn fetch(url: &str, init: &JsValue) -> Promise;
    #[wasm_bindgen(method, js_name = then)]
    fn then2(
        this: &Promise,
        resolve: &Closure<dyn FnMut(JsValue)>,
        reject: &Closure<dyn FnMut(JsValue)>,
    ) -> Promise;

    #[wasm_bindgen(js_name = Object)]
    type Plain;
    #[wasm_bindgen(constructor, js_class = "Object")]
    fn new() -> Plain;

    #[wasm_bindgen(js_name = Headers)]
    type Headers;
    #[wasm_bindgen(constructor, js_class = "Headers", catch)]
    fn new() -> Result<Headers, JsValue>;
    #[wasm_bindgen(method, js_name = set, catch)]
    fn set(this: &Headers, name: &str, value: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, js_name = forEach)]
    fn for_each(this: &Headers, callback: &Closure<dyn FnMut(String, String)>);

    #[wasm_bindgen(js_name = AbortController)]
    #[derive(Clone)]
    type AbortController;
    #[wasm_bindgen(constructor, js_class = "AbortController", catch)]
    fn new() -> Result<AbortController, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn signal(this: &AbortController) -> AbortSignal;
    #[wasm_bindgen(method)]
    fn abort(this: &AbortController);

    type AbortSignal;
    #[wasm_bindgen(method, getter)]
    fn aborted(this: &AbortSignal) -> bool;

    type Response;
    #[wasm_bindgen(method, getter)]
    fn status(this: &Response) -> u16;
    #[wasm_bindgen(method, getter)]
    fn headers(this: &Response) -> Headers;
    #[wasm_bindgen(method, js_name = arrayBuffer, catch)]
    fn array_buffer(this: &Response) -> Result<Promise, JsValue>;

    // Bytes in and out of JavaScript: `Object(x)` returns `x` for an object, so it works as identity
    #[wasm_bindgen(js_name = Object)]
    fn view_of(bytes: &[u8]) -> Bytes;
    #[wasm_bindgen(js_name = Object)]
    fn copy_of(bytes: &Bytes) -> Vec<u8>;
    #[wasm_bindgen(js_name = Uint8Array)]
    type Bytes;
    #[wasm_bindgen(constructor, js_class = "Uint8Array")]
    fn new(buffer: &JsValue) -> Bytes;
    #[wasm_bindgen(method)]
    fn slice(this: &Bytes) -> Bytes;
}

pub fn window() -> Option<Window> {
    WINDOW.with(Clone::clone)
}

pub fn document() -> Option<Document> {
    window()?.document()
}

impl Window {
    /// The window as the target of `addEventListener`
    pub fn target(&self) -> &EventTarget {
        self.unchecked_ref()
    }
}

impl Document {
    pub fn target(&self) -> &EventTarget {
        self.unchecked_ref()
    }
}

impl Node {
    pub fn target(&self) -> &EventTarget {
        self.unchecked_ref()
    }
}

pub fn log(message: &str) {
    console_log(message);
}

pub fn warn(message: &str) {
    console_warn(message);
}

pub fn error(message: &str) {
    console_error(message);
}

/// `Reflect.get`, for a string property of any object; `None` if it's missing, not a string or the
/// read throws
pub fn get_string(target: &JsValue, key: &str) -> Option<String> {
    reflect_get(target, key).ok()?.as_string()
}

/// `Reflect.get`, for a number property of any object (`0.0` when it is missing)
pub fn get_number(target: &JsValue, key: &str) -> f64 {
    reflect_get(target, key).ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// A new empty JavaScript object, `{}`
pub fn new_plain_object() -> JsValue {
    Plain::new().into()
}

/// Height of the browser window's viewport in pixels
pub fn viewport_height() -> f64 {
    window().map_or(0.0, |w| get_number(&w, "innerHeight"))
}

/// Width of the browser window's viewport in pixels
pub fn viewport_width() -> f64 {
    window().map_or(0.0, |w| get_number(&w, "innerWidth"))
}

/// `Reflect.get`, for a property that holds an object
pub fn get_object(target: &JsValue, key: &str) -> Option<JsValue> {
    reflect_get(target, key).ok().filter(|v| v.is_object())
}

/// Assigns the string `value` to a property (`input.value`, `input.checked` ...)
pub fn set_string(target: &JsValue, key: &str, value: &str) {
    let _ = reflect_set(target, key, &JsValue::from_str(value));
}

/// Milliseconds since the epoch, from the browser's clock
pub fn now_ms() -> f64 {
    date_now()
}

/// Runs `callback` after the current task, once the call stack has unwound
pub fn after_current_call(callback: &Closure<dyn FnMut()>) {
    queue_microtask(callback);
}

/// Calls `callback` once after `ms` milliseconds
pub fn after_ms(callback: &Closure<dyn FnMut()>, ms: i32) -> i32 {
    set_timeout(callback.as_ref(), ms)
}

/// Cancels a timer started with [`after_ms`]
pub fn cancel_timeout(handle: i32) {
    clear_timeout(handle);
}

/// Shows `url` in the address bar without loading it and without a new history entry
pub fn replace_url(url: &str) {
    if let Some(h) = window().and_then(|w| w.history().ok()) {
        let _ = h.replace_state(&JsValue::NULL, "", url);
    }
}

/// Loads `url` as a new page
pub fn assign_url(url: &str) {
    if let Some(w) = window() {
        let _ = w.location().assign(url);
    }
}

/// `array[index]` of a JavaScript array-like
pub fn array_item(array: &JsValue, index: u32) -> Option<JsValue> {
    let v = reflect_get(array, &index.to_string()).ok()?;
    (!v.is_undefined() && !v.is_null()).then_some(v)
}

/// Waits for a JavaScript promise: what it resolved to, or what it was rejected with
pub async fn await_promise(promise: JsValue) -> Result<JsValue, JsValue> {
    PromiseFuture::new(&promise.unchecked_into::<Promise>()).await
}

type Task = Rc<RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>>;

fn task_waker(task: Task) -> Waker {
    use std::task::{RawWaker, RawWakerVTable};
    unsafe fn clone(p: *const ()) -> RawWaker {
        unsafe {
            Rc::increment_strong_count(
                p as *const RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
            )
        };
        RawWaker::new(p, &VTABLE)
    }
    unsafe fn wake(p: *const ()) {
        schedule(unsafe {
            Rc::from_raw(p as *const RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>)
        });
    }
    unsafe fn wake_by_ref(p: *const ()) {
        unsafe {
            Rc::increment_strong_count(
                p as *const RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
            );
            wake(p);
        }
    }
    unsafe fn drop_waker(p: *const ()) {
        drop(unsafe {
            Rc::from_raw(p as *const RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>)
        });
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
    // The browser runs one thread, so a waker that isn't `Send` is never moved anywhere.
    unsafe { Waker::from_raw(RawWaker::new(Rc::into_raw(task) as *const (), &VTABLE)) }
}

fn schedule(task: Task) {
    let run = Closure::once_into_js(move || {
        let waker = task_waker(task.clone());
        let mut cx = Context::from_waker(&waker);
        let mut slot = task.borrow_mut();
        if let Some(fut) = slot.as_mut()
            && fut.as_mut().poll(&mut cx).is_ready()
        {
            *slot = None;
        }
    });
    queue_microtask_once(&run);
}

/// Runs a future to completion on the browser's event loop, starting after the current call
pub fn spawn_local(future: impl Future<Output = ()> + 'static) {
    schedule(Rc::new(RefCell::new(Some(Box::pin(future)))));
}

/// What `fetch` needs to know about one request
pub struct FetchRequest<'a> {
    pub url: &'a str,
    pub method: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub timeout_ms: i32,
}

/// What came back: status, headers and the body
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why `fetch` failed
pub enum FetchError {
    Timeout,
    /// The browser refused a header name or value
    Header(String),
    /// Network failure, CORS, a body that couldn't be read
    Network(String),
}

fn message(e: &JsValue) -> String {
    e.as_string().or_else(|| get_string(e, "message")).unwrap_or_else(|| "fetch failed".into())
}

struct PromiseFuture {
    state: Rc<RefCell<(Option<Result<JsValue, JsValue>>, Option<Waker>)>>,
    // alive until the promise settled; a closure the browser still holds must not be dropped
    callbacks: Option<(Closure<dyn FnMut(JsValue)>, Closure<dyn FnMut(JsValue)>)>,
}

impl PromiseFuture {
    fn new(promise: &Promise) -> PromiseFuture {
        let state: Rc<RefCell<(Option<Result<JsValue, JsValue>>, Option<Waker>)>> = Rc::default();
        let settle = |state: Rc<RefCell<(Option<Result<JsValue, JsValue>>, Option<Waker>)>>,
                      ok: bool| {
            Closure::<dyn FnMut(JsValue)>::once(move |v: JsValue| {
                let waker = {
                    let mut s = state.borrow_mut();
                    s.0 = Some(if ok { Ok(v) } else { Err(v) });
                    s.1.take()
                };
                if let Some(w) = waker {
                    w.wake();
                }
            })
        };
        let (resolve, reject) = (settle(state.clone(), true), settle(state.clone(), false));
        promise.then2(&resolve, &reject);
        PromiseFuture { state, callbacks: Some((resolve, reject)) }
    }
}

impl Future for PromiseFuture {
    type Output = Result<JsValue, JsValue>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ready = self.state.borrow_mut().0.take();
        match ready {
            Some(r) => {
                self.callbacks = None;
                Poll::Ready(r)
            }
            None => {
                self.state.borrow_mut().1 = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

pub async fn fetch_bytes(req: FetchRequest<'_>) -> Result<FetchResponse, FetchError> {
    let init = Plain::new();
    let set = |key: &str, value: &JsValue| {
        let _ = reflect_set(&init, key, value);
    };
    set("method", &JsValue::from_str(req.method));
    set("redirect", &JsValue::from_str("manual"));
    let headers = Headers::new().map_err(|e| FetchError::Network(message(&e)))?;
    for (n, v) in req.headers {
        headers.set(n, v).map_err(|e| FetchError::Header(message(&e)))?;
    }
    set("headers", &headers);
    if !req.body.is_empty() {
        // a copy: the view onto WebAssembly memory dies when the memory grows
        set("body", &view_of(req.body).slice());
    }
    let controller = AbortController::new().map_err(|e| FetchError::Network(message(&e)))?;
    set("signal", &controller.signal());
    let timer = window().map(|_| {
        let c = controller.clone();
        let cb = Closure::once_into_js(move || c.abort());
        set_timeout(&cb, req.timeout_ms)
    });
    let result = async {
        let res = PromiseFuture::new(&fetch(req.url, &init)).await.map_err(|e| {
            if controller.signal().aborted() {
                FetchError::Timeout
            } else {
                FetchError::Network(message(&e))
            }
        })?;
        let res: Response = res.unchecked_into();
        let buffer =
            PromiseFuture::new(&res.array_buffer().map_err(|e| FetchError::Network(message(&e)))?)
                .await
                .map_err(|e| FetchError::Network(message(&e)))?;
        let collected = Rc::new(RefCell::new(Vec::new()));
        let sink = collected.clone();
        let each = Closure::<dyn FnMut(String, String)>::new(move |value, name| {
            sink.borrow_mut().push((name, value))
        });
        res.headers().for_each(&each);
        let headers = collected.take();
        Ok(FetchResponse { status: res.status(), headers, body: copy_of(&Bytes::new(&buffer)) })
    }
    .await;
    if let Some(t) = timer {
        clear_timeout(t);
    }
    result
}

#[wasm_bindgen(
    inline_js = "export function viewTransition(update) { if (document.startViewTransition) document.startViewTransition(update); else update(); }"
)]
extern "C" {
    #[wasm_bindgen(js_name = viewTransition)]
    fn view_transition(update: &JsValue);
}

/// Runs `update` (which changes the page) inside the browser's view transition, so the old and the
/// new page can be animated with CSS (`::view-transition-old(root)`)
pub fn with_view_transition(update: impl FnOnce() + 'static) {
    view_transition(&Closure::once_into_js(update));
}

#[wasm_bindgen(
    inline_js = "export function scrollNearest(el) { if (el.scrollIntoView) el.scrollIntoView({ block: 'nearest', inline: 'nearest' }); }"
)]
extern "C" {
    #[wasm_bindgen(js_name = scrollNearest)]
    pub fn scroll_nearest(node: &Node);
}

#[wasm_bindgen(
    inline_js = "export function devTrackLiteral(kind, original, current, node) { if (window.__yawnTrackLiteral) window.__yawnTrackLiteral(kind, original, current, node); }"
)]
extern "C" {
    #[wasm_bindgen(js_name = devTrackLiteral)]
    pub fn dev_track_literal(kind: &str, original: &str, current: &str, node: &Node);
}

#[wasm_bindgen(
    inline_js = "export function reportPanic(text) { if (window.__yawnPanic) window.__yawnPanic(text); }"
)]
extern "C" {
    #[wasm_bindgen(js_name = reportPanic)]
    fn report_panic(text: &str);
}

/// Shows a panic on the page itself (under `yawn dev`, which provides the overlay); elsewhere
/// nothing happens
pub fn show_panic(text: &str) {
    report_panic(text);
}

pub fn copy_to_clipboard(text: &str) {
    let Some(w) = window() else { return };
    let Some(nav) = get_object(&w, "navigator") else {
        return;
    };
    let Some(cb) = get_object(&nav, "clipboard") else {
        return;
    };
    let _ = cb.unchecked_into::<Clipboard>().write_text(text);
}
