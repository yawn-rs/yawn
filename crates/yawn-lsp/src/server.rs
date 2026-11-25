use crate::complete;
use crate::context::{Ctx, context};
use crate::index::Index;
use crate::mask;
use crate::position::{Text, gen_to_src, src_to_gen};
use crate::trace::{self, trace};
use crate::web::Web;
use crossbeam_channel::{Receiver, select, unbounded};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use yawn_rsx::{SourceMap, project};

mod ops;

type Res<T> = Result<T, Box<dyn Error + Send + Sync>>;

struct Gen {
    uri: Url,
    text: Text,
    map: SourceMap,
    version: i32,
    opened: bool,
}

fn install_gen(
    doc: &mut Doc,
    rel: &str,
    rust: String,
    map: SourceMap,
) -> Option<(Url, i32, String, bool)> {
    let root = doc.root.as_ref()?;
    let stem = project::generated_stem(
        root,
        rel.strip_prefix("src/").unwrap_or(rel).trim_end_matches(".rsx"),
    );
    let is_entry = rel == format!("src/{}", project::ENTRY);
    let gen_path = if is_entry {
        root.join(project::CLIENT_ROOT)
    } else {
        root.join(project::OUT_DIR).join(format!("{stem}.rs"))
    };
    let rust = if is_entry { project::client_root(root, &rust) } else { rust };
    let guri = Url::from_file_path(&gen_path).ok()?;
    let g = doc.r#gen.get_or_insert_with(|| Gen {
        uri: guri.clone(),
        text: Text::new(String::new()),
        map: SourceMap::default(),
        version: 0,
        opened: false,
    });
    g.version += 1;
    g.text = Text::new(rust.clone());
    g.map = map;
    let out = (g.uri.clone(), g.version, rust, g.opened);
    g.opened = true;
    Some(out)
}

fn recover_empty_expressions(
    src: &str,
    rel: &str,
    text: &Text,
    first: yawn_rsx::diag::Diagnostic,
) -> Option<(Vec<Diagnostic>, yawn_rsx::Compiled)> {
    const FILL: &str = "{todo!()}";
    let mut patched = src.to_string();
    // how much longer the patched text is before a given position: (position in patched, delta)
    let mut shifts: Vec<(usize, usize)> = Vec::new();
    let mut diags = Vec::new();
    let mut err = first;
    for _ in 0..8 {
        if err.message != "empty expression" {
            return None;
        }
        let delta_before: usize = shifts.iter().map(|(_, d)| d).sum();
        let (a, b) = (err.span.start, err.span.end);
        let orig = a.checked_sub(delta_before)?..b.checked_sub(delta_before)?;
        diags.push(lsp_diag(&yawn_rsx::diag::Diagnostic { span: orig, ..err.clone() }, text));
        let added = FILL.len().checked_sub(b - a)?;
        patched.replace_range(a..b, FILL);
        shifts.push((a, added));
        match yawn_rsx::compile(&patched, rel) {
            Ok(c) => return Some((diags, c)),
            Err(e) => err = e,
        }
    }
    None
}

struct Doc {
    text: Text,
    root: Option<PathBuf>,
    r#gen: Option<Gen>,
    own: Vec<Diagnostic>,
    ra: Vec<Diagnostic>,
    ra_raw: Vec<Diagnostic>,
    css: Vec<Diagnostic>,
}

enum Pending {
    Init,
    /// Code actions of rust-analyzer plus ours
    CodeAction(Url, Vec<Value>),
    ActionResolve,
    InlayHints(Url),
    Rename,
    PrepareRename(Url),
    Tokens(Url),
    /// Locations for a `.rs` document: only those inside generated modules are kept, mapped to
    /// `.rsx`
    RsLocations,
    Completion(Url),
    /// Completion right after `.` or `::` in code that doesn't compile yet: asked on a copy with a
    /// placeholder name there, so the edits come back as plain inserts
    CompletionAfterDot(Url),
    /// Props of a component from another crate: rust-analyzer's struct-field completion, asked at a
    /// placeholder attribute in a copy of the generated code, turned into prop items for
    /// `start..offset`
    Props {
        uri: Url,
        start: usize,
        offset: usize,
    },
    Hover(Url),
    /// rust-analyzer's status for a `.rs` file of the project, written to the log (not sent to the
    /// editor)
    Status,
    /// `completionItem/resolve`: auto-import edits arrive here, in generated-file coordinates
    Resolve(Url),
    Locations,
}

struct Server {
    client: Connection,
    ra_in: Option<std::process::ChildStdin>,
    ra_rx: Receiver<Message>,
    docs: HashMap<Url, Doc>,
    gen_to_doc: HashMap<Url, Url>,
    pending: HashMap<RequestId, Pending>,
    status_asked: u32,
    index: Index,
    compiled_roots: HashSet<PathBuf>,
    web: Option<Web>,
    web_failed: bool,
}

pub fn run() -> Res<()> {
    trace::init();
    let (client, io) = Connection::stdio();
    let mut s = Server {
        client,
        ra_in: None,
        ra_rx: crossbeam_channel::never(),
        docs: HashMap::new(),
        gen_to_doc: HashMap::new(),
        pending: HashMap::new(),
        status_asked: 0,
        index: Index::default(),
        compiled_roots: HashSet::new(),
        web: None,
        web_failed: false,
    };
    let rx = s.client.receiver.clone();
    loop {
        let ra_rx = s.ra_rx.clone();
        select! {
            recv(rx) -> m => match m {
                Ok(Message::Notification(n)) if n.method == "exit" => { s.send_ra(Message::Notification(n)); break; }
                Ok(m) => s.handle_client(m)?,
                Err(_) => break,
            },
            recv(ra_rx) -> m => match m {
                Ok(m) => s.handle_ra(m)?,
                Err(_) => { s.ra_in = None; s.ra_rx = crossbeam_channel::never(); eprintln!("yawn-lsp: rust-analyzer stopped; Rust features are off"); }
            },
        }
    }
    drop(s);
    io.join()?;
    Ok(())
}

