use crate::assets::{self, Manifest};
use crate::config::{self, ConfigError, ProjectConfig};
use crate::ui::{Component, ssr};
use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, broadcast};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, IntervalStream};
use tower_http::CompressionLevel;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

pub use crate::protocol::{Control, escape_line, json_string, parse_control};
pub use axum;

#[derive(Debug)]
pub enum Error {
    Config(ConfigError),
    Bind { addr: SocketAddr, source: std::io::Error },
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => write!(f, "{e}"),
            Error::Bind { addr, source } if source.kind() == std::io::ErrorKind::AddrInUse => {
                write!(
                    f,
                    "cannot listen on {addr}: port already in use (choose another with --port or PORT)"
                )
            }
            Error::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            Error::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for Error {}
impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Error::Config(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Everything the server needs to know, resolved from the environment and project files
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub addr: SocketAddr,
    pub port_source: config::PortSource,
    pub root: PathBuf,
    /// Compiled client output; `/pkg/*` is served from `<dist>/pkg`
    pub dist: PathBuf,
    /// The dev session's own files (`YAWN_DEV_DIR`); `/__yawn/literals.json` is served from here
    pub dev_dir: PathBuf,
    pub assets: PathBuf,
    /// Output of `yawn build` (hashed files and `manifest.json`), when there is one; used outside
    /// development in place of `dist` and `assets`
    pub public: Option<PathBuf>,
    /// Development mode: reload channel, no-store caching, request logging, stdin control
    pub dev: bool,
    /// Revision the CLI had reached when it started this process
    pub rev: u64,
    pub project: ProjectConfig,
}

impl ServeConfig {
    /// Resolves from the process environment
    pub fn from_env() -> Result<Self, Error> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let cwd = std::env::current_dir()?;
        // a project; failing that the directory of the executable (a build in `dist/`, which holds a copy of Yawn.toml)
        let beside_exe = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(PathBuf::from))
            .filter(|d| d.join(config::CONFIG_FILE).is_file());
        let root = config::find_project_root(&cwd).or(beside_exe).unwrap_or(cwd);
        let project = ProjectConfig::load(&root)?.unwrap_or_default();
        let cli_port = match var("YAWN_PORT") {
            Some(v) => Some(
                v.parse::<u16>()
                    .map_err(|_| ConfigError::BadPort { source: "YAWN_PORT", value: v })?,
            ),
            None => None,
        };
        let resolved = config::resolve_port(cli_port, var("PORT").as_deref(), project.server.port)?;
        let host = project.server.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let dev = var("YAWN_DEV").is_some_and(|v| v != "0");
        let public = (!dev)
            .then(|| {
                let beside =
                    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("public")));
                var("YAWN_PUBLIC")
                    .map(PathBuf::from)
                    .into_iter()
                    .chain(beside)
                    .chain([root.join("dist/public")])
                    .find(|d| d.join(assets::MANIFEST_FILE).is_file())
            })
            .flatten();
        Ok(ServeConfig {
            addr: SocketAddr::new(host, resolved.port),
            port_source: resolved.source,
            dist: var("YAWN_DIST")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(config::DIST_DIR)),
            dev_dir: var("YAWN_DEV_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join(config::DEV_DIR)),
            assets: root.join(project.assets_dir()),
            public,
            dev,
            rev: var("YAWN_REV").and_then(|v| v.parse().ok()).unwrap_or(0),
            root,
            project,
        })
    }
}

/// An application: its name, document settings and any routes of its own
pub struct App {
    name: String,
    title: Option<String>,
    stylesheets: Option<Vec<String>>,
    router: Router,
    renderer: Option<Renderer>,
}

/// Renders the application for a request; see [`App::ssr`]
pub type Renderer = Arc<dyn Fn(ssr::Request) -> ssr::Page + Send + Sync>;

