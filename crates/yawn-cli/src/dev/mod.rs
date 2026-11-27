pub mod cargo;
pub mod changes;
pub mod dynamic;
pub mod fetch;
mod literals;
pub mod loading;
pub mod project;
pub mod replay;
pub mod rsx;
pub mod tailwind;
pub mod wasm_bindgen;

use crate::build::{CLIENT_TARGET_DIR, SERVER_TARGET_DIR};
use crate::event::{BuildEvent, Event, Level, Location, LogEvent, fmt_duration};
use crate::pipeline::{EventSink, EventSource};
use cargo::{KillGroup, Parsed};
use changes::ChangeSet;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use project::Project;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use yawn::config;

const DEBOUNCE: Duration = Duration::from_millis(40);
const MAX_BURST: Duration = Duration::from_millis(400);
const WASM_TARGET: &str = "wasm32-unknown-unknown";
const CLIENT_BUNDLE: &str = "app";

pub struct DevSource {
    pub project: Project,
    /// `--port`, passed to the server
    pub port: Option<u16>,
}

impl EventSource for DevSource {
    fn run(self: Box<Self>, sink: EventSink) {
        Dev::new(*self, sink).run();
    }
}

enum Cmd {
    Fs(Vec<PathBuf>, Instant),
    Server {
        r#gen: u64,
        line: String,
        err: bool,
    },
    ServerExit {
        r#gen: u64,
        status: String,
    },
    /// `r` in the TUI
    Reload,
    Stop,
}