fn find_rust_analyzer() -> String {
    if let Ok(p) = std::env::var("YAWN_RUST_ANALYZER") {
        return p;
    }
    let works = |bin: &str| {
        Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if works("rust-analyzer") {
        return "rust-analyzer".into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let zed = Path::new(&home).join(".local/share/zed/languages/rust-analyzer");
    let mut found: Vec<PathBuf> =
        std::fs::read_dir(zed).into_iter().flatten().flatten().map(|e| e.path()).collect();
    found.sort();
    for p in found.into_iter().rev() {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if p.is_file() && !name.ends_with(".metadata") && works(&p.to_string_lossy()) {
            return p.to_string_lossy().into_owned();
        }
    }
    "rust-analyzer".into()
}

fn find_root(file: &Path) -> Option<PathBuf> {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d.join("Yawn.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

fn find_projects(dir: &Path, depth: usize, out: &mut Vec<Value>) {
    if dir.join("Yawn.toml").is_file() && dir.join("Cargo.toml").is_file() {
        // A `main.rsx` project builds from the generated manifest in `.yawn/`: compile first so
        // its crate roots exist before rust-analyzer loads it
        if dir.join("src/main.rsx").is_file() {
            let _ = yawn_rsx::project::compile_project(dir);
            if !dir.join(yawn_rsx::manifest::MANIFEST).is_file() {
                let _ = yawn_rsx::manifest::write(dir, yawn_rsx::project::uses_routes(dir), &[]);
            }
        }
        if dir.join(yawn_rsx::manifest::MANIFEST).is_file() && dir.join("src/main.rsx").is_file() {
            out.push(json!(dir.join(yawn_rsx::manifest::MANIFEST)));
        } else {
            out.push(json!(dir.join("Cargo.toml")));
        }
    }
    if depth >= 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || matches!(name.as_str(), "target" | "node_modules") {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            find_projects(&e.path(), depth + 1, out);
        }
    }
}

fn is_rsx(uri: &Url) -> bool {
    uri.path().ends_with(".rsx")
}

fn is_project_rust(uri: &Url) -> bool {
    uri.path().ends_with(".rs")
        && uri
            .to_file_path()
            .ok()
            .and_then(|p| find_root(&p))
            .is_some_and(|root| root.join("src/main.rsx").is_file())
}

fn lsp_diag(d: &yawn_rsx::Diagnostic, text: &Text) -> Diagnostic {
    let mut message = d.message.clone();
    if let Some(l) = &d.label {
        message.push_str(&format!(" ({l})"));
    }
    if let Some(h) = &d.help {
        message.push_str(&format!("\nhelp: {h}"));
    }
    let end =
        if d.span.end > d.span.start { d.span.end } else { (d.span.start + 1).min(text.s.len()) };
    Diagnostic {
        range: Range::new(text.position(d.span.start), text.position(end)),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("yawn-rsx".into()),
        message,
        ..Default::default()
    }
}

impl Server {
    fn spawn_ra(&mut self) {
        if self.ra_in.is_some() {
            return;
        }
        let ra_bin = find_rust_analyzer();
        match Command::new(&ra_bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(mut child) => {
                self.ra_in = child.stdin.take();
                let mut out = BufReader::new(child.stdout.take().unwrap());
                let (tx, rx) = unbounded();
                std::thread::spawn(move || {
                    while let Ok(Some(m)) = Message::read(&mut out) {
                        if tx.send(m).is_err() {
                            break;
                        }
                    }
                });
                self.ra_rx = rx;
                trace!("rust-analyzer started: {ra_bin}");
            }
            Err(e) => eprintln!(
                "yawn-lsp: cannot start `{ra_bin}` ({e}); Rust features are off, HTML and CSS work"
            ),
        }
    }

    fn send_ra(&mut self, m: Message) {
        if let Some(w) = &mut self.ra_in {
            let _ = m.write(w);
            let _ = w.flush();
        }
    }

    fn to_client(&self, m: Message) {
        let _ = self.client.sender.send(m);
    }

    fn notify_ra(&mut self, method: &str, params: Value) {
        self.send_ra(Message::Notification(Notification { method: method.into(), params }));
    }

    fn respond(&self, id: RequestId, result: Value) {
        self.to_client(Message::Response(Response { id, result: Some(result), error: None }));
    }

    fn handle_client(&mut self, m: Message) -> Res<()> {
        match m {
            Message::Request(r) => self.request(r),
            Message::Notification(n) => self.notification(n),
            Message::Response(r) => self.send_ra(Message::Response(r)),
        }
        Ok(())
    }

    fn request(&mut self, r: Request) {
        let uri = r
            .params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok());
        match uri {
            Some(u) if is_rsx(&u) => self.rsx_request(u, r),
            Some(u) => self.foreign_request(&u, r),
            None => self.global_request(r),
        }
    }

    fn global_request(&mut self, mut r: Request) {
        match r.method.as_str() {
            "workspace/symbol" => return self.workspace_symbols(r),
            // our own items (tags, attributes, components) are already complete; an empty answer would replace them
            "completionItem/resolve" if r.params.get("data").is_none() => {
                return self.respond(r.id, r.params);
            }
            "codeAction/resolve" => {
                let r#gen = r
                    .params
                    .pointer("/data/codeActionParams/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|u| Url::parse(u).ok());
                if r#gen.is_some_and(|g| self.gen_to_doc.contains_key(&g)) {
                    self.pending.insert(r.id.clone(), Pending::ActionResolve);
                }
            }
            _ => {}
        }
        {
            if r.method == "initialize" {
                self.initialize(&mut r);
            }
            if self.ra_in.is_none() && r.method == "initialize" {
                self.respond(r.id, json!({"capabilities": own_capabilities(&Value::Null)}));
                return;
            }
            if self.ra_in.is_none() {
                self.respond(r.id, Value::Null);
                return;
            }
            if r.method == "initialize" {
                self.pending_init(r.id.clone());
            }
            if r.method == "completionItem/resolve" {
                let r#gen = r
                    .params
                    .pointer("/data/position/textDocument/uri")
                    .and_then(Value::as_str)
                    .and_then(|u| Url::parse(u).ok());
                if let Some(doc) = r#gen.and_then(|g| self.gen_to_doc.get(&g)) {
                    self.pending.insert(r.id.clone(), Pending::Resolve(doc.clone()));
                }
            }
            self.send_ra(Message::Request(r));
        }
    }

    fn foreign_request(&mut self, uri: &Url, r: Request) {
        if is_project_rust(uri) && self.ra_in.is_some() {
            // completion, hover, diagnostics, formatting...: all of it, as asked
            trace!("{} on {} -> rust-analyzer", r.method, uri.path());
            if matches!(r.method.as_str(), "textDocument/completion" | "textDocument/hover")
                && self.status_asked < 3
            {
                // what rust-analyzer knows about this file (its crate, cfg, whether the workspace loaded)
                self.status_asked += 1;
                let id = RequestId::from(format!("yawn-status-{}", self.status_asked));
                self.pending.insert(id.clone(), Pending::Status);
                self.send_ra(Message::Request(Request {
                    id,
                    method: "rust-analyzer/analyzerStatus".into(),
                    params: json!({"textDocument": {"uri": uri}}),
                }));
            }
            return self.send_ra(Message::Request(r));
        }
        let locations = matches!(
            r.method.as_str(),
            "textDocument/definition"
                | "textDocument/typeDefinition"
                | "textDocument/declaration"
                | "textDocument/implementation"
                | "textDocument/references"
        );
        if !locations || self.ra_in.is_none() {
            return self.respond(r.id, Value::Null);
        }
        self.pending.insert(r.id.clone(), Pending::RsLocations);
        self.send_ra(Message::Request(r));
    }

    fn rsx_request(&mut self, uri: Url, r: Request) {
        if self.rsx_range_request(&uri, &r) {
            return;
        }
        let mut after_dot = false;
        if r.method == "textDocument/completion" {
            let at = r
                .params
                .get("position")
                .and_then(|p| serde_json::from_value::<Position>(p.clone()).ok());
            if let Some(offset) = at.and_then(|p| self.docs.get(&uri).map(|d| d.text.offset(p))) {
                let in_rust = self
                    .docs
                    .get(&uri)
                    .is_some_and(|d| matches!(context(&d.text.s, offset), Ctx::Rust));
                after_dot = in_rust && self.repair_for_completion(&uri, offset);
            }
        }
        let Some(doc) = self.docs.get(&uri) else {
            return self.respond(r.id, Value::Null);
        };
        let Some(pos) = r
            .params
            .get("position")
            .and_then(|p| serde_json::from_value::<Position>(p.clone()).ok())
        else {
            return self.respond(r.id, Value::Null);
        };
        let offset = doc.text.offset(pos);
        let ctx = context(&doc.text.s, offset);
        if r.method == "textDocument/onTypeFormatting" {
            let ch = r.params.get("ch").and_then(Value::as_str).unwrap_or("");
            let unit = match r.params.pointer("/options/insertSpaces").and_then(Value::as_bool) {
                Some(false) => "\t".to_string(),
                _ => " ".repeat(
                    r.params.pointer("/options/tabSize").and_then(Value::as_u64).unwrap_or(4)
                        as usize,
                ),
            };
            let edits = if matches!(ctx, Ctx::Children { .. }) {
                crate::ontype::on_type(&doc.text, offset, ch, &unit)
            } else {
                Vec::new()
            };
            trace!("onTypeFormatting {ch:?} in {ctx:?} -> {} edits", edits.len());
            return self.respond(r.id, serde_json::to_value(edits).unwrap_or(Value::Null));
        }
        trace!("{} at {}:{} in {:?}", r.method, pos.line + 1, pos.character + 1, ctx);
        if let (Ctx::Attr { tag, start }, "textDocument/hover") = (&ctx, r.method.as_str())
            && tag.starts_with(|c: char| c.is_uppercase())
            && self.prop_hover(&uri, &r, tag, *start, pos)
        {
            return;
        }
        if let Ctx::Attr { tag, start } = &ctx
            && r.method == "textDocument/completion"
            && tag.starts_with(|c: char| c.is_uppercase())
            && self.index.find(tag).is_none()
            && self.props_via_ra(&uri, &r, *start, offset)
        {
            return;
        }
        let Some(doc) = self.docs.get(&uri) else {
            return self.respond(r.id, Value::Null);
        };
        if !matches!(ctx, Ctx::Rust)
            && matches!(r.method.as_str(), "textDocument/completion" | "textDocument/hover")
        {
            let src = doc.text.s.clone();
            let text = Text::new(src.clone());
            let result = if r.method == "textDocument/completion"
                && matches!(ctx, Ctx::Class { .. })
            {
                // class names: only with `[tailwind] enabled = true`; long lists, so the editor asks again per keystroke
                let on = uri.to_file_path().is_ok_and(|p| crate::tailwind::enabled_for(&p));
                let items = if on {
                    complete::complete(
                        &ctx,
                        &Text::new(src.clone()),
                        offset,
                        &self.index,
                        None,
                        &Default::default(),
                    )
                } else {
                    Vec::new()
                };
                json!({"isIncomplete": true, "items": items})
            } else if r.method == "textDocument/completion" {
                let html = self.web_items(&ctx, &src, pos);
                let imports = self.component_imports(&uri);
                Value::Array(complete::complete(&ctx, &text, offset, &self.index, html, &imports))
            } else {
                self.hover_non_rust(&ctx, &src, pos, offset)
            };
            return self.respond(r.id, result);
        }
        if !matches!(ctx, Ctx::None) && self.component_request(&uri, &r, offset) {
            return;
        }
        // A space only triggers completion for attributes; in Rust code it would pop up noise.
        if r.method == "textDocument/completion"
            && matches!(ctx, Ctx::Rust)
            && matches!(
                r.params.pointer("/context/triggerCharacter").and_then(Value::as_str),
                Some(" " | "-")
            )
        {
            return self.respond(r.id, Value::Null);
        }
        let rust_methods = [
            "textDocument/completion",
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/typeDefinition",
            "textDocument/declaration",
            "textDocument/implementation",
            "textDocument/references",
            "textDocument/signatureHelp",
            "textDocument/documentHighlight",
            "textDocument/rename",
            "textDocument/prepareRename",
        ];
        let kind = match r.method.as_str() {
            "textDocument/rename" => Pending::Rename,
            "textDocument/prepareRename" => Pending::PrepareRename(uri.clone()),
            "textDocument/completion" if after_dot => Pending::CompletionAfterDot(uri.clone()),
            "textDocument/completion" => Pending::Completion(uri.clone()),
            "textDocument/hover" => Pending::Hover(uri.clone()),
            "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/declaration"
            | "textDocument/implementation"
            | "textDocument/references"
            | "textDocument/documentHighlight" => Pending::Locations,
            _ => Pending::Locations,
        };
        let mapped = (|| {
            if !rust_methods.contains(&r.method.as_str()) || !matches!(ctx, Ctx::Rust) {
                return None;
            }
            let g = doc.r#gen.as_ref()?;
            let gp = src_to_gen(&g.map, &doc.text, &g.text, pos)?;
            Some((g.uri.clone(), gp))
        })();
        let position_known = mapped.is_some();
        let Some((guri, gp)) = mapped.filter(|_| self.ra_in.is_some()) else {
            trace!(
                "  -> null (rust-analyzer running: {}, position in generated code: {})",
                self.ra_in.is_some(),
                position_known
            );
            return self.respond(r.id, Value::Null);
        };
        let mut params = r.params.clone();
        params["textDocument"]["uri"] = json!(guri);
        params["position"] = json!(gp);
        self.pending.insert(r.id.clone(), kind);
        self.send_ra(Message::Request(Request { id: r.id, method: r.method, params }));
    }

    fn web(&mut self) -> Option<&mut Web> {
        if self.web.is_none() && !self.web_failed {
            match Web::start() {
                Ok(w) => self.web = Some(w),
                Err(e) => {
                    self.web_failed = true;
                    eprintln!(
                        "yawn-lsp: HTML/CSS language services are off ({e}); install Node.js (or set YAWN_NODE)"
                    );
                }
            }
        }
        self.web.as_mut()
    }

    fn web_items(&mut self, ctx: &Ctx, src: &str, pos: Position) -> Option<Vec<Value>> {
        let (op, text) = match ctx {
            Ctx::Css { .. } => ("cssComplete", mask::css(src)),
            Ctx::Rust | Ctx::None | Ctx::Class { .. } => return None,
            _ => ("htmlComplete", mask::html(src)),
        };
        let r = self.web()?.call(
            op,
            &text,
            pos.line,
            pos.character,
            std::time::Duration::from_secs(5),
        )?;
        r.as_array().cloned()
    }

    fn hover_non_rust(&mut self, ctx: &Ctx, src: &str, pos: Position, offset: usize) -> Value {
        // a component in an opening or closing tag: its signature and the doc comment above it
        if let Some(name) = component_name_at(src, offset)
            && let Some(h) = self.index.find(&name)
        {
            return component_hover(h);
        }
        let (op, text) = match ctx {
            Ctx::Css { .. } => ("cssHover", mask::css(src)),
            Ctx::Tag { .. } | Ctx::Attr { .. } | Ctx::Event { .. } => {
                ("htmlHover", mask::html(src))
            }
            _ => return Value::Null,
        };
        self.web()
            .and_then(|w| {
                w.call(op, &text, pos.line, pos.character, std::time::Duration::from_secs(5))
            })
            .unwrap_or(Value::Null)
    }

    fn component_definition(&self, name: &str) -> Value {
        let open = |p: &Path| {
            Url::from_file_path(p).ok().and_then(|u| self.docs.get(&u)).map(|d| d.text.s.clone())
        };
        let Some((path, text, at)) = self.index.definition(name, &open) else {
            return Value::Null;
        };
        let t = Text::new(text);
        let Ok(uri) = Url::from_file_path(&path) else {
            return Value::Null;
        };
        json!({"uri": uri, "range": Range::new(t.position(at), t.position(at + name.len()))})
    }

    fn update_css(&mut self, uri: &Url) {
        let Some(src) = self.docs.get(uri).map(|d| d.text.s.clone()) else {
            return;
        };
        let diags = if src.contains("style") {
            let css = mask::css(&src);
            let found = if css.trim().is_empty() {
                None
            } else {
                self.web().and_then(|w| {
                    w.call("cssDiagnostics", &css, 0, 0, std::time::Duration::from_secs(2))
                })
            };
            found
                .and_then(|v| serde_json::from_value::<Vec<Diagnostic>>(v).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(d) = self.docs.get_mut(uri) {
            d.css = diags;
        }
    }

    fn initialize(&mut self, r: &mut Request) {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(u) =
            r.params.get("rootUri").and_then(Value::as_str).and_then(|u| Url::parse(u).ok())
        {
            roots.extend(u.to_file_path().ok());
        }
        for f in r.params.get("workspaceFolders").and_then(Value::as_array).into_iter().flatten() {
            if let Some(p) = f
                .get("uri")
                .and_then(Value::as_str)
                .and_then(|u| Url::parse(u).ok())
                .and_then(|u| u.to_file_path().ok())
            {
                roots.push(p);
            }
        }
        // rust-analyzer only finds a Cargo.toml in the workspace root or one level below; a project can sit deeper
        let mut linked: Vec<Value> = Vec::new();
        for root in &roots {
            self.index.scan(root);
            find_projects(root, 0, &mut linked);
        }
        // keep `cargo check` out of `yawn dev`'s `target/`, or they wait on each other's lock
        let opts = &mut r.params["initializationOptions"];
        if !opts.is_object() {
            *opts = json!({});
        }
        if opts.pointer("/cargo/targetDir").is_none() {
            opts["cargo"]["targetDir"] = json!(true);
        }
        let mut seen = HashSet::new();
        linked.retain(|v| seen.insert(v.to_string()));
        if !linked.is_empty() {
            let opts = &mut r.params["initializationOptions"];
            if !opts.is_object() {
                *opts = json!({});
            }
            let existing =
                opts.get("linkedProjects").and_then(Value::as_array).cloned().unwrap_or_default();
            linked.extend(existing.into_iter().filter(|v| seen.insert(v.to_string())));
            trace!("initialize: roots {roots:?}, linkedProjects {linked:?}");
            opts["linkedProjects"] = Value::Array(linked);
            self.spawn_ra();
        } else {
            trace!("initialize: roots {roots:?}, no Yawn.toml project found below them");
        }
    }

    fn pending_init(&mut self, id: RequestId) {
        self.pending.insert(id, Pending::Init);
    }

    fn notification(&mut self, n: Notification) {
        let uri = n
            .params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok());
        match (n.method.as_str(), uri) {
            ("textDocument/didOpen", Some(uri)) if is_rsx(&uri) => {
                let text = n
                    .params
                    .pointer("/textDocument/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.open(uri, text);
            }
            ("textDocument/didChange", Some(uri)) if is_rsx(&uri) => {
                if let Some(text) = n
                    .params
                    .pointer("/contentChanges")
                    .and_then(Value::as_array)
                    .and_then(|c| c.last())
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                {
                    self.change(&uri, text.to_string());
                }
            }
            ("textDocument/didClose", Some(uri)) if is_rsx(&uri) => self.close(&uri),
            ("textDocument/didSave", Some(uri)) if is_rsx(&uri) => {
                if let Some(root) = self.docs.get(&uri).and_then(|d| d.root.clone()) {
                    let _ = project::compile_project(&root); // keeps the generated files on disk current
                }
                // rust-analyzer's `cargo check` runs when it is told a file was saved. The user saved the
                // `.rsx`, so tell it the generated module was: otherwise errors from the last check stay.
                if let Some(guri) =
                    self.docs.get(&uri).and_then(|d| d.r#gen.as_ref().map(|g| g.uri.clone()))
                {
                    trace!("didSave {uri} -> rust-analyzer didSave {guri}");
                    self.notify_ra("textDocument/didSave", json!({"textDocument": {"uri": guri}}));
                }
            }
            (method, uri) => {
                if let Some(u) = uri
                    .as_ref()
                    .filter(|u| method == "textDocument/didOpen" && u.path().ends_with(".rs"))
                {
                    trace!(
                        "didOpen {} (served here: {}, rust-analyzer running: {})",
                        u.path(),
                        is_project_rust(u),
                        self.ra_in.is_some()
                    );
                }
                self.send_ra(Message::Notification(n))
            }
        }
    }

    fn open(&mut self, uri: Url, text: String) {
        let root = uri.to_file_path().ok().and_then(|p| find_root(&p));
        if let Some(root) = &root
            && self.compiled_roots.insert(root.clone())
        {
            // rust-analyzer needs the generated modules to exist on disk once.
            let _ = project::compile_project(root);
            self.index.scan(root);
        }
        if let Ok(p) = uri.to_file_path() {
            self.index.set(p, &text);
        }
        self.docs.insert(
            uri.clone(),
            Doc {
                text: Text::new(text),
                root,
                r#gen: None,
                own: Vec::new(),
                ra: Vec::new(),
                ra_raw: Vec::new(),
                css: Vec::new(),
            },
        );
        self.refresh(&uri);
    }

    fn change(&mut self, uri: &Url, text: String) {
        if let Some(d) = self.docs.get_mut(uri) {
            d.text = Text::new(text.clone());
        }
        if let Ok(p) = uri.to_file_path() {
            self.index.set(p, &text);
        }
        self.refresh(uri);
    }

    fn close(&mut self, uri: &Url) {
        if let Some(doc) = self.docs.remove(uri)
            && let Some(g) = doc.r#gen
        {
            self.gen_to_doc.remove(&g.uri);
            // Back to the file on disk (kept current by didSave).
            self.notify_ra("textDocument/didClose", json!({"textDocument": {"uri": g.uri}}));
        }
        self.to_client(Message::Notification(Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: json!({"uri": uri, "diagnostics": []}),
        }));
    }

    fn prop_hover(
        &mut self,
        uri: &Url,
        r: &Request,
        tag: &str,
        start: usize,
        pos: Position,
    ) -> bool {
        let Some(doc) = self.docs.get(uri) else {
            return false;
        };
        let src = &doc.text.s;
        let end = src[start..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(src.len(), |n| start + n);
        let name = &src[start..end];
        if name.is_empty() {
            return false;
        }
        if let Some(h) = self.index.find(tag) {
            let Some((_, ty)) = h.params.iter().find(|(n, _)| n == name) else {
                return false;
            };
            let value = format!("```rust\n(prop) {name}: {ty}\n```\non `{tag}`");
            self.respond(r.id.clone(), json!({"contents": {"kind": "markdown", "value": value}}));
            return true;
        }
        let Some(g) = doc.r#gen.as_ref() else {
            return false;
        };
        if self.ra_in.is_none() {
            return false;
        }
        let Some(gp) = src_to_gen(&g.map, &doc.text, &g.text, pos) else {
            return false;
        };
        let mut params = r.params.clone();
        params["textDocument"]["uri"] = json!(g.uri);
        params["position"] = json!(gp);
        self.pending.insert(r.id.clone(), Pending::Hover(uri.clone()));
        self.send_ra(Message::Request(Request {
            id: r.id.clone(),
            method: r.method.clone(),
            params,
        }));
        true
    }

    fn props_via_ra(&mut self, uri: &Url, r: &Request, start: usize, offset: usize) -> bool {
        if self.ra_in.is_none() {
            return false;
        }
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        let rel = doc.root.as_ref().and_then(|root| {
            uri.to_file_path().ok().and_then(|p| {
                p.strip_prefix(root).ok().map(|x| x.to_string_lossy().replace('\\', "/"))
            })
        });
        let rel = rel.unwrap_or_else(|| "file.rsx".into());
        let patched = format!("{}__yawn__{}", &doc.text.s[..start], &doc.text.s[offset..]);
        let Ok(c) = yawn_rsx::compile(&patched, &rel) else {
            return false;
        };
        let Some((guri, version, text, opened)) = install_gen(doc, &rel, c.rust, c.map) else {
            return false;
        };
        let Some(at) = text.find("__yawn__: true,") else {
            return false;
        };
        let pos = Text::new(text.clone()).position(at);
        self.gen_to_doc.insert(guri.clone(), uri.clone());
        if opened {
            self.notify_ra("textDocument/didChange", json!({"textDocument": {"uri": guri, "version": version}, "contentChanges": [{"text": text}]}));
        } else {
            self.notify_ra("textDocument/didOpen", json!({"textDocument": {"uri": guri, "languageId": "rust", "version": version, "text": text}}));
        }
        self.pending.insert(r.id.clone(), Pending::Props { uri: uri.clone(), start, offset });
        let params = json!({"textDocument": {"uri": guri}, "position": pos});
        self.send_ra(Message::Request(Request {
            id: r.id.clone(),
            method: "textDocument/completion".into(),
            params,
        }));
        true
    }

    fn repair_for_completion(&mut self, uri: &Url, offset: usize) -> bool {
        let Some(doc) = self.docs.get_mut(uri) else {
            return false;
        };
        let before = &doc.text.s[..offset];
        let after_ident =
            doc.text.s[offset..].chars().next().is_some_and(|c| c.is_alphanumeric() || c == '_');
        if !(before.ends_with('.') && !before.ends_with("..") || before.ends_with("::"))
            || after_ident
        {
            return false;
        }
        let rel = doc
            .root
            .as_ref()
            .and_then(|r| {
                uri.to_file_path().ok().and_then(|p| {
                    p.strip_prefix(r).ok().map(|x| x.to_string_lossy().replace('\\', "/"))
                })
            })
            .unwrap_or_else(|| "file.rsx".into());
        if yawn_rsx::compile(&doc.text.s, &rel).is_ok() {
            return false;
        }
        // `let t = params.` + newline needs a `;` to be a statement; in an expression it must not
        // have one. Try both.
        let rest = &doc.text.s[offset..];
        let Some(c) = ["__yawn__", "__yawn__;"]
            .iter()
            .find_map(|p| yawn_rsx::compile(&format!("{before}{p}{rest}"), &rel).ok())
        else {
            return false;
        };
        let Some((guri, version, text, opened)) = install_gen(doc, &rel, c.rust, c.map) else {
            return false;
        };
        self.gen_to_doc.insert(guri.clone(), uri.clone());
        if opened {
            self.notify_ra("textDocument/didChange", json!({"textDocument": {"uri": guri, "version": version}, "contentChanges": [{"text": text}]}));
        } else {
            self.notify_ra("textDocument/didOpen", json!({"textDocument": {"uri": guri, "languageId": "rust", "version": version, "text": text}}));
        }
        true
    }

    fn refresh(&mut self, uri: &Url) {
        let Some(doc) = self.docs.get_mut(uri) else {
            return;
        };
        let rel = doc
            .root
            .as_ref()
            .and_then(|r| {
                uri.to_file_path().ok().and_then(|p| {
                    p.strip_prefix(r).ok().map(|x| x.to_string_lossy().replace('\\', "/"))
                })
            })
            .unwrap_or_else(|| "file.rsx".into());
        let mut sync: Option<(Url, i32, String, bool)> = None;
        match yawn_rsx::compile(&doc.text.s, &rel) {
            Ok(c) => {
                doc.own.clear();
                sync = install_gen(doc, &rel, c.rust, c.map);
            }
            Err(d) => {
                doc.own = vec![lsp_diag(&d, &doc.text)];
                // A fresh `prop={}` is an error of ours, but it shouldn't blind rust-analyzer to
                // the rest of the file: show it a copy with `todo!()` in the empty braces.
                if let Some((own, c)) = recover_empty_expressions(&doc.text.s, &rel, &doc.text, d) {
                    doc.own = own;
                    sync = install_gen(doc, &rel, c.rust, c.map);
                }
            }
        }
        if let Some((guri, version, text, opened)) = sync {
            self.gen_to_doc.insert(guri.clone(), uri.clone());
            if opened {
                self.notify_ra("textDocument/didChange", json!({"textDocument": {"uri": guri, "version": version}, "contentChanges": [{"text": text}]}));
            } else {
                self.notify_ra("textDocument/didOpen", json!({"textDocument": {"uri": guri, "languageId": "rust", "version": version, "text": text}}));
            }
        }
        self.update_css(uri);
        self.publish(uri);
    }

    fn publish(&self, uri: &Url) {
        let Some(d) = self.docs.get(uri) else { return };
        let mut all = d.own.clone();
        all.extend(d.css.iter().cloned());
        all.extend(d.ra.iter().cloned());
        self.to_client(Message::Notification(Notification {
            method: "textDocument/publishDiagnostics".into(),
            params: json!({"uri": uri, "diagnostics": all}),
        }));
    }

    fn handle_ra(&mut self, m: Message) -> Res<()> {
        match m {
            Message::Response(mut r) => {
                if matches!(self.pending.get(&r.id), Some(Pending::Init)) {
                    self.pending.remove(&r.id);
                    if let Some(res) = &mut r.result {
                        let caps = res.get("capabilities").cloned().unwrap_or(Value::Null);
                        res["capabilities"] = own_capabilities(&caps);
                    }
                    self.to_client(Message::Response(r));
                    return Ok(());
                }
                if matches!(self.pending.get(&r.id), Some(Pending::Status)) {
                    self.pending.remove(&r.id);
                    let text = r.result.as_ref().and_then(Value::as_str).unwrap_or("(no answer)");
                    trace!(
                        "rust-analyzer status:\n{}",
                        text.lines().take(40).collect::<Vec<_>>().join("\n")
                    );
                    return Ok(());
                }
                match self.pending.remove(&r.id) {
                    Some(kind) => {
                        // the generated code was a copy with a placeholder; put the real one back
                        let restore = if let Pending::Props { uri, .. } = &kind {
                            Some(uri.clone())
                        } else {
                            None
                        };
                        trace!(
                            "  <- rust-analyzer: {}",
                            match (&r.result, &r.error) {
                                (_, Some(e)) => format!("error {}", e.message),
                                (Some(v), _) => v
                                    .get("items")
                                    .and_then(Value::as_array)
                                    .or_else(|| v.as_array())
                                    .map_or("answer".to_string(), |a| format!("{} items", a.len())),
                                _ => "null".into(),
                            }
                        );
                        if let Some(res) = r.result.take() {
                            r.result = Some(self.map_result(kind, res));
                        }
                        self.to_client(Message::Response(r));
                        if let Some(uri) = restore {
                            self.refresh(&uri);
                        }
                    }
                    None => {
                        trace!(
                            "  <- rust-analyzer (as is): {}",
                            match (&r.result, &r.error) {
                                (_, Some(e)) => format!("error {}", e.message),
                                (Some(v), _) => v
                                    .get("items")
                                    .and_then(Value::as_array)
                                    .or_else(|| v.as_array())
                                    .map_or("answer".to_string(), |a| format!("{} items", a.len())),
                                _ => "null".into(),
                            }
                        );
                        self.to_client(Message::Response(r))
                    }
                }
            }
            Message::Notification(n) if n.method == "textDocument/publishDiagnostics" => {
                let uri =
                    n.params.get("uri").and_then(Value::as_str).and_then(|u| Url::parse(u).ok());
                // Diagnostics of other files are the editor's own rust-analyzer's business, except for the
                // `.rs` files of a project it can't see (below).
                if let Some(rsx_uri) = uri.as_ref().and_then(|u| self.gen_to_doc.get(u)).cloned() {
                    let diags: Vec<Diagnostic> = serde_json::from_value(
                        n.params.get("diagnostics").cloned().unwrap_or(Value::Null),
                    )
                    .unwrap_or_default();
                    if let Some(doc) = self.docs.get_mut(&rsx_uri) {
                        let mapped = match &doc.r#gen {
                            Some(g) => diags
                                .iter()
                                .cloned()
                                .filter_map(|d| map_diag(d, g, &doc.text))
                                .collect(),
                            None => Vec::new(),
                        };
                        doc.ra = mapped;
                        doc.ra_raw = diags;
                    }
                    self.publish(&rsx_uri);
                } else if uri.as_ref().is_some_and(is_project_rust) {
                    trace!(
                        "diagnostics of {} <- rust-analyzer: {}",
                        uri.as_ref().map_or("", |u| u.path()),
                        n.params.get("diagnostics").and_then(Value::as_array).map_or(0, Vec::len)
                    );
                    self.to_client(Message::Notification(n));
                }
            }
            m => self.to_client(m),
        }
        Ok(())
    }

    fn map_result(&self, kind: Pending, mut res: Value) -> Value {
        match kind {
            Pending::CodeAction(..)
            | Pending::ActionResolve
            | Pending::InlayHints(_)
            | Pending::Rename
            | Pending::PrepareRename(_)
            | Pending::Tokens(_)
            | Pending::RsLocations => self.map_ops(kind, res),
            Pending::Completion(ref uri) | Pending::CompletionAfterDot(ref uri) => {
                let after_dot = matches!(kind, Pending::CompletionAfterDot(_));
                let uri = uri.clone();
                let Some(doc) = self.docs.get(&uri) else {
                    return res;
                };
                let Some(g) = &doc.r#gen else { return res };
                let items = if res.is_array() {
                    res.as_array_mut()
                } else {
                    res.get_mut("items").and_then(Value::as_array_mut)
                };
                for item in items.into_iter().flatten() {
                    if after_dot {
                        // the edit's range covers the placeholder, which isn't in the source
                        if let Some(text) =
                            item.get("textEdit").and_then(|t| t.get("newText")).cloned()
                        {
                            item["insertText"] = text;
                            item.as_object_mut().map(|o| o.remove("textEdit"));
                        }
                    }
                    map_completion_item(item, g, &doc.text);
                }
                res
            }
            Pending::Props { uri, start, offset } => {
                let Some(doc) = self.docs.get(&uri) else {
                    return Value::Array(Vec::new());
                };
                let items = if res.is_array() {
                    res.as_array()
                } else {
                    res.get("items").and_then(Value::as_array)
                };
                // struct fields only (kind 5); the type is the detail
                let fields: Vec<(String, String)> = items
                    .into_iter()
                    .flatten()
                    .filter(|i| i.get("kind").and_then(Value::as_u64) == Some(5))
                    .filter_map(|i| {
                        let label = i.get("label").and_then(Value::as_str)?;
                        let name = label
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or(label)
                            .to_string();
                        let ty = i
                            .get("labelDetails")
                            .and_then(|d| d.get("detail"))
                            .and_then(Value::as_str)
                            .or_else(|| i.get("detail").and_then(Value::as_str))
                            .unwrap_or("")
                            .trim_start_matches(": ")
                            .to_string();
                        Some((name, ty))
                    })
                    .collect();
                Value::Array(complete::prop_items(
                    &Text::new(doc.text.s.clone()),
                    start,
                    offset,
                    &fields,
                ))
            }
            Pending::Resolve(uri) => {
                if let Some(g) =
                    self.docs.get(&uri).and_then(|d| d.r#gen.as_ref().map(|g| (g, &d.text)))
                {
                    map_completion_item(&mut res, g.0, g.1);
                }
                res
            }
            Pending::Hover(uri) => {
                if let (Some(doc), Some(range)) = (self.docs.get(&uri), res.get("range").cloned())
                    && let Some(g) = &doc.r#gen
                {
                    match serde_json::from_value::<Range>(range)
                        .ok()
                        .and_then(|r| map_range(r, g, &doc.text))
                    {
                        Some(r) => res["range"] = json!(r),
                        None => {
                            res.as_object_mut().map(|o| o.remove("range"));
                        }
                    }
                }
                res
            }
            Pending::Init | Pending::Status => res,
            Pending::Locations => {
                match &mut res {
                    Value::Array(a) => a.iter_mut().for_each(|l| self.map_location(l)),
                    Value::Object(_) => self.map_location(&mut res),
                    _ => {}
                }
                res
            }
        }
    }

    fn map_location(&self, loc: &mut Value) {
        let uri_key = if loc.get("targetUri").is_some() { "targetUri" } else { "uri" };
        let Some(uri) = loc.get(uri_key).and_then(Value::as_str).and_then(|u| Url::parse(u).ok())
        else {
            return;
        };
        let Some((rsx_uri, map, gen_text, src_text)) = self.resolve_generated(&uri) else {
            return;
        };
        let conv = |v: &mut Value, key: &str| {
            if let Some(r) =
                v.get(key).cloned().and_then(|r| serde_json::from_value::<Range>(r).ok())
            {
                let s = gen_to_src(&map, &gen_text, &src_text, r.start).map(|x| x.0);
                let e = gen_to_src(&map, &gen_text, &src_text, r.end).map(|x| x.0);
                if let (Some(s), Some(e)) = (s, e) {
                    v[key] = json!(Range::new(
                        s,
                        if e > s { e } else { Position::new(s.line, s.character + 1) }
                    ));
                }
            }
        };
        loc[uri_key] = json!(rsx_uri);
        for key in ["range", "targetRange", "targetSelectionRange", "originSelectionRange"] {
            conv(loc, key);
        }
    }

    fn resolve_generated(&self, r#gen: &Url) -> Option<(Url, SourceMap, Text, Text)> {
        if let Some(rsx) = self.gen_to_doc.get(r#gen) {
            let doc = self.docs.get(rsx)?;
            let g = doc.r#gen.as_ref()?;
            return Some((
                rsx.clone(),
                g.map.clone(),
                Text::new(g.text.s.clone()),
                Text::new(doc.text.s.clone()),
            ));
        }
        // Not open: use what's on disk.
        let path = r#gen.to_file_path().ok()?;
        let (map_path, root) = if path.ends_with(project::CLIENT_ROOT) {
            // the client crate root, generated from main.rsx
            let root = path.parent()?.parent()?.to_path_buf();
            (root.join(project::OUT_DIR).join("main.map"), root)
        } else {
            (
                path.with_extension("map"),
                path.ancestors()
                    .find(|a| a.ends_with(".yawn"))
                    .and_then(Path::parent)?
                    .to_path_buf(),
            )
        };
        let map = SourceMap::parse(&std::fs::read_to_string(map_path).ok()?)?;
        let rsx = Url::from_file_path(root.join(&map.source)).ok()?;
        let gen_text = Text::new(std::fs::read_to_string(&path).ok()?);
        let src_text = Text::new(std::fs::read_to_string(root.join(&map.source)).ok()?);
        Some((rsx, map, gen_text, src_text))
    }
}

fn map_range(r: Range, g: &Gen, src: &Text) -> Option<Range> {
    let (s, se) = gen_to_src(&g.map, &g.text, src, r.start)?;
    let (e, ee) = gen_to_src(&g.map, &g.text, src, r.end)?;
    (se && ee).then_some(Range::new(s, e))
}

fn map_diag(mut d: Diagnostic, g: &Gen, src: &Text) -> Option<Diagnostic> {
    let (s, se) = gen_to_src(&g.map, &g.text, src, d.range.start)?;
    let (e, ee) = gen_to_src(&g.map, &g.text, src, d.range.end)?;
    // Glue code the user didn't write: only errors are worth showing, at the construct.
    if !(se && ee) && d.severity != Some(DiagnosticSeverity::ERROR) {
        return None;
    }
    d.range = if se && ee && e > s {
        Range::new(s, e)
    } else if d.message.starts_with("expected") || d.message.contains("unclosed") {
        // An error in the glue after the user's Rust comes from unfinished code (`let x =` then
        // `view`): point at the last character the user wrote
        let before = src.s[..src.offset(s)].trim_end();
        match before.char_indices().last() {
            Some((i, c)) => Range::new(src.position(i), src.position(i + c.len_utf8())),
            None => Range::new(s, Position::new(s.line, s.character + 1)),
        }
    } else {
        Range::new(s, Position::new(s.line, s.character + 1))
    };
    d.related_information = None;
    Some(d)
}

fn map_completion_item(item: &mut Value, g: &Gen, src: &Text) {
    if let Some(edit) = item.get("textEdit").cloned() {
        let mut ok = false;
        let mut edit = edit;
        for key in ["range", "insert", "replace"] {
            if let Some(r) =
                edit.get(key).cloned().and_then(|r| serde_json::from_value::<Range>(r).ok())
                && let Some(m) = map_range(r, g, src)
            {
                edit[key] = json!(m);
                ok = true;
            }
        }
        if ok {
            item["textEdit"] = edit;
        } else {
            item.as_object_mut().map(|o| o.remove("textEdit"));
        }
    }
    // Auto-imports are edits at the top of the generated file: put `use` lines at the top of the `.rsx`.
    if let Some(edits) = item.get_mut("additionalTextEdits").and_then(Value::as_array_mut) {
        let mapped: Vec<Value> = edits
            .iter()
            .filter_map(|e| {
                let r = serde_json::from_value::<Range>(e.get("range")?.clone()).ok()?;
                let text = e.get("newText")?.as_str()?;
                match map_range(r, g, src) {
                    Some(m) => Some(json!({"range": m, "newText": text})),
                    None if text.contains("use ") => {
                        let (at, text) = import_edit(src, text);
                        Some(json!({"range": Range::new(at, at), "newText": text}))
                    }
                    None => None,
                }
            })
            .collect();
        *edits = mapped;
    }
}

pub(crate) fn import_edit(src: &Text, text: &str) -> (Position, String) {
    let mut last = None;
    for (n, line) in src.s.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("component ") || t.starts_with("pub component ") {
            break;
        }
        if (line.starts_with("use ") || line.starts_with("pub use "))
            && line.trim_end().ends_with(';')
        {
            last = Some(n);
        }
    }
    match last {
        Some(n) => (Position::new(n as u32 + 1, 0), format!("{}\n", text.trim_matches('\n'))),
        None => (Position::new(0, 0), format!("{}\n\n", text.trim_matches('\n'))),
    }
}

fn own_capabilities(ra: &Value) -> Value {
    let mut caps = json!({"textDocumentSync": {"openClose": true, "change": 1, "save": {}}});
    let mut triggers: Vec<Value> = ra
        .pointer("/completionProvider/triggerCharacters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for t in [".", ":", "<", "'", "(", " ", "-"] {
        if !triggers.iter().any(|x| x == t) {
            triggers.push(json!(t));
        }
    }
    let resolve =
        ra.pointer("/completionProvider/resolveProvider").and_then(Value::as_bool).unwrap_or(false);
    caps["completionProvider"] = json!({"triggerCharacters": triggers, "resolveProvider": resolve});
    for k in [
        "hoverProvider",
        "definitionProvider",
        "typeDefinitionProvider",
        "declarationProvider",
        "implementationProvider",
        "referencesProvider",
        "documentHighlightProvider",
    ] {
        caps[k] = json!(true);
    }
    caps["documentOnTypeFormattingProvider"] =
        json!({"firstTriggerCharacter": ">", "moreTriggerCharacter": ["\n"]});
    caps["signatureHelpProvider"] = ra
        .get("signatureHelpProvider")
        .cloned()
        .unwrap_or_else(|| json!({"triggerCharacters": ["(", ","]}));
    caps["documentSymbolProvider"] = json!(true);
    caps["workspaceSymbolProvider"] = json!(true);
    caps["documentFormattingProvider"] = json!(true);
    caps["renameProvider"] = json!({"prepareProvider": true});
    let has_ra = ra.is_object();
    caps["codeActionProvider"] = json!({"codeActionKinds": ["quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source", "source.organizeImports"], "resolveProvider": has_ra});
    if has_ra {
        caps["inlayHintProvider"] = json!(true);
        if let Some(legend) = ra.pointer("/semanticTokensProvider/legend") {
            caps["semanticTokensProvider"] =
                json!({"legend": legend, "full": true, "range": false});
        }
    }
    caps
}

#[cfg(test)]
mod project_rust_tests {
    use super::*;

    #[test]
    fn plain_rust_files_of_a_main_rsx_project_are_served_here() {
        let root = std::env::temp_dir().join(format!("yawn-lsp-rs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/api")).unwrap();
        std::fs::write(root.join("Yawn.toml"), "").unwrap();
        let uri = |p: &str| Url::from_file_path(root.join(p)).unwrap();
        assert!(
            !is_project_rust(&uri("src/api/x.rs")),
            "no main.rsx: an ordinary crate, the editor's rust-analyzer owns it"
        );
        std::fs::write(root.join("src/main.rsx"), "").unwrap();
        assert!(is_project_rust(&uri("src/api/x.rs")));
        assert!(!is_project_rust(&uri("src/main.rsx")));
        let _ = std::fs::remove_dir_all(&root);
    }
}

fn component_name_at(src: &str, offset: usize) -> Option<String> {
    let offset = offset.min(src.len());
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let start = src[..offset]
        .rfind(|c: char| !word(c))
        .map_or(0, |n| n + src[n..].chars().next().map_or(1, char::len_utf8));
    let end = src[offset..].find(|c: char| !word(c)).map_or(src.len(), |n| offset + n);
    let name = &src[start..end];
    let before = &src[..start];
    (name.starts_with(|c: char| c.is_uppercase())
        && (before.ends_with('<') || before.ends_with("</")))
    .then(|| name.to_string())
}

fn component_hover(h: &yawn_rsx::parser::Header) -> Value {
    let sig = h.params.iter().map(|(n, t)| format!("{n}: {t}")).collect::<Vec<_>>().join(", ");
    let mut value = format!("```rust\ncomponent {}({sig})\n```", h.name);
    if !h.doc.is_empty() {
        value.push_str("\n\n---\n\n");
        value.push_str(&h.doc);
    }
    json!({"contents": {"kind": "markdown", "value": value}})
}

#[cfg(test)]
mod hover_tests {
    use super::component_name_at;

    #[test]
    fn finds_the_component_in_open_close_and_self_closing_tags() {
        let src = "<div><Test a=\"1\" /></Test>";
        assert_eq!(component_name_at(src, 8).as_deref(), Some("Test"));
        assert_eq!(component_name_at(src, 23).as_deref(), Some("Test"));
        assert_eq!(component_name_at(src, 2), None);
        assert_eq!(component_name_at(src, 12), None);
    }
}