impl App {
    pub fn new(name: impl Into<String>) -> Self {
        App {
            name: name.into(),
            title: None,
            stylesheets: None,
            router: Router::new(),
            renderer: None,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Replaces the stylesheet list (otherwise `Yawn.toml`, otherwise `/assets/style.css`)
    pub fn stylesheets(mut self, urls: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.stylesheets = Some(urls.into_iter().map(Into::into).collect());
        self
    }

    /// Renders `C` on the server for every page request, so the HTML that arrives already holds the
    /// page (and its `<title>`, `<meta>` tags and translations)
    pub fn ssr<C: Component + Default + 'static>(mut self) -> Self {
        self.renderer = Some(Arc::new(|req| ssr::render_with(req, C::default)));
        self
    }

    /// Adds routes of your own
    pub fn merge(mut self, router: Router) -> Self {
        self.router = self.router.merge(router);
        self
    }

    /// Runs the server until it's stopped (Ctrl-C, or stdin closing under `yawn dev`)
    pub fn run(self) -> Result<(), Error> {
        let config = ServeConfig::from_env()?;
        let rt =
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let result = rt.block_on(self.serve(config));
        // A blocked stdin read must not delay exit.
        rt.shutdown_background();
        result
    }

    /// Like [`App::run`] for a `main` that returns `ExitCode`: prints the error and exits with 1
    pub fn start(self) -> std::process::ExitCode {
        match self.run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::ExitCode::FAILURE
            }
        }
    }

    pub async fn serve(self, config: ServeConfig) -> Result<(), Error> {
        let listener = tokio::net::TcpListener::bind(config.addr)
            .await
            .map_err(|source| Error::Bind { addr: config.addr, source })?;
        self.serve_listener(listener, config).await
    }

    /// Like [`App::serve`] on an already bound listener (used by tests and socket activation)
    pub async fn serve_listener(
        self,
        listener: tokio::net::TcpListener,
        config: ServeConfig,
    ) -> Result<(), Error> {
        let local = listener.local_addr()?;
        #[cfg(feature = "http")]
        crate::http::install_crypto_provider();
        // A production build: hashed files and the map to them. Every URL the page links goes through it.
        let manifest = match (&config.public, config.dev) {
            (Some(public), false) => std::fs::read_to_string(public.join(assets::MANIFEST_FILE))
                .ok()
                .and_then(|t| Manifest::parse(&t))
                .map(Arc::new),
            _ => None,
        };
        if let Some(m) = &manifest {
            assets::install((**m).clone());
        }
        let resolve =
            |url: &str| manifest.as_ref().map_or_else(|| url.to_string(), |m| m.resolve(url));
        let doc = Document {
            title: self
                .title
                .or_else(|| config.project.app.title.clone())
                .unwrap_or_else(|| self.name.clone()),
            stylesheets: self
                .stylesheets
                .or_else(|| config.project.app.stylesheets.clone())
                .unwrap_or_else(|| vec!["/assets/style.css".to_string()])
                .iter()
                .map(|u| resolve(u))
                .collect(),
            scripts: config.project.app.scripts.iter().map(|u| resolve(u)).collect(),
            entry: resolve("/pkg/app.js"),
            assets: manifest.as_ref().map(|m| m.to_json()),
        };
        let shutdown = Arc::new(Notify::new());
        let hub = config.dev.then(|| Arc::new(DevHub::new(config.rev)));
        let (pkg_dir, assets_dir) = match (&manifest, &config.public) {
            (Some(_), Some(public)) => (public.join("pkg"), public.join("assets")),
            _ => (config.dist.join("pkg"), config.assets.clone()),
        };
        let shared = Shared {
            doc: Arc::new(doc),
            rsx_css: config.dist.join("pkg/rsx.css"),
            tailwind_css: config
                .project
                .tailwind
                .enabled
                .then(|| config.dist.join(config::TAILWIND_FILE)),
            manifest: manifest.clone(),
            hub: hub.clone(),
            renderer: self.renderer.clone(),
        };

        let mut internal = Router::<Shared>::new()
            .nest_service("/pkg", ServeDir::new(&pkg_dir).precompressed_br())
            .nest_service("/assets", ServeDir::new(assets_dir).precompressed_br())
            .fallback(shell);
        if hub.is_some() {
            let literals = config.dev_dir.join("literals.json");
            internal = internal.route(
                "/__yawn/literals.json",
                get(move || {
                    let literals = literals.clone();
                    async move {
                        let body =
                            tokio::fs::read(&literals).await.unwrap_or_else(|_| b"{}".to_vec());
                        ([(header::CONTENT_TYPE, "application/json")], body)
                    }
                }),
            );
            internal = internal
                .route("/__yawn/events", get(events))
                .route("/__yawn/loaded", get(loaded))
                .route("/__yawn/rev", get(current_rev));
        }
        // User routes first: they take precedence and may not collide with the built-in ones.
        // gzip or brotli, whatever the browser asks for; event streams are left alone. Dev uses the
        // fastest level, the default takes ~250 ms on every reload
        let level =
            if hub.is_some() { CompressionLevel::Fastest } else { CompressionLevel::Default };
        let mut router = self.router.merge(internal.with_state(shared));
        if let Some(dict) = (hub.is_none()).then(|| Dictionary::load(&pkg_dir)).flatten() {
            router = router.layer(middleware::from_fn_with_state(dict, dictionary));
        }
        router = router.layer(CompressionLayer::new().quality(level));
        if hub.is_some() {
            router = router
                .layer(middleware::from_fn(log_requests))
                .layer(middleware::from_fn(development_cache));
        } else if manifest.is_some() {
            router = router.layer(middleware::from_fn(production_cache));
        }

        if let Some(hub) = hub {
            emit("ready", &format!("http://{local}"));
            tokio::spawn(control_loop(hub, shutdown.clone()));
        } else {
            println!("yawn: {} listening on http://{local}", self.name);
        }

        // Open event streams would block a graceful shutdown, so stop waits only a moment
        let grace = std::time::Duration::from_millis(if config.dev { 30 } else { 300 });
        let stop = shutdown.clone();
        let stopped = async move {
            race(stop.notified(), tokio::signal::ctrl_c()).await;
            tokio::time::sleep(grace).await;
        };
        accept_loop(listener, router, stopped).await.map_err(Error::Io)
    }
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    stop: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;
    let mut stop = std::pin::pin!(stop);
    loop {
        let accepted = race(listener.accept(), stop.as_mut()).await;
        let Some(accepted) = accepted else {
            return Ok(());
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            // a failed accept (too many open files, a reset before it completed) must not end the server
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let service = TowerToHyperService::new(router.clone());
        tokio::spawn(async move {
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

/// Runs both futures at once and returns both outputs
pub async fn join<A: std::future::Future, B: std::future::Future>(
    a: A,
    b: B,
) -> (A::Output, B::Output) {
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    let (mut first, mut second) = (None, None);
    std::future::poll_fn(|cx| {
        if first.is_none() {
            first = match a.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => Some(v),
                std::task::Poll::Pending => None,
            };
        }
        if second.is_none() {
            second = match b.as_mut().poll(cx) {
                std::task::Poll::Ready(v) => Some(v),
                std::task::Poll::Pending => None,
            };
        }
        if first.is_some() && second.is_some() {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    })
    .await;
    (first.expect("polled to completion"), second.expect("polled to completion"))
}

async fn race<A: std::future::Future, B: std::future::Future>(a: A, b: B) -> Option<A::Output> {
    let (mut a, mut b) = (std::pin::pin!(a), std::pin::pin!(b));
    std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
            return std::task::Poll::Ready(Some(v));
        }
        if b.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(None);
        }
        std::task::Poll::Pending
    })
    .await
}

