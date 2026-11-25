#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use super::{Component, mock};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

type Pending = Pin<Box<dyn Future<Output = Option<String>>>>;

#[derive(Default)]
struct Prefetch {
    done: HashMap<String, Option<String>>,
    open: Vec<(String, Pending)>,
}

/// What a component can know about the request it's rendered for
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// Path and query, e.g. `/guilds?x=1`
    pub url: String,
    /// The locale chosen for the request (`en`, `pl`, ...)
    pub locale: String,
    /// The `Cookie` header, empty if none
    pub cookies: String,
}

thread_local! {
    static CURRENT: RefCell<Option<Request>> = const { RefCell::new(None) };
    static PREFETCH: RefCell<Option<Prefetch>> = const { RefCell::new(None) };
}

pub(crate) fn fetched(name: &str, ask: impl FnOnce() -> Pending) -> Option<Option<String>> {
    let mut ask = Some(ask);
    let queued = PREFETCH.with(|p| {
        let mut p = p.borrow_mut();
        let p = p.as_mut()?;
        if let Some(found) = p.done.get(name) {
            return Some(Some(found.clone()));
        }
        if !p.open.iter().any(|(n, _)| n == name)
            && let Some(ask) = ask.take()
        {
            p.open.push((name.to_string(), ask()));
        }
        Some(None)
    });
    match queued {
        Some(answer) => answer,
        None => ask.map(|ask| poll_once(ask())),
    }
}

fn poll_once(mut future: Pending) -> Option<String> {
    let mut cx = Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(text) => text,
        Poll::Pending => None,
    }
}

fn answer(open: Vec<(String, Pending)>) -> Vec<(String, Option<String>)> {
    let names: Vec<String> = open.iter().map(|(n, _)| n.clone()).collect();
    let futures: Vec<Pending> = open.into_iter().map(|(_, f)| f).collect();
    let count = futures.len();
    let all = async move {
        let mut slots: Vec<Option<Pending>> = futures.into_iter().map(Some).collect();
        let mut out: Vec<Option<String>> = vec![None; slots.len()];
        std::future::poll_fn(|cx| {
            let mut waiting = false;
            for (i, slot) in slots.iter_mut().enumerate() {
                if let Some(f) = slot {
                    match f.as_mut().poll(cx) {
                        Poll::Ready(text) => {
                            out[i] = text;
                            *slot = None;
                        }
                        Poll::Pending => waiting = true,
                    }
                }
            }
            if waiting { Poll::Pending } else { Poll::Ready(()) }
        })
        .await;
        out
    };
    #[cfg(all(not(target_arch = "wasm32"), any(feature = "server", feature = "http")))]
    let out = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), all)
                .await
                .unwrap_or_else(|_| vec![None; count])
        }),
        Err(_) => poll_once_all(all, count),
    };
    #[cfg(not(all(not(target_arch = "wasm32"), any(feature = "server", feature = "http"))))]
    let out = poll_once_all(all, count);
    names.into_iter().zip(out).collect()
}

fn poll_once_all(
    all: impl Future<Output = Vec<Option<String>>>,
    count: usize,
) -> Vec<Option<String>> {
    let mut all = Box::pin(all);
    let mut cx = Context::from_waker(std::task::Waker::noop());
    match all.as_mut().poll(&mut cx) {
        Poll::Ready(out) => out,
        Poll::Pending => vec![None; count],
    }
}

/// The request being rendered, or `None` in the browser and in tests that didn't set one
pub fn request() -> Option<Request> {
    CURRENT.with(|c| c.borrow().clone())
}

/// A rendered page: the body markup and what the components put in the head
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// Markup for `<div id="app">`
    pub html: String,
    /// Set by `set_title`
    pub title: Option<String>,
    /// `<meta>` and `<link>` tags, one per line
    pub head: String,
}

struct Scope(Option<Request>, bool);

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        if self.1 {
            PREFETCH.with(|p| *p.borrow_mut() = None);
        }
    }
}

fn build<C: Component>(root: C) -> Page {
    let app = mock::mount(root);
    let title = Some(app.title()).filter(|t| !t.is_empty());
    let page = Page { html: app.html(), title, head: app.head_html() };
    drop(app);
    super::rt::release_document();
    page
}

pub fn render<C: Component>(request: Request, root: C) -> Page {
    let _scope = Scope(CURRENT.with(|c| c.borrow_mut().replace(request)), false);
    build(root)
}