enum CargoMsg {
    Event(&'static str, Parsed),
    Done(&'static str, Box<Finished>),
}

struct Finished {
    run: std::io::Result<cargo::CargoRun>,
    wasm: Option<(PathBuf, bool)>,
    exe: Option<PathBuf>,
}

struct CargoDone {
    ok: bool,
    client: Option<(bool, Duration, Option<Duration>)>,
    server: Option<Duration>,
}

struct ServerProc {
    stdin: std::cell::RefCell<Option<ChildStdin>>,
    done: Arc<(Mutex<bool>, Condvar)>,
}

fn bundle_is_current(wasm: &std::path::Path, bundle: &std::path::Path) -> bool {
    let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!((modified(wasm), modified(bundle)), (Some(w), Some(b)) if b >= w)
}

struct Dev {
    project: Project,
    port: Option<u16>,
    sink: EventSink,
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    build_kill: Arc<KillGroup>,
    server_kill: Arc<KillGroup>,
    stop: Arc<AtomicBool>,
    server: Option<ServerProc>,
    loading: Option<loading::Loading>,
    tailwind: Option<tailwind::Tailwind>,
    r#gen: u64,
    server_exe: Option<PathBuf>,
    server_rebuilt: bool,
    server_baseline: Option<literals::Snapshot>,
    snapshot_cache: literals::Cache,
    lanes: [Option<Arc<Mutex<replay::Lane>>>; 2],
    dynamic: Option<dynamic::Dynamic>,
    dropped_dynamic: bool,
    rev: u64,
    pending: ChangeSet,
    saves_seen: Arc<AtomicU64>,
    cycle_saves: u64,
    carry: ChangeSet,
    build_id: u32,
    last_progress: std::cell::Cell<Option<Instant>>,
    saves: HashMap<u64, Instant>,
    last_error: Option<String>,
    bindgen: Option<wasm_bindgen::Tool>,
    literal_baseline: Option<literals::Snapshot>,
    literal_patch: bool,
    _watcher: Option<RecommendedWatcher>,
}

impl Dev {
    fn new(src: DevSource, sink: EventSink) -> Dev {
        let (tx, rx) = mpsc::channel();
        let lane = |name: &str| {
            replay::Lane::new(&src.project.root, src.project.dev_dir.join(name))
                .map(|l| Arc::new(Mutex::new(l)))
        };
        let wanted = yawn_rsx::project::split_enabled(&src.project.root)
            && yawn_rsx::project::dynamic_enabled(&src.project.root);
        let dynamic = if wanted {
            match dynamic::available() {
                Ok(()) => Some(dynamic::Dynamic::new(&src.project.root, &src.project.dev_dir)),
                Err(why) => {
                    sink.log(Level::Info, "client", format!("route hot-swap off: {why}"));
                    None
                }
            }
        } else {
            None
        };
        let lanes = [
            lane("replay-client.log"),
            lane("replay-server.log"),
        ];
        Dev {
            project: src.project,
            port: src.port,
            sink,
            rx,
            tx,
            lanes,
            dynamic,
            dropped_dynamic: false,
            build_kill: Arc::new(KillGroup::default()),
            server_kill: Arc::new(KillGroup::default()),
            stop: Arc::new(AtomicBool::new(false)),
            server: None,
            loading: None,
            tailwind: None,
            r#gen: 0,
            server_exe: None,
            server_rebuilt: false,
            server_baseline: None,
            snapshot_cache: Default::default(),
            rev: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            pending: ChangeSet::default(),
            saves_seen: Arc::new(AtomicU64::new(0)),
            cycle_saves: 0,
            carry: ChangeSet::default(),
            build_id: 0,
            last_progress: std::cell::Cell::new(None),
            saves: HashMap::new(),
            last_error: None,
            bindgen: None,
            literal_baseline: None,
            literal_patch: false,
            _watcher: None,
        }
    }

    fn log(&self, level: Level, source: &'static str, msg: impl AsRef<str>) {
        self.sink.log(level, source, msg);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn superseded(&self) -> bool {
        self.saves_seen.load(Ordering::SeqCst) != self.cycle_saves
    }

    fn run(&mut self) {
        // Shutdown: abort a running build and wake the loop
        {
            let (stop, kill, tx) = (self.stop.clone(), self.build_kill.clone(), self.tx.clone());
            self.sink.shutdown().on_shutdown(move || {
                stop.store(true, Ordering::SeqCst);
                kill.kill();
                let _ = tx.send(Cmd::Stop);
            });
        }
        self.log(
            Level::Info,
            "dev",
            format!("project {} at {}", self.project.display_name(), self.project.root.display()),
        );
        match config::resolve_port(
            self.port,
            std::env::var("PORT").ok().as_deref(),
            self.project.config.server.port,
        ) {
            Ok(p) => {
                self.log(
                    Level::Info,
                    "dev",
                    format!("port {} (from {})", p.port, p.source.describe()),
                );
                let host = self
                    .project
                    .config
                    .server
                    .host
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
                self.loading = loading::Loading::start(host, p.port);
            }
            Err(e) => {
                self.log(Level::Error, "dev", e.to_string());
                return;
            }
        }
        {
            let tx = self.tx.clone();
            self.sink.reload().on_reload(move || {
                let _ = tx.send(Cmd::Reload);
            });
        }
        self.sink.info("server", "starting");
        if let Err(e) = self.start_watcher() {
            self.log(Level::Error, "watch", format!("cannot watch files: {e}"));
            return;
        }
        self.cycle(ChangeSet::initial());

        while !self.stopping() {
            let Ok(first) = self.rx.recv() else { break };
            self.handle(first);
            while let Ok(m) = self.rx.try_recv() {
                self.handle(m);
            }
            if self.stopping() {
                break;
            }
            if !self.pending.is_empty() {
                let cs = std::mem::take(&mut self.pending);
                self.cycle(cs);
            }
        }
        self.stop_server();
        self.log(Level::Info, "dev", "session ended");
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Stop => {}
            Cmd::Reload => self.pending.merge(ChangeSet::manual()),
            Cmd::Fs(paths, at) => {
                if paths.iter().any(|p| self.is_manifest(p)) {
                    self.refresh_project();
                }
                for p in paths {
                    self.pending.add(
                        &self.project.root,
                        &self.project.assets,
                        &self.project.data,
                        &self.project.watched_crates(),
                        &p,
                        at,
                    );
                }
            }
            Cmd::Server { r#gen, line, err } if r#gen == self.r#gen => self.server_line(&line, err),
            Cmd::ServerExit { r#gen, status } if r#gen == self.r#gen => {
                self.server = None;
                self.server_kill.clear();
                self.log(Level::Error, "server", format!("server exited ({status})"));
                self.sink.info("server", "stopped");
            }
            Cmd::Server { .. } | Cmd::ServerExit { .. } => {}
        }
    }

    fn is_manifest(&self, path: &Path) -> bool {
        let name = path.file_name();
        (name.is_some_and(|n| n == "Cargo.toml")
            && (path.parent() == Some(self.project.root.as_path())
                || self
                    .project
                    .watched_crates()
                    .iter()
                    .any(|c| path.parent() == Some(c.as_path()))))
            || (name.is_some_and(|n| n == "Yawn.toml")
                && path.parent() == Some(self.project.root.as_path()))
    }

    fn refresh_project(&mut self) {
        match Project::open(self.project.root.clone()) {
            Ok(project) => {
                if project.watched_crates() == self.project.watched_crates()
                    && project.data == self.project.data
                    && project.assets == self.project.assets
                {
                    return;
                }
                self.project = project;
                self._watcher = None;
                if let Err(e) = self.start_watcher() {
                    self.log(Level::Error, "watch", format!("cannot watch files: {e}"));
                }
            }
            Err(e) => self.log(Level::Error, "dev", e.to_string()),
        }
    }

    fn start_watcher(&mut self) -> notify::Result<()> {
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<PathBuf>>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res
                    && matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    )
                {
                    let _ = raw_tx.send(ev.paths);
                }
            })?;
        let root = &self.project.root;
        watcher.watch(root, RecursiveMode::NonRecursive)?;
        for dir in [root.join("src"), self.project.assets.clone()]
            .into_iter()
            .chain(self.project.data.iter().cloned())
        {
            if dir.is_dir() {
                watcher.watch(&dir, RecursiveMode::Recursive)?;
            }
        }
        for lib in &self.project.watched_crates() {
            if lib.join("src").is_dir() {
                watcher.watch(&lib.join("src"), RecursiveMode::Recursive)?;
            }
        }
        let tx = self.tx.clone();
        let (saves, root, assets) =
            (self.saves_seen.clone(), self.project.root.clone(), self.project.assets.clone());
        let data = self.project.data.clone();
        let libs = self.project.watched_crates();
        std::thread::Builder::new().name("yawn-debounce".into()).spawn(move || {
            while let Ok(first) = raw_rx.recv() {
                let started = Instant::now();
                let mut batch = first;
                while started.elapsed() < MAX_BURST {
                    match raw_rx.recv_timeout(DEBOUNCE) {
                        Ok(more) => batch.extend(more),
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                let builds = batch.iter().any(|p| {
                    matches!(
                        changes::classify(&root, &assets, &data, &libs, p),
                        Some(
                            changes::Change::Both
                                | changes::Change::Client
                                | changes::Change::Server
                        )
                    )
                });
                if builds {
                    saves.fetch_add(1, Ordering::SeqCst);
                }
                if tx.send(Cmd::Fs(batch, started)).is_err() {
                    return;
                }
            }
        })?;
        self._watcher = Some(watcher);
        Ok(())
    }

    fn cycle(&mut self, mut cs: ChangeSet) {
        self.literal_patch = false;
        self.cycle_saves = self.saves_seen.load(Ordering::SeqCst);
        if !self.carry.is_empty() {
            cs.merge(std::mem::take(&mut self.carry));
        }
        if self.server_exe.is_none() && cs.client {
            cs.server = true;
        }
        self.rev += 1;
        let rev = self.rev;
        let t0 = cs.since.unwrap_or_else(Instant::now);
        if !cs.initial {
            self.saves.insert(rev, t0);
            if self.saves.len() > 32 {
                let oldest = *self.saves.keys().min().unwrap();
                self.saves.remove(&oldest);
            }
        }
        if cs.manual {
            self.log(Level::Info, "watch", "reload asked for");
        }
        if !cs.initial && !cs.files.is_empty() {
            let files: Vec<String> = cs.files.iter().map(|p| p.display().to_string()).collect();
            let shown = files.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
            let more = if files.len() > 4 {
                format!(" and {} more", files.len() - 4)
            } else {
                String::new()
            };
            self.log(Level::Info, "watch", format!("changed {shown}{more}"));
        }

        if cs.restart {
            // Yawn.toml changed
            match config::ProjectConfig::load(&self.project.root) {
                Ok(c) => self.project.config = c.unwrap_or_default(),
                Err(e) => self.log(Level::Error, "dev", e.to_string()),
            }
            self.tailwind = None;
        }
        let mut ok = true;
        let mut errors = Vec::new();
        let mut tailwind_changed = false;
        let tailwind_due =
            self.project.config.tailwind.enabled && (cs.needs_tailwind() || cs.restart);
        let overlap =
            tailwind_due && cs.needs_build() && self.tailwind.is_some() && !self.superseded();
        if tailwind_due && !overlap {
            match self.run_tailwind() {
                Ok(changed) => tailwind_changed = changed,
                Err(e) => {
                    self.log(Level::Error, "tailwind", &e);
                    errors.push(format!("tailwind: {e}"));
                    ok = false;
                }
            }
            if self.stopping() {
                return;
            }
        }
        if self.superseded() && cs.needs_build() {
            return self.carry_over(cs);
        }
        if overlap {
            let tw = self.tailwind.take().expect("checked above");
            self.progress(0, 0, "tailwind", true);
            let (built, build_ok) = std::thread::scope(|scope| {
                let job = scope.spawn(|| tw.build());
                let build_ok = self.build(&cs, &mut errors);
                (job.join().unwrap_or_else(|_| Err("tailwind thread panicked".into())), build_ok)
            });
            self.tailwind = Some(tw);
            ok = build_ok;
            match built {
                Ok(b) => {
                    self.log(
                        Level::Info,
                        "tailwind",
                        format!("css built in {}", fmt_duration(b.duration)),
                    );
                    tailwind_changed = b.changed;
                }
                Err(e) => {
                    self.log(Level::Error, "tailwind", &e);
                    errors.push(format!("tailwind: {e}"));
                    ok = false;
                }
            }
            if self.stopping() {
                return;
            }
            if self.superseded() {
                return self.carry_over(cs);
            }
        } else if ok && cs.needs_build() {
            ok = self.build(&cs, &mut errors);
            if self.stopping() {
                return;
            }
            if self.superseded() {
                return self.carry_over(cs);
            }
        }

        if !ok {
            let text = errors.join("\n");
            self.last_error = Some(text.clone());
            if let Some(l) = &self.loading {
                l.set(|s| s.error = Some(text.clone()));
            }
            self.send_control(&format!("error {rev} {}", yawn::protocol::escape_line(&text)));
            return;
        }
        self.last_error = None;

        let server_changed = cs.server || cs.restart || self.server_rebuilt;
        if server_changed {
            self.stop_server();
        }
        if self.server.is_none() && !self.start_server() {
            return;
        }
        if !server_changed {
            if self.literal_patch && !cs.asset {
                for href in &cs.css {
                    self.send_control(&format!("css {rev} {href}"));
                }
                if self.project.dist.join("pkg/rsx.css").is_file() {
                    self.send_control(&format!("css {rev} /pkg/rsx.css"));
                }
                if tailwind_changed {
                    self.send_control(&format!("css {rev} {}", config::TAILWIND_URL));
                }
                self.send_control(&format!("patch {rev}"));
            } else if cs.needs_reload() {
                for path in &cs.data {
                    self.send_control(&format!(
                        "data {rev} {}",
                        yawn::protocol::escape_line(&path.to_string_lossy())
                    ));
                }
                self.send_control(&format!("reload {rev}"));
            } else {
                for href in &cs.css {
                    self.send_control(&format!("css {rev} {href}"));
                }
                if tailwind_changed {
                    self.send_control(&format!("css {rev} {}", config::TAILWIND_URL));
                }
            }
        }
    }

    fn carry_over(&mut self, cs: ChangeSet) {
        self.log(Level::Debug, "dev", "newer save while building, starting over");
        self.send_control(&format!("progress {} 0 0 ", self.rev));
        self.carry = cs;
    }

    fn run_tailwind(&mut self) -> Result<bool, String> {
        self.progress(0, 0, "tailwind", true);
        if self.tailwind.is_none() {
            let log = |m: &str| {
                self.sink.log(Level::Info, "tailwind", m);
            };
            let prepared = tailwind::Tailwind::prepare(
                &self.project.root,
                &self.project.dist,
                &self.project.config.tailwind,
                &log,
            )?;
            self.tailwind = Some(prepared);
        }
        let built = self.tailwind.as_ref().expect("just set").build()?;
        self.log(Level::Info, "tailwind", format!("css built in {}", fmt_duration(built.duration)));
        self.send_control(&format!("progress {} 0 0 ", self.rev));
        Ok(built.changed)
    }

    fn build(&mut self, cs: &ChangeSet, errors: &mut Vec<String>) -> bool {
        self.build_id += 1;
        let id = self.build_id;
        let target = match (cs.client, cs.server) {
            (true, true) => "client + server",
            (true, false) => "client",
            _ => "server",
        };
        let started = Instant::now();
        let target_owned = target.to_string();
        self.sink.build(|at| BuildEvent::Started { at, id, target: target_owned });
        self.progress(0, 0, "starting", true);

        let mut counts = (0u32, 0u32); // errors, warnings
        let mut ok = true;
        let mut timings = Vec::new();

        let mut rsx_ok = true;
        if cs.client {
            let rsx_t = Instant::now();
            rsx_ok = self.compile_rsx(errors, &mut counts);
            if rsx_ok {
                timings.push(format!("rsx {}", fmt_duration(rsx_t.elapsed())));
            } else {
                ok = false;
            }
        }
        let snapshot =
            (cs.client && rsx_ok).then(|| literals::Snapshot::read(&self.project.root, &mut self.snapshot_cache)).flatten();
        let patch = if !cs.initial
            && !cs.server
            && !cs.restart
            && !cs.files.is_empty()
            && cs.files.iter().all(|p| p.extension().is_some_and(|e| e == "rsx"))
        {
            self.literal_baseline
                .as_ref()
                .zip(snapshot.as_ref())
                .and_then(|(base, next)| base.replacements(next))
        } else {
            None
        };
        let patched = patch
            .as_ref()
            .is_some_and(|values| literals::write(&self.project.dev_dir, values).is_ok());
        self.literal_patch = patched;
        if patched {
            timings.push("literals updated, cargo skipped".into());
        }
        let mut need_server = cs.server;
        if cs.client && rsx_ok && !cs.server && !cs.initial {
            match self
                .server_baseline
                .as_ref()
                .zip(snapshot.as_ref())
                .and_then(|(base, next)| base.server_replacements(next))
            {
                Some(values) => {
                    if let Err(error) = literals::write_server(&self.project.dev_dir, &values) {
                        errors.push(format!("cannot write the server's literals: {error}"));
                        ok = false;
                    }
                }
                None => need_server = true,
            }
        }
        self.server_rebuilt = need_server && !cs.server;
        if (cs.initial || cs.server || cs.restart)
            && let Some(d) = self.dynamic.as_mut()
        {
            d.reset_exports();
        }
        let mut sided = false;
        let mut early: Option<CargoDone> = None;
        if cs.client && rsx_ok && !patched && !cs.initial && !cs.server && !cs.restart {
            if let Some(mut d) = self.dynamic.take() {
                match d.plan() {
                    dynamic::Plan::Nothing => {}
                    dynamic::Plan::Full(why) => self.log(Level::Debug, "client", format!("full build: {why}")),
                    dynamic::Plan::Leaves(leaves) => {
                        let t = Instant::now();
                        self.progress(0, 0, "side module", true);
                        let kill = self.build_kill.clone();
                        kill.clear();
                        let recs: Vec<Option<replay::Recorded>> = leaves
                            .iter()
                            .map(|leaf| {
                                let krate = leaf.package.replace('-', "_");
                                self.lanes[0]
                                    .as_ref()
                                    .and_then(|l| l.lock().unwrap_or_else(|e| e.into_inner()).recorded(&krate))
                            })
                            .collect();
                        let dist = self.project.dist.clone();
                        let blocked = leaves.iter().any(|l| d.native_blocked(&l.dir));
                        let side = |d: &mut dynamic::Dynamic| {
                            let mut out = Vec::new();
                            for (leaf, rec) in leaves.iter().zip(&recs) {
                                let mut seen = Vec::new();
                                let r = match rec {
                                    Some(rec) => d.build_side(leaf, rec, &dist, &kill, &mut |p| seen.push(p.clone())),
                                    None => Err(dynamic::SideError::Fallback("no recorded rustc command".into())),
                                };
                                let stop = r.is_err();
                                out.push((seen, r));
                                if stop {
                                    break;
                                }
                            }
                            out
                        };
                        let results = if need_server && !cs.server && blocked {
                            self.log(Level::Debug, "server", "full build: the crate has mutable statics");
                            let (results, done) = std::thread::scope(|scope| {
                                let job = scope.spawn(|| side(&mut d));
                                let done = self.run_cargo(false, true, errors, &mut counts);
                                (job.join().unwrap_or_default(), done)
                            });
                            early = Some(done);
                            results
                        } else {
                            side(&mut d)
                        };
                        let mut all = true;
                        for (seen, r) in results {
                            for p in &seen {
                                self.report(p, errors, &mut counts, "client");
                            }
                            match r {
                                Ok(()) => {}
                                Err(dynamic::SideError::Failed) => {
                                    ok = false;
                                    sided = true;
                                    all = false;
                                }
                                Err(dynamic::SideError::Fallback(why)) => {
                                    self.log(Level::Debug, "client", format!("full build: {why}"));
                                    all = false;
                                }
                            }
                        }
                        if all {
                            sided = true;
                            timings.push(format!("side module {}", fmt_duration(t.elapsed())));
                            if need_server && !cs.server && !blocked {
                                let t = Instant::now();
                                let mut done = true;
                                for leaf in &leaves {
                                    let args = self.cargo_args(
                                        &[
                                            "build",
                                            "-p",
                                            leaf.package.as_str(),
                                            "--lib",
                                            "--config",
                                            "profile.dev.opt-level=0",
                                        ],
                                        Some(SERVER_TARGET_DIR),
                                    );
                                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                                    let env = self.lanes[1]
                                        .as_ref()
                                        .map(|l| l.lock().unwrap_or_else(|e| e.into_inner()).env())
                                        .unwrap_or_default();
                                    let mut seen = Vec::new();
                                    let r = d.build_native(
                                        leaf,
                                        &args,
                                        &env,
                                        &self.project.dev_dir,
                                        &kill,
                                        &mut |p| seen.push(p.clone()),
                                    );
                                    for p in &seen {
                                        self.report(p, errors, &mut counts, "server");
                                    }
                                    match r {
                                        Ok(()) => {}
                                        Err(dynamic::SideError::Failed) => {
                                            ok = false;
                                            done = false;
                                            need_server = false;
                                            break;
                                        }
                                        Err(dynamic::SideError::Fallback(why)) => {
                                            self.log(
                                                Level::Debug,
                                                "server",
                                                format!("full build: {why}"),
                                            );
                                            done = false;
                                            break;
                                        }
                                    }
                                }
                                if done {
                                    need_server = false;
                                    self.server_rebuilt = false;
                                    timings.push(format!("server module {}", fmt_duration(t.elapsed())));
                                    if let Some(next) = snapshot.clone() {
                                        let _ = literals::write_server(
                                            &self.project.dev_dir,
                                            &Default::default(),
                                        );
                                        self.server_baseline = Some(next);
                                    }
                                }
                            }
                            if let Some(next) = snapshot.clone() {
                                let _ = literals::write(&self.project.dev_dir, &Default::default());
                                self.literal_baseline = Some(next);
                            }
                        }
                    }
                }
                self.dynamic = Some(d);
            }
        }
        let client = cs.client && rsx_ok && !patched && !sided;
        let server = need_server && !self.stopping();
        if client || server {
            self.progress(0, 0, "cargo", true);
            let mark = errors.len();
            let mut done = match early.take() {
                Some(done) if !client => done,
                _ => self.run_cargo(client, server, errors, &mut counts),
            };
            if self.dropped_dynamic {
                self.dropped_dynamic = false;
                errors.truncate(mark);
                let again = self.run_cargo(true, false, errors, &mut counts);
                done.ok = again.ok;
                done.client = again.client;
            }
            ok &= done.ok;
            if let Some((client_ok, cargo_t, bindgen_t)) = done.client {
                timings.push(format!("client cargo {}", fmt_duration(cargo_t)));
                if let Some(t) = bindgen_t {
                    timings.push(format!("wasm-bindgen {}", fmt_duration(t)));
                }
                let chip = if client_ok {
                    format!("✓ {}", fmt_duration(cargo_t + bindgen_t.unwrap_or_default()))
                } else {
                    "✗ failed".to_string()
                };
                self.sink.info("client", chip);
            }
            if let Some(t) = done.server {
                timings.push(format!("server cargo {}", fmt_duration(t)));
            }
        }
        if ok && server {
            if let Some(d) = self.dynamic.as_mut() {
                d.reset_native(&self.project.dev_dir);
            }
            if let Err(error) = literals::write_server(&self.project.dev_dir, &Default::default()) {
                errors.push(format!("cannot reset the server's literals: {error}"));
                ok = false;
            } else if snapshot.is_some() {
                self.server_baseline = snapshot.clone();
            }
        }
        if ok && client {
            if let Err(error) = literals::write(&self.project.dev_dir, &Default::default()) {
                errors.push(format!("cannot reset dev literals: {error}"));
                ok = false;
            } else {
                self.literal_baseline = snapshot;
            }
        }
        let duration = started.elapsed();
        if self.superseded() {
            return false;
        }
        self.sink.build(|at| BuildEvent::Finished {
            at,
            id,
            target: target.to_string(),
            ok,
            duration,
            warnings: counts.1,
            errors: counts.0,
        });
        // the browser's progress bar is over
        self.send_control(&format!("progress {} 0 0 ", self.rev));
        if ok {
            self.log(
                Level::Info,
                "build",
                format!("{target} built in {} ({})", fmt_duration(duration), timings.join(", ")),
            );
        }
        ok
    }

    fn compile_rsx(&mut self, errors: &mut Vec<String>, counts: &mut (u32, u32)) -> bool {
        match rsx::compile(&self.project.root) {
            Ok(report) => {
                if report.written > 0 {
                    self.log(
                        Level::Debug,
                        "rsx",
                        format!(
                            "compiled {} file(s), {} written",
                            report.sources.len(),
                            report.written
                        ),
                    );
                }
                true
            }
            Err(diags) => {
                for d in &diags {
                    let l = rsx::locate(d);
                    let ev = LogEvent::new(self.sink.elapsed(), Level::Error, "rsx", &l.message)
                        .at_location(Location::new(&l.file, l.line, l.col));
                    self.sink.send(Event::Log(ev));
                    counts.0 += 1;
                    if errors.len() < 6 {
                        errors.push(l.rendered);
                    }
                }
                false
            }
        }
    }

    fn cargo_args(&self, args: &[&str], target_dir: Option<&str>) -> Vec<String> {
        let mut v: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        if self.project.root.join(yawn_rsx::manifest::MANIFEST).is_file() {
            v.extend(["--manifest-path".into(), yawn_rsx::manifest::MANIFEST.into()]);
            v.extend(["--target-dir".into(), target_dir.unwrap_or("target").into()]);
        } else if let Some(dir) = target_dir {
            v.extend(["--target-dir".into(), dir.into()]);
        }
        v
    }

    fn report(
        &self,
        p: &Parsed,
        errors: &mut Vec<String>,
        counts: &mut (u32, u32),
        label: &'static str,
    ) {
        match p {
            Parsed::Diagnostic(d) => {
                let mut ev = LogEvent::new(self.sink.elapsed(), d.level, label, &d.message);
                // Show `.rsx` errors at their location.
                let location = d.location.as_ref().map(|l| {
                    match rsx::remap(&self.project.root, &l.file, l.line, l.col) {
                        Some((file, line, col)) => Location::new(file, line, col),
                        None => l.clone(),
                    }
                });
                if let Some(l) = &location {
                    ev = ev.at_location(l.clone());
                }
                if let Some(c) = &d.code {
                    ev = ev.with_code(c);
                }
                if d.level == Level::Error {
                    counts.0 += 1;
                    if errors.len() < 6 {
                        let loc = location.as_ref().map(|l| format!("{l} ")).unwrap_or_default();
                        let code = d.code.as_ref().map(|c| format!("[{c}] ")).unwrap_or_default();
                        errors.push(format!("{loc}{code}{}", d.message));
                    }
                } else {
                    counts.1 += 1;
                }
                self.sink.send(Event::Log(ev));
            }
            Parsed::Compiling(name) => {
                self.log(Level::Debug, "cargo", format!("compiling {name}"));
            }
            Parsed::Progress { done, total, running } => {
                let names = running.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                let more = running.len().saturating_sub(4);
                let label = if more > 0 {
                    format!("{label}: {names} +{more}")
                } else {
                    format!("{label}: {names}")
                };
                self.progress(*done, *total, label.trim_end_matches(": "), false);
            }
            Parsed::Other(line) if line.contains("Blocking") => {
                self.log(Level::Debug, "cargo", line);
            }
            Parsed::Other(_) | Parsed::Artifact(_) => {}
        }
    }

    fn run_cargo(
        &mut self,
        client: bool,
        server: bool,
        errors: &mut Vec<String>,
        counts: &mut (u32, u32),
    ) -> CargoDone {
        let root = self.project.root.clone();
        let package = self.project.package.clone();
        // `cargo rustc` so `Cargo.toml` needs no `crate-type = ["cdylib"]`
        let mut client_args = self.cargo_args(
            &["rustc", "--lib", "--target", WASM_TARGET, "--crate-type", "cdylib"],
            Some(CLIENT_TARGET_DIR),
        );
        // The app's crates follow the project's `[profile.dev]`: a size-tuned profile makes the
        // module wasm-bindgen reads much smaller, and a plain one stays unoptimized
        // The dev server binary is never shipped, so skip the project's size-tuned `[profile.dev]`
        let mut server_args = self.cargo_args(
            &["rustc", "--bin", &package, "--config", "profile.dev.opt-level=0"],
            Some(SERVER_TARGET_DIR),
        );
        if let Some(d) = &self.dynamic
            && let Some(arg) = d.server_link_arg()
        {
            server_args.extend(["--".into(), arg]);
        }
        // The name section is over half the module and wasm-bindgen would drop it anyway
        let split = self.split_wasm();
        if split {
            client_args.extend(["--".into(), "-Clink-arg=--emit-relocs".into()]);
        } else if std::env::var_os("YAWN_WASM_NAMES").is_none() {
            client_args.extend(["--".into(), "-Clink-arg=--strip-all".into()]);
        }
        if let Some(d) = &self.dynamic {
            if !client_args.iter().any(|a| a == "--") {
                client_args.push("--".into());
            }
            client_args.push(d.link_arg());
        }
        let pic_env: Vec<(String, String)> = Vec::new();
        let server_env: Vec<(String, String)> = Vec::new();
        let kill = self.build_kill.clone();
        kill.clear();
        let (tx, rx) = mpsc::channel::<CargoMsg>();
        let mut done = CargoDone { ok: true, client: None, server: None };
        let mut progress: HashMap<&'static str, (u32, u32)> = HashMap::new();
        std::thread::scope(|scope| {
            let spawn = |label: &'static str, args: Vec<String>, lane: Option<Arc<Mutex<replay::Lane>>>, extra: Vec<(String, String)>| {
                let (tx, root, kill, package) =
                    (tx.clone(), root.clone(), kill.clone(), package.clone());
                scope.spawn(move || {
                    let args: Vec<&str> = args.iter().map(String::as_str).collect();
                    let (mut wasm, mut exe) = (None, None);
                    let mut handle = |p: &Parsed| {
                        if let Parsed::Artifact(a) = p {
                            if label == "client" {
                                if let Some(f) = a
                                    .files
                                    .iter()
                                    .find(|f| f.extension().is_some_and(|e| e == "wasm"))
                                {
                                    wasm = Some((f.clone(), a.fresh));
                                }
                            } else if a.name == package {
                                exe = a.executable.clone().or(exe.take());
                            }
                            if let Some(l) = &lane
                                && let Ok(mut l) = l.try_lock()
                            {
                                l.note(a);
                            }
                        }
                        let _ = tx.send(CargoMsg::Event(label, p.clone()));
                    };
                    let replayed = lane.as_ref().and_then(|l| {
                        l.lock().unwrap_or_else(|e| e.into_inner()).try_run(&kill, &mut handle)
                    });
                    let run = match replayed {
                        Some(run) => Ok(run),
                        None => {
                            let mut env = lane
                                .as_ref()
                                .map(|l| {
                                    let mut l = l.lock().unwrap_or_else(|e| e.into_inner());
                                    l.begin();
                                    l.env()
                                })
                                .unwrap_or_default();
                            env.extend(extra);
                            let run = cargo::run_with(&args, &root, &kill, &env, &mut handle);
                            if let (Some(l), Ok(r)) = (&lane, &run)
                                && r.ok
                            {
                                l.lock().unwrap_or_else(|e| e.into_inner()).complete();
                            }
                            run
                        }
                    };
                    let _ = tx.send(CargoMsg::Done(label, Box::new(Finished { run, wasm, exe })));
                });
            };
            if client {
                spawn("client", client_args, self.lanes[0].clone(), pic_env);
            }
            if server {
                spawn("server", server_args, self.lanes[1].clone(), server_env);
            }
            drop(tx);
            let mut cut = false;
            loop {
                // a newer save makes this build pointless: stop cargo, the next cycle starts at once
                if !cut && self.superseded() {
                    cut = true;
                    kill.kill();
                }
                let msg = match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(m) => m,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match msg {
                    // two builds report their own `[n/m]`; the browser gets the sum
                    CargoMsg::Event(label, Parsed::Progress { done: d, total, running }) => {
                        progress.insert(label, (d, total));
                        let build = self.build_id;
                        self.sink.build(|at| BuildEvent::Lane {
                            at,
                            id: build,
                            lane: label.to_string(),
                            done: d,
                            total,
                        });
                        let (d, total) =
                            progress.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
                        let names = running.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                        let more = running.len().saturating_sub(4);
                        let text = if more > 0 {
                            format!("{label}: {names} +{more}")
                        } else {
                            format!("{label}: {names}")
                        };
                        self.progress(d, total, text.trim_end_matches(": "), false);
                    }
                    CargoMsg::Event(label, p) => self.report(&p, errors, counts, label),
                    CargoMsg::Done(label, result) => {
                        let Finished { run, wasm, exe } = *result;
                        if label == "client" {
                            let r = self.finish_client(run, wasm, errors, counts);
                            done.ok &= r.0;
                            done.client = Some(r);
                        } else {
                            let r = self.finish_server(run, exe, errors, counts);
                            done.ok &= r.0;
                            done.server = Some(r.1);
                        }
                    }
                }
            }
        });
        yawn_rsx::manifest::sync_lock_back(&root);
        done
    }

    fn finish_client(
        &mut self,
        run: std::io::Result<cargo::CargoRun>,
        wasm: Option<(PathBuf, bool)>,
        errors: &mut Vec<String>,
        counts: &mut (u32, u32),
    ) -> (bool, Duration, Option<Duration>) {
        let run = match run {
            Ok(r) => r,
            Err(e) => {
                errors.push(e.to_string());
                self.log(Level::Error, "client", e.to_string());
                return (false, Duration::ZERO, None);
            }
        };
        if self.stopping() || self.superseded() {
            return (false, run.duration, None);
        }
        if !run.ok && run.errors == 0 && self.dynamic.is_some() {
            let why = run.other.iter().rev().find(|l| !l.starts_with("warning")).cloned();
            self.log(
                Level::Info,
                "client",
                format!(
                    "route hot-swap off: the position-independent build failed ({})",
                    why.unwrap_or_default()
                ),
            );
            self.dynamic = None;
            self.dropped_dynamic = true;
            return (false, run.duration, None);
        }
        if !run.ok {
            if run.errors == 0 {
                for l in run.other.iter().filter(|l| !l.starts_with("warning")).take(6) {
                    errors.push(l.clone());
                    self.log(Level::Error, "client", l);
                    counts.0 += 1;
                }
                if run.other.iter().any(|l| {
                    l.contains("wasm32-unknown-unknown") && l.contains("not installed")
                        || l.contains("can't find crate for `core`")
                }) {
                    self.log(
                        Level::Info,
                        "client",
                        format!("hint: rustup target add {WASM_TARGET}"),
                    );
                }
            }
            return (false, run.duration, None);
        }
        let Some((wasm_path, _)) = wasm else {
            let msg = "cargo built no wasm module: the project needs a library (src/lib.rs, or src/main.rsx)";
            errors.push(msg.into());
            self.log(Level::Error, "client", msg);
            counts.0 += 1;
            return (false, run.duration, None);
        };

        let out = self.project.dist.join("pkg");
        let mut relinked = false;
        if let Some(mut d) = self.dynamic.take() {
            let kill = self.build_kill.clone();
            let result = match self.lanes[0].as_ref() {
                Some(lane) => {
                    let mut lane = lane.lock().unwrap_or_else(|e| e.into_inner());
                    let root = lane.recorded_root();
                    d.refresh_exports(
                        &mut |krate| lane.recorded(krate).map(|r| r.out_dir),
                        root,
                        &wasm_path,
                        &kill,
                    )
                }
                None => Ok(false),
            };
            match result {
                Ok(r) => relinked = r,
                Err(e) => self.log(Level::Debug, "client", format!("exports: {e}")),
            }
            self.dynamic = Some(d);
        }
        // `fresh` isn't enough: another cargo may have built the wasm just before this run
        if !relinked && bundle_is_current(&wasm_path, &out.join(format!("{CLIENT_BUNDLE}_bg.wasm"))) {
            self.log(Level::Debug, "client", "wasm unchanged, skipping wasm-bindgen");
            if let Some(mut d) = self.dynamic.take() {
                if !out.join("dyn").join("manifest.json").is_file() {
                    let _ = d.reset(&self.project.dist);
                }
                dynamic::patch_glue(&out);
                d.remember();
                self.dynamic = Some(d);
            }
            return (true, run.duration, None);
        }
        self.progress(0, 0, "client: wasm-bindgen", true);
        let t = Instant::now();
        let result = self.bindgen(&wasm_path, &out);
        match result {
            Ok(()) => {
                if let Some(mut d) = self.dynamic.take() {
                    if !dynamic::patch_glue(&out) {
                        self.log(
                            Level::Info,
                            "client",
                            "route hot-swap off: the wasm-bindgen glue has an unknown shape",
                        );
                    } else {
                        if let Err(e) = d.reset(&self.project.dist) {
                            self.log(Level::Warn, "client", format!("dynamic modules: {e}"));
                        }
                        d.remember();
                        self.dynamic = Some(d);
                    }
                }
                if self.split_wasm() {
                    let log = |m: &str| {
                        self.sink.log(Level::Info, "split", m);
                    };
                    crate::split::apply(&out, &wasm_path, &log, true);
                }
                (true, run.duration, Some(t.elapsed()))
            }
            Err(msg) => {
                errors.push(msg.clone());
                self.log(Level::Error, "wasm-bindgen", msg);
                counts.0 += 1;
                (false, run.duration, Some(t.elapsed()))
            }
        }
    }

    /// Whether the client module is cut into a main module and route modules, as `yawn build` does
    fn split_wasm(&self) -> bool {
        std::env::var_os("YAWN_DEV_SPLIT").is_some_and(|v| v != "0")
    }

    fn bindgen(&mut self, wasm: &std::path::Path, out: &std::path::Path) -> Result<(), String> {
        if self.bindgen.is_none() {
            let log = |m: &str| {
                self.sink.log(Level::Info, "wasm-bindgen", m);
            };
            self.bindgen = Some(wasm_bindgen::tool(&self.project.root, &log)?);
        }
        std::fs::create_dir_all(out)
            .map_err(|e| format!("cannot create {}: {e}", out.display()))?;
        let options = wasm_bindgen::Options {
            out_name: CLIENT_BUNDLE,
            demangle: !self.split_wasm(),
            // YAWN_WASM_NAMES=1 keeps the name section
            remove_names: !self.split_wasm() && std::env::var_os("YAWN_WASM_NAMES").is_none(),
            keep_linker_exports: self.dynamic.is_some(),
        };
        wasm_bindgen::run(self.bindgen.as_ref().expect("just set"), wasm, out, &options)
    }

    fn finish_server(
        &mut self,
        run: std::io::Result<cargo::CargoRun>,
        exe: Option<PathBuf>,
        errors: &mut Vec<String>,
        counts: &mut (u32, u32),
    ) -> (bool, Duration) {
        let package = self.project.package.clone();
        let run = match run {
            Ok(r) => r,
            Err(e) => {
                errors.push(e.to_string());
                self.log(Level::Error, "server", e.to_string());
                return (false, Duration::ZERO);
            }
        };
        if self.superseded() {
            return (false, run.duration);
        }
        if !run.ok || self.stopping() {
            if !run.ok && run.errors == 0 {
                for l in run.other.iter().filter(|l| !l.starts_with("warning")).take(6) {
                    errors.push(l.clone());
                    self.log(Level::Error, "server", l);
                    counts.0 += 1;
                }
            }
            return (false, run.duration);
        }
        match exe {
            Some(e) => {
                if let Some(mut d) = self.dynamic.take() {
                    if let Some(lane) = self.lanes[1].as_ref() {
                        let mut lane = lane.lock().unwrap_or_else(|x| x.into_inner());
                        let bin = e
                            .file_stem()
                            .and_then(|n| n.to_str())
                            .and_then(|n| lane.recorded_bin(&n.replace('-', "_")));
                        let kill = self.build_kill.clone();
                        let r = d.refresh_server_exports(
                            &mut |krate| lane.recorded(krate).map(|r| r.out_dir),
                            bin,
                            &e,
                            &kill,
                        );
                        match r {
                            Ok(relinked) => self.log(
                                Level::Debug,
                                "server",
                                format!("exports: relinked={relinked}"),
                            ),
                            Err(err) => self.log(Level::Debug, "server", format!("exports: {err}")),
                        }
                    }
                    self.dynamic = Some(d);
                }
                self.server_exe = Some(e);
                (true, run.duration)
            }
            None => {
                let msg = format!(
                    "cargo built no binary named `{package}` (the server is src/main.rs, or generated from src/main.rsx)"
                );
                errors.push(msg.clone());
                self.log(Level::Error, "server", msg);
                counts.0 += 1;
                (false, run.duration)
            }
        }
    }

    fn start_server(&mut self) -> bool {
        let Some(exe) = self.server_exe.clone() else {
            self.log(Level::Error, "server", "no server binary built yet");
            self.sink.info("server", "not built");
            return false;
        };
        // the app's server takes over the port; the progress page reloads itself onto it
        if let Some(l) = self.loading.take() {
            l.stop();
        }
        self.r#gen += 1;
        let r#gen = self.r#gen;
        let mut cmd = Command::new(&exe);
        cmd.current_dir(&self.project.root)
            .env("YAWN_DEV", "1")
            .env("YAWN_DIST", &self.project.dist)
            .env("YAWN_DEV_DIR", &self.project.dev_dir)
            .env("YAWN_REV", self.rev.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(p) = self.port {
            cmd.env("YAWN_PORT", p.to_string());
        }
        cargo::own_group(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.log(Level::Error, "server", format!("cannot start {}: {e}", exe.display()));
                self.sink.info("server", "failed to start");
                return false;
            }
        };
        self.server_kill.set(child.id());
        self.sink.info("server", "starting");
        let stdin = child.stdin.take();
        let (out, err) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        for (stream, is_err) in
            [(Box::new(out) as Box<dyn std::io::Read + Send>, false), (Box::new(err), true)]
        {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(Cmd::Server { r#gen, line, err: is_err }).is_err() {
                        break;
                    }
                }
            });
        }
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let (done2, tx) = (done.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let status = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            *done2.0.lock().unwrap() = true;
            done2.1.notify_all();
            let _ = tx.send(Cmd::ServerExit { r#gen, status });
        });
        self.server = Some(ServerProc { stdin: std::cell::RefCell::new(stdin), done });
        true
    }