/// The HTML page that boots the client
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub title: String,
    pub stylesheets: Vec<String>,
    /// Scripts that run once the page is parsed, before the client starts
    pub scripts: Vec<String>,
    /// Module that initialises the WASM client
    pub entry: String,
    /// The asset manifest as JSON, for the client to resolve paths it builds (production only)
    pub assets: Option<String>,
}

/// State handed to the page in development mode
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevBoot {
    pub rev: u64,
    pub error: Option<String>,
    /// A build in progress: crates done, crates in total (0 = unknown), what's compiling
    pub progress: Option<(u32, u32, String)>,
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

impl Document {
    pub fn render(&self, dev: Option<&DevBoot>) -> String {
        self.render_page(dev, None)
    }

    /// The page with `page` (a server render) inside `<div id="app">`, its title and head tags
    pub fn render_page(&self, dev: Option<&DevBoot>, page: Option<&ssr::Page>) -> String {
        let mut s = String::with_capacity(1024);
        s.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
        s.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1, interactive-widget=resizes-content\">\n");
        let title = page.and_then(|p| p.title.as_deref()).unwrap_or(&self.title);
        s.push_str(&format!("<title>{}</title>\n", escape_html(title)));
        if let Some(p) = page {
            s.push_str(&p.head);
        }
        for href in &self.stylesheets {
            s.push_str(&format!("<link rel=\"stylesheet\" href=\"{}\">\n", escape_html(href)));
        }
        s.push_str("</head>\n<body>\n<div id=\"app\">");
        if let Some(p) = page {
            s.push_str(&p.html);
        }
        s.push_str("</div>\n");
        for src in &self.scripts {
            s.push_str(&format!("<script src=\"{}\"></script>\n", escape_html(src)));
        }
        if let Some(boot) = dev {
            let error = boot.error.as_deref().map_or("null".to_string(), json_string);
            let progress = boot.progress.as_ref().map_or("null".to_string(), |(d, t, l)| {
                format!("{{done:{d},total:{t},label:{}}}", json_string(l))
            });
            s.push_str(&format!(
                "<script>window.__yawn={{rev:{},error:{},progress:{}}};</script>\n<script>{}</script>\n",
                boot.rev,
                error,
                progress,
                include_str!("dev_client.js")
            ));
        }
        if let Some(json) = &self.assets {
            s.push_str(&format!("<script>window.__yawnAssets={json};</script>\n"));
        }
        // In development the browser is told which build it asks for, so no cache serves a stale one
        let entry_url_json = format!("{:?}", self.entry);
        let (entry, init) = match dev {
            Some(boot) => (
                format!("{}?v={}", self.entry, boot.rev),
                format!(
                    "init({{ module_or_path: new URL(\"app_bg.wasm?v={}\", new URL({}, location.href)) }})",
                    boot.rev, entry_url_json
                ),
            ),
            None => (self.entry.clone(), "init()".to_string()),
        };
        if dev.is_some() {
            s.push_str("<script type=\"module\">window.__yawnLiteralsReady = fetch('/__yawn/literals.json', {cache:'no-store'}).then(r => { if (!r.ok) throw new Error('Cannot load dev literals'); return r.json(); }).then(v => { window.__yawnLiterals = v; });</script>\n");
        }
        s.push_str(&format!(
            "<script type=\"module\">\nimport init, * as client from \"{}\";\nconst boot = {{}};\nawait Promise.all([...document.querySelectorAll(\"link[data-yawn-boot]\")].map(async (l) => {{ try {{ const r = await fetch(l.href); if (r.ok) boot[l.dataset.yawnBoot] = await r.text(); }} catch {{}} }}));\nwindow.__yawnBoot = boot;\nawait window.__yawnLiteralsReady;\nawait {};\nif (window.__yawnLoaded) {{ window.__yawnModule = client; window.__yawnLoaded(); }}\n</script>\n</body>\n</html>\n",
            escape_html(&entry),
            init
        ));
        s
    }
}