/// Renders the component `make` builds for `request`
pub fn render_with<C: Component>(request: Request, make: impl Fn() -> C) -> Page {
    let _scope = Scope(CURRENT.with(|c| c.borrow_mut().replace(request)), true);
    PREFETCH.with(|p| *p.borrow_mut() = Some(Prefetch::default()));
    let mut page = build(make());
    for _ in 0..6 {
        let open = PREFETCH.with(|p| {
            p.borrow_mut().as_mut().map(|p| std::mem::take(&mut p.open)).unwrap_or_default()
        });
        if open.is_empty() {
            break;
        }
        let answers = answer(open);
        PREFETCH.with(|p| {
            if let Some(p) = p.borrow_mut().as_mut() {
                p.done.extend(answers);
            }
        });
        page = build(make());
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{View, head, rt, set_title};

    struct Hello;
    impl Component for Hello {
        fn render(self) -> View {
            let p = rt::element("p");
            let who = request().map(|r| r.url).unwrap_or_default();
            rt::add(p, rt::text(&format!("hi {who} & <you>")));
            set_title("Hello");
            head::meta("description", "a \"page\"");
            head::link("canonical", "/x");
            head::stylesheet("/a.css");
            head::stylesheet("/a.css");
            head::boot("ftl", "/t/pl.ftl");
            p.into()
        }
    }

    #[test]
    fn renders_html_title_and_head_for_the_request() {
        let page = render(
            Request { url: "/guilds".into(), locale: "en".into(), cookies: String::new() },
            Hello,
        );
        assert_eq!(page.html, "<p>hi /guilds &amp; &lt;you&gt;</p>");
        assert_eq!(page.title.as_deref(), Some("Hello"));
        assert!(
            page.head.contains("<meta name=\"description\" content=\"a &quot;page&quot;\">"),
            "{}",
            page.head
        );
        assert!(page.head.contains("<link rel=\"canonical\" href=\"/x\">"));
        assert_eq!(page.head.matches("href=\"/a.css\"").count(), 1, "{}", page.head);
        assert!(
            page.head.contains("data-yawn-boot=\"ftl\"")
                && page.head.contains("href=\"/t/pl.ftl\""),
            "{}",
            page.head
        );
        assert_eq!(request(), None, "the request is only visible during the render");
    }

    struct Raw;
    impl Component for Raw {
        fn render(self) -> View {
            let d = rt::element("div");
            rt::bind_prop(d, "innerHTML", || "<b>bold</b> text");
            rt::add(d, rt::text("not shown"));
            d.into()
        }
    }

    #[test]
    fn a_raw_html_property_is_written_into_the_page() {
        let page = render(Request::default(), Raw);
        assert_eq!(page.html, "<div><b>bold</b> text</div>");
    }

    #[test]
    fn renders_can_follow_each_other_on_one_thread() {
        let a = render(Request { url: "/a".into(), ..Default::default() }, Hello);
        let b = render(Request { url: "/b".into(), ..Default::default() }, Hello);
        assert!(a.html.contains("/a") && b.html.contains("/b"));
    }

    #[cfg(feature = "server")]
    mod resources {
        use super::*;
        use crate::data::resource;
        use crate::ui::signal;
        use std::time::{Duration, Instant};

        async fn slow(text: &'static str) -> Option<String> {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Some(text.to_string())
        }

        struct Asks;
        impl Component for Asks {
            fn render(self) -> View {
                let first = resource("a", "/a", || slow("one"));
                let second = resource("b", "/b", || slow("two"));
                let third = resource("c", "/c", || slow("three"));
                // asked only once the first has come in
                let later =
                    resource(if first.get().is_some() { "d" } else { "none" }, "/d", || {
                        slow("four")
                    });
                let p = rt::element("p");
                let all = format!(
                    "{:?}{:?}{:?}{:?}",
                    first.get(),
                    second.get(),
                    third.get(),
                    later.get()
                );
                rt::add(p, rt::text(&all));
                let _ = signal(0);
                p.into()
            }
        }

        #[test]
        fn what_a_page_asks_for_is_answered_at_the_same_time() {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let started = Instant::now();
            let page = runtime.block_on(async {
                tokio::task::spawn_blocking(|| render_with(Request::default(), || Asks))
                    .await
                    .unwrap()
            });
            let took = started.elapsed();
            assert!(
                page.html.contains("Some(\"one\")")
                    && page.html.contains("Some(\"two\")")
                    && page.html.contains("Some(\"three\")"),
                "{}",
                page.html
            );
            assert!(
                page.html.contains("Some(\"four\")"),
                "a question asked in the second round is answered too: {}",
                page.html
            );
            // four answers of 150 ms in two rounds: about 300 ms, where one after the other it would be 600
            assert!(took < Duration::from_millis(480), "took {took:?}");
        }

        struct Prefetches;
        impl Component for Prefetches {
            fn render(self) -> View {
                crate::data::prefetch("late", || slow("later"));
                let first = resource("a", "/a", || slow("one"));
                let late =
                    resource(if first.get().is_some() { "late" } else { "none" }, "/l", || {
                        slow("never asked")
                    });
                let p = rt::element("p");
                rt::add(p, rt::text(&format!("{:?}{:?}", first.get(), late.get())));
                p.into()
            }
        }

        #[test]
        fn a_question_prefetched_at_the_top_goes_out_with_the_first_round() {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            let started = Instant::now();
            let page = runtime.block_on(async {
                tokio::task::spawn_blocking(|| render_with(Request::default(), || Prefetches))
                    .await
                    .unwrap()
            });
            assert!(
                page.html.contains("Some(\"one\")") && page.html.contains("Some(\"later\")"),
                "{}",
                page.html
            );
            assert!(started.elapsed() < Duration::from_millis(250), "took {:?}", started.elapsed());
        }
    }
}