    fn stop_server(&mut self) {
        let Some(sp) = self.server.take() else { return };
        self.r#gen += 1; // its exit message is no longer interesting
        drop(sp.stdin.borrow_mut().take());
        let wait = |d: Duration| {
            let g = sp.done.0.lock().unwrap();
            *sp.done.1.wait_timeout_while(g, d, |done| !*done).unwrap().0
        };
        if !wait(Duration::from_millis(1500)) {
            self.server_kill.kill();
            wait(Duration::from_secs(1));
        }
        self.server_kill.clear();
    }

    fn send_control(&self, line: &str) {
        if let Some(sp) = &self.server
            && let Some(stdin) = sp.stdin.borrow_mut().as_mut()
        {
            let _ = writeln!(stdin, "{line}").and_then(|_| stdin.flush());
        }
    }

    fn progress(&self, done: u32, total: u32, label: &str, force: bool) {
        let id = self.build_id;
        if let Some(l) = &self.loading {
            l.set(|s| {
                s.done = done;
                s.total = total;
                s.label = label.to_string();
                s.error = None;
            });
        }
        self.sink.build(|at| BuildEvent::Progress {
            at,
            id,
            done,
            total,
            label: label.to_string(),
        });
        let now = Instant::now();
        let due = self
            .last_progress
            .get()
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(80));
        if due || force {
            self.last_progress.set(Some(now));
            self.send_control(&format!(
                "progress {} {done} {total} {}",
                self.rev,
                yawn::protocol::escape_line(label)
            ));
        }
    }

    fn server_line(&mut self, line: &str, err: bool) {
        if let Some(rest) = line.strip_prefix("@yawn ") {
            let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
            match kind {
                "ready" => {
                    self.sink.info("server", arg);
                    self.log(Level::Info, "server", format!("listening on {arg}"));
                    if let Some(e) = self.last_error.clone() {
                        let rev = self.rev;
                        self.send_control(&format!(
                            "error {rev} {}",
                            yawn::protocol::escape_line(&e)
                        ));
                    }
                }
                "loaded" => {
                    let mut it = arg.split_whitespace();
                    let rev: Option<u64> = it.next().and_then(|r| r.parse().ok());
                    let kind = it.next().unwrap_or("reload");
                    if kind == "css" && self.literal_patch {
                        return;
                    }
                    if let Some(t) = rev.and_then(|r| self.saves.remove(&r)) {
                        let ms = t.elapsed();
                        let what = match kind {
                            "css" => "styles updated",
                            "patch" => "view updated",
                            "client" => "client replaced",
                            _ => "page reloaded",
                        };
                        self.log(
                            Level::Info,
                            "browser",
                            format!("{what} {} after save", fmt_duration(ms)),
                        );
                        self.sink.info("refresh", fmt_duration(ms));
                    }
                }
                "req" => {
                    let mut it = arg.splitn(4, ' ');
                    let status: u16 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let level = if status >= 500 {
                        Level::Error
                    } else if status >= 400 {
                        Level::Warn
                    } else {
                        Level::Debug
                    };
                    let (ms, method, path) =
                        (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
                    self.log(level, "server", format!("{method} {path} {status} {ms}"));
                }
                _ => self.log(Level::Debug, "server", line),
            }
            return;
        }
        let level = if line.contains("panicked")
            || line.starts_with("error")
            || line.starts_with("Error")
        {
            Level::Error
        } else if err {
            Level::Warn
        } else {
            Level::Info
        };
        self.log(level, "server", line);
    }
}