struct DevHub {
    tx: broadcast::Sender<String>,
    ssr_stale: std::sync::atomic::AtomicBool,
    state: Mutex<DevBoot>,
}

impl DevHub {
    fn new(rev: u64) -> Self {
        DevHub {
            tx: broadcast::channel(64).0,
            ssr_stale: std::sync::atomic::AtomicBool::new(false),
            state: Mutex::new(DevBoot { rev, error: None, progress: None }),
        }
    }

    fn boot(&self) -> DevBoot {
        self.state.lock().unwrap().clone()
    }

    fn apply(&self, c: Control) {
        if matches!(c, Control::NoSsr { .. }) {
            self.ssr_stale.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        if let Control::Data { path, .. } = &c {
            crate::dev::data_changed(std::path::Path::new(path));
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            match &c {
                Control::Reload { rev } | Control::Patch { rev } => {
                    st.rev = *rev;
                    st.error = None;
                    st.progress = None;
                }
                Control::Css { rev, .. } => {
                    st.rev = *rev;
                    st.progress = None;
                }
                Control::Error { rev, text } => {
                    st.rev = *rev;
                    st.error = Some(text.clone());
                    st.progress = None;
                }
                Control::NoSsr { .. } | Control::Data { .. } => {}
                Control::Progress { done, total, label, .. } => {
                    // the revision isn't the page's yet: only a finished build changes it
                    st.progress =
                        (*total > 0 || !label.is_empty()).then(|| (*done, *total, label.clone()));
                }
            }
        }
        // No receivers means no browser is connected right now.
        let _ = self.tx.send(c.to_json());
    }
}

async fn control_loop(hub: Arc<DevHub>, shutdown: Arc<Notify>) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(c) = parse_control(&line) {
            hub.apply(c);
        }
    }
    shutdown.notify_one();
}

fn emit(kind: &str, msg: &str) {
    println!("@yawn {kind} {msg}");
}

#[derive(Clone)]
struct Shared {
    doc: Arc<Document>,
    rsx_css: PathBuf,
    tailwind_css: Option<PathBuf>,
    manifest: Option<Arc<Manifest>>,
    renderer: Option<Renderer>,
    hub: Option<Arc<DevHub>>,
}

async fn shell(State(s): State<Shared>, uri: Uri, headers: HeaderMap) -> Response {
    // A path that looks like a file is answered as a missing file.
    let path = uri.path();
    if path == "/favicon.ico" {
        return StatusCode::NO_CONTENT.into_response();
    }
    if path.starts_with("/__yawn") || path.rsplit('/').next().is_some_and(|seg| seg.contains('.')) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let boot = s.hub.as_ref().map(|h| h.boot());
    let has = |url: &str, file: Option<&PathBuf>| match &s.manifest {
        Some(m) => m.get(url).is_some(),
        None => file.is_some_and(|p| p.is_file()),
    };
    let link = |url: &str| s.manifest.as_ref().map_or_else(|| url.to_string(), |m| m.resolve(url));
    let mut doc = (*s.doc).clone();
    if has(config::TAILWIND_URL, s.tailwind_css.as_ref()) {
        // first, so the project's own stylesheets can override it
        doc.stylesheets.insert(0, link(config::TAILWIND_URL));
    }
    if has("/pkg/rsx.css", Some(&s.rsx_css)) {
        doc.stylesheets.push(link("/pkg/rsx.css"));
    }
    let stale =
        s.hub.as_ref().is_some_and(|h| h.ssr_stale.load(std::sync::atomic::Ordering::SeqCst));
    let page = match &s.renderer {
        Some(render) => {
            let request = ssr::Request {
                url: uri
                    .path_and_query()
                    .map_or_else(|| path.to_string(), |p| p.as_str().to_string()),
                locale: String::new(),
                cookies: headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string(),
            };
            let render = render.clone();
            let dev = s.hub.is_some();
            // a thread of the blocking pool: the render is CPU work and uses thread-local state
            match tokio::task::spawn_blocking(move || {
                if dev {
                    crate::dev::refresh_literals();
                    crate::dev::refresh_native();
                }
                render(request)
            })
            .await
            {
                Ok(mut page) => {
                    if stale {
                        page.html.clear();
                    }
                    Some(page)
                }
                Err(e) => {
                    emit("log", &format!("server render failed, sending the empty shell: {e}"));
                    None
                }
            }
        }
        None => None,
    };
    Html(doc.render_page(boot.as_ref(), page.as_ref())).into_response()
}

async fn events(
    State(s): State<Shared>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = s.hub.as_ref().expect("dev only route").tx.subscribe();
    let stream =
        BroadcastStream::new(rx).filter_map(|m| m.ok().map(|json| Ok(Event::default().data(json))));
    // an empty comment now and then keeps proxies from closing a quiet stream (`Sse::keep_alive` needs axum's `tokio` feature)
    let period = std::time::Duration::from_secs(15);
    let ticks =
        IntervalStream::new(tokio::time::interval_at(tokio::time::Instant::now() + period, period))
            .map(|_| Ok(Event::default().comment("")));
    Sse::new(stream.merge(ticks))
}

async fn current_rev(State(s): State<Shared>) -> Response {
    let rev = s.hub.as_ref().map_or(0, |h| h.boot().rev);
    ([(header::CACHE_CONTROL, "no-store")], rev.to_string()).into_response()
}

async fn loaded(Query(q): Query<HashMap<String, String>>) -> StatusCode {
    let rev = q.get("rev").map(String::as_str).unwrap_or("0");
    let kind = q.get("kind").map(String::as_str).unwrap_or("reload");
    emit("loaded", &format!("{rev} {kind}"));
    StatusCode::NO_CONTENT
}

/// Dictionary-coded files. A route chunk built against the main module (`chunk_N.<hash>.wasm.dcz`)
/// goes to browsers that announce the main module as a dictionary, and a file built against an
/// earlier build of itself (`name.from-<sha256>.dcz`) to browsers that hold that build, which turns
/// a deploy into a small diff
struct Dictionary {
    pkg: PathBuf,
    id: String,
    deltas: HashMap<(String, String), PathBuf>,
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from(":");
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out.push(':');
    out
}

impl Dictionary {
    fn load(pkg: &std::path::Path) -> Option<Arc<Self>> {
        use sha2::{Digest, Sha256};
        let mut any = false;
        let mut deltas = HashMap::new();
        for e in std::fs::read_dir(pkg).ok()?.flatten() {
            let file = e.file_name().to_string_lossy().into_owned();
            any |= file.ends_with(".wasm.dcz");
            if let Some((name, rest)) = file.split_once(".from-")
                && let Some(hex) = rest.strip_suffix(".dcz")
                && hex.len() == 64
                && let Some(sha) = (0..32).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect::<Option<Vec<u8>>>()
            {
                deltas.insert((name.to_string(), base64(&sha)), e.path());
                any = true;
            }
        }
        if !any {
            return None;
        }
        let id = base64(&Sha256::digest(std::fs::read(pkg.join("app_bg.wasm")).ok()?));
        Some(Arc::new(Self { pkg: pkg.to_path_buf(), id, deltas }))
    }
}

async fn dictionary(State(dict): State<Arc<Dictionary>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let name = path.strip_prefix("/pkg/").filter(|n| n.ends_with(".wasm") && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'));
    let chunk = name.is_some_and(|n| n.starts_with("chunk_"));
    let main = name.is_some_and(|n| n.starts_with("app_bg."));
    if let Some(name) = name.filter(|_| chunk || main) {
        let (accepts, available) = {
            let get = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            (get("accept-encoding").split(',').any(|e| e.trim().starts_with("dcz")), get("available-dictionary"))
        };
        if accepts {
            let file = dict
                .deltas
                .get(&(name.to_string(), available.clone()))
                .cloned()
                .or_else(|| (chunk && available == dict.id).then(|| dict.pkg.join(format!("{name}.dcz"))));
            if let Some(file) = file
                && let Ok(body) = tokio::fs::read(file).await
            {
                let mut res = Response::new(axum::body::Body::from(body));
                let h = res.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/wasm"));
                h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("dcz"));
                h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding, Available-Dictionary"));
                announce(&mut res, name);
                return res;
            }
        }
    }
    let mut res = next.run(req).await;
    if let Some(name) = name.filter(|_| chunk || main) {
        res.headers_mut().append(header::VARY, HeaderValue::from_static("Available-Dictionary"));
        if res.status().is_success() {
            announce(&mut res, name);
        }
    }
    res
}

/// Marks a file as the dictionary for later builds of itself. A chunk is matched by its number, the
/// main module also covers the chunks that have no earlier build of their own
fn announce(res: &mut Response, name: &str) {
    let pattern = match name.split_once('.') {
        Some((stem, _)) if stem.starts_with("chunk_") => format!("/pkg/{stem}.*.wasm"),
        _ => "/pkg/*.wasm".to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&format!("match=\"{pattern}\"")) {
        res.headers_mut().insert("use-as-dictionary", v);
    }
}

async fn production_cache(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    if !res.headers().contains_key(header::CACHE_CONTROL) {
        let value = if res.status().is_success() && assets::is_hashed(&path) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    }
    res
}

async fn development_cache(req: Request, next: Next) -> Response {
    let versioned = req.uri().path().starts_with("/pkg/") && req.uri().query().is_some_and(|q| q.starts_with("v="));
    let value = if versioned { "public, max-age=31536000, immutable" } else { "no-store" };
    let mut res = next.run(req).await;
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    res
}

async fn log_requests(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let res = next.run(req).await;
    if !path.starts_with("/__yawn") {
        emit(
            "req",
            &format!("{} {}ms {method} {path}", res.status().as_u16(), start.elapsed().as_millis()),
        );
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn doc() -> Document {
        Document {
            title: "A & <B>".into(),
            stylesheets: vec!["/assets/style.css".into()],
            scripts: vec!["/assets/site.js".into()],
            entry: "/pkg/app.js".into(),
            assets: None,
        }
    }

    #[test]
    fn document_runs_scripts_before_the_client_starts() {
        let h = doc().render(None);
        let script = h.find("<script src=\"/assets/site.js\"></script>").unwrap();
        assert!(h.find("</div>").unwrap() < script);
    }

    #[test]
    fn document_escapes_and_links() {
        let h = doc().render(None);
        assert!(h.contains("<title>A &amp; &lt;B&gt;</title>"));
        assert!(h.contains("<link rel=\"stylesheet\" href=\"/assets/style.css\">"));
        assert!(h.contains("import init, * as client from \"/pkg/app.js\""));
        assert!(
            !h.contains("EventSource") && !h.contains("window.__yawn="),
            "no dev code in production output"
        );
    }

    #[test]
    fn dev_document_embeds_boot_state_safely() {
        let boot =
            DevBoot { rev: 7, error: Some("</script><b>x\"y\n".into()), ..DevBoot::default() };
        let h = doc().render(Some(&boot));
        assert!(h.contains("rev:7"));
        assert!(h.contains("EventSource"));
        assert!(!h.contains("</script><b>"), "error text cannot break out of the script");
        assert!(h.contains("\\u003c/script\\u003e"));
        // the wasm sits next to the entry module (inline modules have the page's URL)
        assert!(
            h.contains("new URL(\"app_bg.wasm?v=7\", new URL(\"/pkg/app.js\", location.href))"),
            "{h}"
        );
        assert!(!h.contains("import.meta.url"));
    }

    #[test]
    fn hub_tracks_state_for_late_joining_pages() {
        let hub = DevHub::new(1);
        hub.apply(Control::Error { rev: 2, text: "boom".into() });
        assert_eq!(hub.boot(), DevBoot { rev: 2, error: Some("boom".into()), progress: None });
        hub.apply(Control::Progress { rev: 3, done: 4, total: 9, label: "serde".into() });
        assert_eq!(hub.boot().progress, Some((4, 9, "serde".into())));
        hub.apply(Control::Progress { rev: 3, done: 0, total: 0, label: String::new() });
        assert_eq!(hub.boot().progress, None);
        hub.apply(Control::Progress { rev: 3, done: 1, total: 2, label: "x".into() });
        hub.apply(Control::Reload { rev: 3 });
        assert_eq!(hub.boot(), DevBoot { rev: 3, error: None, progress: None });
    }

    fn http_get(addr: SocketAddr, path: &str) -> (u16, String, String) {
        http_get_with(addr, path, "")
    }

    fn http_get_with(addr: SocketAddr, path: &str, extra_headers: &str) -> (u16, String, String) {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra_headers}\r\n")
            .unwrap();
        let mut out = String::new();
        // compressed bodies aren't UTF-8: read bytes and keep what's text
        let mut bytes = Vec::new();
        s.read_to_end(&mut bytes).unwrap();
        out.push_str(&String::from_utf8_lossy(&bytes));
        let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head.to_string(), body.to_string())
    }

    #[test]
    fn serves_shell_assets_pkg_and_404s_missing_files() {
        let dir = std::env::temp_dir().join(format!("yawn-srv-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("dist/pkg")).unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/style.css"), "body{color:red}").unwrap();
        std::fs::write(dir.join("assets/big.css"), "a{color:red}\n".repeat(400)).unwrap();
        std::fs::write(dir.join("dist/pkg/app_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
        let config = ServeConfig {
            addr: "127.0.0.1:0".parse().unwrap(),
            port_source: config::PortSource::Default,
            root: dir.clone(),
            dist: dir.join("dist"),
            dev_dir: dir.join("dev"),
            assets: dir.join("assets"),
            public: None,
            dev: false,
            rev: 0,
            project: ProjectConfig::default(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let cfg = config.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let l = tokio::net::TcpListener::bind(cfg.addr).await.unwrap();
                tx.send(l.local_addr().unwrap()).unwrap();
                let extra = Router::new().route("/api/ping", get(|| async { "pong" }));
                let _ = App::new("t").title("T").merge(extra).serve_listener(l, cfg).await;
            });
        });
        let addr = rx.recv().unwrap();

        let (st, _, body) = http_get(addr, "/");
        assert_eq!(st, 200);
        assert!(body.contains("<title>T</title>") && body.contains("id=\"app\""));
        let (st, _, body) = http_get(addr, "/some/client/route");
        assert_eq!(
            (st, body.contains("id=\"app\"")),
            (200, true),
            "unknown paths fall back to the shell"
        );
        let (st, head, body) = http_get(addr, "/assets/style.css");
        assert_eq!((st, body.as_str()), (200, "body{color:red}"));
        assert!(head.to_lowercase().contains("text/css"));
        let (st, head, _) = http_get(addr, "/pkg/app_bg.wasm");
        assert_eq!(st, 200);
        assert!(head.to_lowercase().contains("application/wasm"), "{head}");
        let (_, head, _) = http_get_with(addr, "/assets/big.css", "Accept-Encoding: gzip\r\n");
        assert!(
            head.to_lowercase().contains("content-encoding: gzip"),
            "assets are compressed when the browser accepts it: {head}"
        );
        let (_, head, _) = http_get(addr, "/assets/big.css");
        assert!(!head.to_lowercase().contains("content-encoding"), "and not otherwise: {head}");
        assert_eq!(http_get(addr, "/pkg/missing.js").0, 404);
        assert_eq!(http_get(addr, "/nope.png").0, 404);
        assert_eq!(http_get(addr, "/api/ping").2, "pong", "app routes are served");
        assert_eq!(http_get(addr, "/__yawn/events").0, 404, "no dev endpoints in production mode");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Default)]
    struct Landing;
    impl Component for Landing {
        fn render(self) -> crate::ui::View {
            use crate::ui::rt;
            let h = rt::element("h1");
            let url = ssr::request().map(|r| r.url).unwrap_or_default();
            rt::add(h, rt::text(&format!("page {url}")));
            crate::ui::set_title("Landing");
            crate::ui::head::meta("description", "d");
            h.into()
        }
    }

    #[test]
    fn ssr_puts_the_rendered_page_title_and_head_into_the_shell() {
        let dir = std::env::temp_dir().join(format!("yawn-ssr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = ServeConfig {
            addr: "127.0.0.1:0".parse().unwrap(),
            port_source: config::PortSource::Default,
            root: dir.clone(),
            dist: dir.join("dist"),
            dev_dir: dir.join("dev"),
            assets: dir.join("assets"),
            public: None,
            dev: false,
            rev: 0,
            project: ProjectConfig::default(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let l = tokio::net::TcpListener::bind(config.addr).await.unwrap();
                tx.send(l.local_addr().unwrap()).unwrap();
                let _ = App::new("t").title("T").ssr::<Landing>().serve_listener(l, config).await;
            });
        });
        let addr = rx.recv().unwrap();
        let (st, _, body) = http_get(addr, "/guilds?x=1");
        assert_eq!(st, 200);
        assert!(body.contains("<div id=\"app\"><h1>page /guilds?x=1</h1></div>"), "{body}");
        assert!(
            body.contains("<title>Landing</title>")
                && body.contains("<meta name=\"description\" content=\"d\">"),
            "{body}"
        );
        let (_, _, again) = http_get(addr, "/other");
        assert!(again.contains("page /other"), "each request is rendered for its own URL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nossr_preserves_request_boot_data_without_stale_markup() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let hub = Arc::new(DevHub::new(1));
            let shared = Shared {
                doc: Arc::new(doc()),
                rsx_css: PathBuf::new(),
                tailwind_css: None,
                manifest: None,
                hub: Some(hub.clone()),
                renderer: Some(Arc::new(|request| {
                    assert_eq!(request.cookies, "session=test");
                    let lang = request.url.split('/').nth(1).unwrap().to_string();
                    struct Localized(String);
                    impl Component for Localized {
                        fn render(self) -> crate::ui::View {
                            crate::ui::head::boot("ftl", &format!("/translations/{}.ftl", self.0));
                            crate::ui::head::boot("session", "/api/session");
                            crate::ui::rt::text("server markup").into()
                        }
                    }
                    ssr::render(request, Localized(lang))
                })),
            };
            for (command, path, has_markup) in [
                (None, "/pl/dashboard", true),
                (Some("nossr 2"), "/pl/dashboard", false),
                (Some("reload 2"), "/en/dashboard", false),
            ] {
                if let Some(command) = command {
                    hub.apply(parse_control(command).unwrap());
                }
                let mut headers = HeaderMap::new();
                headers.insert(header::COOKIE, HeaderValue::from_static("session=test"));
                let response = shell(State(shared.clone()), path.parse().unwrap(), headers).await;
                assert_eq!(response.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let html = String::from_utf8(bytes.to_vec()).unwrap();
                let lang = path.split('/').nth(1).unwrap();
                assert!(html.contains(&format!("href=\"/translations/{lang}.ftl\"")), "{html}");
                assert!(html.contains("data-yawn-boot=\"ftl\""), "{html}");
                assert!(html.contains("href=\"/api/session\""), "{html}");
                assert_eq!(html.contains("server markup"), has_markup, "{html}");
                if !has_markup {
                    assert!(html.contains("<div id=\"app\"></div>"), "{html}");
                }
            }
        });
    }

    #[test]
    fn production_build_serves_hashed_files_forever_and_the_page_never() {
        let dir = std::env::temp_dir().join(format!("yawn-prod-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let public = dir.join("public");
        std::fs::create_dir_all(public.join("assets")).unwrap();
        std::fs::create_dir_all(public.join("pkg")).unwrap();
        std::fs::write(public.join("assets/prodtest.0123456789ab.css"), "p{color:red}").unwrap();
        std::fs::write(public.join("pkg/app.aaaaaaaaaaaa.js"), "export default 1").unwrap();
        let mut m = Manifest::new();
        m.insert("/assets/prodtest.css", "/assets/prodtest.0123456789ab.css");
        m.insert("/pkg/app.js", "/pkg/app.aaaaaaaaaaaa.js");
        std::fs::write(public.join(assets::MANIFEST_FILE), m.to_json()).unwrap();
        let config = ServeConfig {
            addr: "127.0.0.1:0".parse().unwrap(),
            port_source: config::PortSource::Default,
            root: dir.clone(),
            dist: dir.join("dist"),
            dev_dir: dir.join("dev"),
            assets: dir.join("assets"),
            public: Some(public),
            dev: false,
            rev: 0,
            project: ProjectConfig::default(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                let l = tokio::net::TcpListener::bind(config.addr).await.unwrap();
                tx.send(l.local_addr().unwrap()).unwrap();
                let _ = App::new("t")
                    .stylesheets(["/assets/prodtest.css"])
                    .serve_listener(l, config)
                    .await;
            });
        });
        let addr = rx.recv().unwrap();
        let (st, head, body) = http_get(addr, "/");
        assert_eq!(st, 200);
        assert!(
            body.contains("href=\"/assets/prodtest.0123456789ab.css\""),
            "stylesheet by its hashed name: {body}"
        );
        assert!(
            body.contains("import init, * as client from \"/pkg/app.aaaaaaaaaaaa.js\""),
            "{body}"
        );
        assert!(
            body.contains("window.__yawnAssets={") && body.contains("prodtest.0123456789ab.css"),
            "the client gets the map: {body}"
        );
        assert!(
            head.to_lowercase().contains("cache-control: no-cache"),
            "the page is always checked: {head}"
        );
        let (st, head, body) = http_get(addr, "/assets/prodtest.0123456789ab.css");
        assert_eq!((st, body.as_str()), (200, "p{color:red}"));
        assert!(
            head.to_lowercase().contains("cache-control: public, max-age=31536000, immutable"),
            "{head}"
        );
        assert_eq!(
            http_get(addr, "/assets/prodtest.css").0,
            404,
            "the written name is not a file of the build"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
