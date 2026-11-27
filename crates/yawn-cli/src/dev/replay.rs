use super::cargo::{Artifact, CargoRun, KillGroup, Parsed, own_group, parse_json_line};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Instant, SystemTime};

pub const LOG_ENV: &str = "YAWN_REPLAY_LOG";
const WRAPPER_ENV: &str = "RUSTC_WORKSPACE_WRAPPER";

/// When `yawn` is started by cargo as its rustc wrapper, notes the command line for later
/// replay and then becomes rustc
pub fn shim() -> Option<ExitCode> {
    let log = std::env::var_os(LOG_ENV)?;
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let rustc = args.get(1)?;
    if Path::new(rustc).file_stem().is_none_or(|s| s != "rustc") {
        return None;
    }
    let argv: Vec<String> = args[1..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let name = argv.iter().position(|a| a == "--crate-name").and_then(|i| argv.get(i + 1));
    if let Some(name) = name
        && name != "___"
        && !name.starts_with("build_script")
        && !argv.iter().any(|a| a.starts_with("--print"))
    {
        let env: Vec<(String, String)> = std::env::vars().collect();
        let cwd = std::env::current_dir().unwrap_or_default();
        let line = serde_json::json!({ "cwd": cwd, "argv": argv, "env": env }).to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            let _ = writeln!(f, "{line}");
        }
    }
    let mut cmd = Command::new(rustc);
    cmd.args(&args[2..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        eprintln!("yawn: cannot run rustc: {}", cmd.exec());
        Some(ExitCode::FAILURE)
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => Some(ExitCode::from(s.code().unwrap_or(1) as u8)),
            Err(_) => Some(ExitCode::FAILURE),
        }
    }
}

#[derive(Clone)]
struct Cmd {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    out_dir: PathBuf,
    key: String,
    externs: Vec<String>,
    linked: bool,
}

impl Cmd {
    fn parse(line: &str) -> Option<Cmd> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let strings = |x: &serde_json::Value| -> Vec<String> {
            x.as_array()
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        let argv = strings(v.get("argv")?);
        let env = v
            .get("env")?
            .as_array()?
            .iter()
            .filter_map(|p| {
                let p = p.as_array()?;
                Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
            })
            .collect();
        let cwd = PathBuf::from(v.get("cwd")?.as_str()?);
        let (mut name, mut extra, mut out_dir) = (None, String::new(), None);
        let (mut externs, mut linked) = (Vec::new(), false);
        let mut i = 1;
        while i < argv.len() {
            let a = argv[i].as_str();
            let next = argv.get(i + 1).map(String::as_str).unwrap_or("");
            let codegen = a.strip_prefix("-C").filter(|c| !c.is_empty()).or(if a == "-C" {
                Some(next)
            } else {
                None
            });
            if let Some(c) = codegen
                && let Some(e) = c.strip_prefix("extra-filename=")
            {
                extra = e.to_string();
            }
            match a {
                "--crate-name" => name = Some(next.to_string()),
                "--out-dir" => out_dir = Some(PathBuf::from(next)),
                "--crate-type" => linked |= matches!(next, "bin" | "cdylib"),
                "--extern" => {
                    if let Some((_, path)) = next.split_once('=')
                        && let Some(stem) = Path::new(path).file_stem().and_then(|s| s.to_str())
                    {
                        externs.push(stem.strip_prefix("lib").unwrap_or(stem).to_string());
                    }
                }
                _ => {}
            }
            i += 1;
        }
        let name = name?;
        Some(Cmd {
            key: format!("{name}{extra}"),
            argv,
            env,
            cwd,
            out_dir: out_dir?,
            externs,
            linked,
        })
    }
}

struct DepInfo {
    stamp: (SystemTime, u64),
    targets: Vec<PathBuf>,
    sources: Vec<PathBuf>,
    external: bool,
}

fn split_escaped(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                cur.push(' ');
                chars.next();
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn read_dep_info(path: &Path, cwd: &Path) -> Option<DepInfo> {
    let meta = std::fs::metadata(path).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().find(|l| !l.starts_with('#') && l.contains(": "))?;
    let (targets, sources) = line.split_once(": ")?;
    let abs = |s: String| {
        let p = PathBuf::from(s);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let sources: Vec<PathBuf> = split_escaped(sources).into_iter().map(abs).collect();
    let external = sources.iter().all(|s| {
        let s = s.to_string_lossy();
        s.contains("/registry/src/") || s.contains("/git/checkouts/") || s.contains("/rustlib/")
    });
    Some(DepInfo {
        stamp: (meta.modified().ok()?, meta.len()),
        targets: split_escaped(targets).into_iter().map(abs).collect(),
        sources,
        external,
    })
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Changes when anything cargo needs to re-plan changes: manifests, lockfiles, config
fn manifest_stamp(root: &Path) -> u64 {
    let mut files: Vec<PathBuf> = ["Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain.toml"]
        .iter()
        .map(|f| root.join(f))
        .collect();
    files.push(root.join(".cargo/config.toml"));
    files.push(root.join(yawn_rsx::manifest::MANIFEST));
    if let Some(dir) = root.join(yawn_rsx::manifest::MANIFEST).parent() {
        files.push(dir.join("Cargo.lock"));
        if let Ok(rd) = std::fs::read_dir(dir.join("crates")) {
            for e in rd.flatten() {
                files.push(e.path().join("Cargo.toml"));
            }
        }
    }
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in files {
        match std::fs::metadata(&f) {
            Ok(m) => (f, m.modified().ok(), m.len()).hash(&mut h),
            Err(_) => f.hash(&mut h),
        }
    }
    h.finish()
}

/// A rustc command cargo ran, to run again with other arguments
#[derive(Clone)]
pub struct Recorded {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub out_dir: PathBuf,
}

impl From<&Cmd> for Recorded {
    fn from(c: &Cmd) -> Recorded {
        Recorded {
            argv: c.argv.clone(),
            env: c.env.clone(),
            cwd: c.cwd.clone(),
            out_dir: c.out_dir.clone(),
        }
    }
}

/// Runs a recorded rustc command; `Ok(false)` when it failed (its diagnostics went to `on`)
pub fn run_recorded(
    argv: &[String],
    env: &[(String, String)],
    cwd: &Path,
    kill: &KillGroup,
    on: &mut dyn FnMut(&Parsed),
) -> std::io::Result<bool> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(
            env.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "CARGO_MAKEFLAGS" | "MAKEFLAGS" | "MFLAGS"))
                .map(|(k, v)| (k, v)),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let child = cmd.spawn()?;
    let pid = child.id();
    kill.set(pid);
    let out = child.wait_with_output();
    kill.release(pid);
    let out = out?;
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        if line.contains("\"$message_type\":\"diagnostic\"")
            && let Some(p) =
                parse_json_line(&format!(r#"{{"reason":"compiler-message","message":{line}}}"#))
        {
            on(&p);
        }
    }
    Ok(out.status.success())
}

pub struct Lane {
    root: PathBuf,
    log: PathBuf,
    pos: u64,
    stamp: u64,
    cmds: HashMap<String, Cmd>,
    finals: Vec<Artifact>,
    deps: HashMap<PathBuf, DepInfo>,
    current: HashSet<String>,
    complete: bool,
}

impl Lane {
    /// `None` when replay is switched off or something else already wraps rustc
    pub fn new(root: &Path, log: PathBuf) -> Option<Lane> {
        if std::env::var_os("YAWN_NO_REPLAY").is_some() || std::env::var_os(WRAPPER_ENV).is_some()
        {
            return None;
        }
        // the commands of an earlier session stay valid while the manifests are the same
        let kept = std::fs::read_to_string(log.with_extension("stamp"))
            .ok()
            .and_then(|t| t.trim().parse::<u64>().ok())
            .filter(|stamp| *stamp == manifest_stamp(root) && std::fs::metadata(&log).is_ok_and(|m| m.len() > 0));
        if kept.is_none() {
            let _ = std::fs::write(&log, b"");
        }
        Some(Lane {
            root: root.to_path_buf(),
            log,
            pos: 0,
            stamp: kept.unwrap_or(0),
            cmds: HashMap::new(),
            finals: Vec::new(),
            deps: HashMap::new(),
            current: HashSet::new(),
            complete: false,
        })
    }

    /// The environment that makes cargo report its rustc commands
    pub fn env(&self) -> Vec<(String, String)> {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned());
        let mut env = vec![(LOG_ENV.to_string(), self.log.to_string_lossy().into_owned())];
        if let Ok(exe) = exe {
            env.push((WRAPPER_ENV.to_string(), exe));
        }
        env
    }

    /// The rustc command of a crate of the workspace, as cargo last ran it
    pub fn recorded(&mut self, crate_name: &str) -> Option<Recorded> {
        self.learn();
        let prefix = format!("{crate_name}-");
        self.cmds.values().find(|c| c.key.starts_with(&prefix)).map(Recorded::from)
    }

    /// The command that links the server binary
    pub fn recorded_bin(&mut self, crate_name: &str) -> Option<Recorded> {
        self.learn();
        let prefix = format!("{crate_name}-");
        self.cmds
            .values()
            .find(|c| {
                c.linked
                    && (c.key == crate_name || c.key.starts_with(&prefix))
                    && c.argv.iter().any(|a| a == "bin")
            })
            .map(Recorded::from)
    }

    /// The command that links the client module
    pub fn recorded_root(&mut self) -> Option<Recorded> {
        self.learn();
        self.cmds
            .values()
            .find(|c| c.linked && c.argv.iter().any(|a| a == "cdylib"))
            .map(Recorded::from)
    }

    /// A cargo run starts: its artifact messages name every unit of the build graph
    pub fn begin(&mut self) {
        self.current.clear();
        self.complete = false;
    }

    pub fn complete(&mut self) {
        self.complete = true;
        if self.stamp != 0 {
            let _ = std::fs::write(self.log.with_extension("stamp"), self.stamp.to_string());
        }
    }

    pub fn note(&mut self, a: &Artifact) {
        if let Some(stem) = a.files.first().and_then(|f| f.file_stem()).and_then(|s| s.to_str()) {
            self.current.insert(stem.strip_prefix("lib").unwrap_or(stem).to_string());
        }
        if a.executable.is_some() || a.files.iter().any(|f| f.extension().is_some_and(|e| e == "wasm"))
        {
            self.finals.retain(|f| f.name != a.name);
            self.finals.push(a.clone());
        }
    }

    fn learn(&mut self) {
        let Ok(mut f) = std::fs::File::open(&self.log) else { return };
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return;
        }
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() {
            return;
        }
        self.pos += text.len() as u64;
        for line in text.lines() {
            if let Some(c) = Cmd::parse(line) {
                self.cmds.insert(c.key.clone(), c);
            }
        }
    }

    fn forget(&mut self) {
        self.cmds.clear();
        self.pos = std::fs::metadata(&self.log).map(|m| m.len()).unwrap_or(0);
    }

    /// Units whose sources are newer than their output, or `None` when that can't be told
    fn stale(&mut self) -> Option<HashSet<String>> {
        let mut dirs = HashSet::new();
        for c in self.cmds.values() {
            let mut p = c.out_dir.as_path();
            while let Some(parent) = p.parent() {
                if matches!(p.file_name().and_then(|n| n.to_str()), Some("build" | "deps")) {
                    dirs.insert(parent.to_path_buf());
                    break;
                }
                p = parent;
            }
        }
        let cwd = self.cmds.values().next()?.cwd.clone();
        let mut stale = HashSet::new();
        let mut seen = HashSet::new();
        for profile in dirs {
            let mut files = Vec::new();
            if let Ok(rd) = std::fs::read_dir(profile.join("deps")) {
                files.extend(rd.flatten().map(|e| e.path()));
            }
            if let Ok(pkgs) = std::fs::read_dir(profile.join("build")) {
                for pkg in pkgs.flatten() {
                    if let Ok(hashes) = std::fs::read_dir(pkg.path()) {
                        for h in hashes.flatten() {
                            if let Ok(rd) = std::fs::read_dir(h.path().join("out")) {
                                files.extend(rd.flatten().map(|e| e.path()));
                            }
                        }
                    }
                }
            }
            for d in files.into_iter().filter(|p| p.extension().is_some_and(|e| e == "d")) {
                let Some(key) = d.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                    continue;
                };
                if key.starts_with("build_script") || !self.current.contains(&key) {
                    continue;
                }
                let meta = std::fs::metadata(&d).ok()?;
                let stamp = (meta.modified().ok()?, meta.len());
                if self.deps.get(&d).is_none_or(|i| i.stamp != stamp) {
                    self.deps.insert(d.clone(), read_dep_info(&d, &cwd)?);
                }
                seen.insert(d.clone());
                let info = &self.deps[&d];
                if info.external {
                    continue;
                }
                let out = info.targets.iter().filter_map(|t| mtime(t)).min();
                let missing = info.targets.iter().any(|t| !t.exists());
                let src: Option<Vec<SystemTime>> =
                    info.sources.iter().map(|s| mtime(s)).collect();
                let is_stale = match (out, src) {
                    (Some(out), Some(src)) if !missing => src.iter().any(|s| *s > out),
                    _ => true,
                };
                if is_stale {
                    stale.insert(key);
                }
            }
        }
        self.deps.retain(|k, _| seen.contains(k));
        Some(stale)
    }

    /// Rebuilds what an edit touched by running rustc directly. `None` sends the build to cargo
    pub fn try_run(&mut self, kill: &KillGroup, on: &mut dyn FnMut(&Parsed)) -> Option<CargoRun> {

        let start = Instant::now();
        let stamp = manifest_stamp(&self.root);
        if stamp != self.stamp {
            self.stamp = stamp;
            self.forget();
            return None;
        }
        self.learn();
        if !self.complete || self.finals.is_empty() || self.cmds.is_empty() {
            return None;
        }
        let stale = self.stale()?;
        if stale.is_empty() || stale.iter().any(|k| !self.cmds.contains_key(k)) {
            return None;
        }
        let mut dirty = stale;
        loop {
            let before = dirty.len();
            for c in self.cmds.values() {
                if c.externs.iter().any(|e| dirty.contains(e)) {
                    dirty.insert(c.key.clone());
                }
            }
            if dirty.len() == before {
                break;
            }
        }
        let mut order: Vec<String> = Vec::new();
        let mut waves: Vec<Vec<String>> = Vec::new();
        let mut left: Vec<&String> = dirty.iter().collect();
        while !left.is_empty() {
            let (ready, rest): (Vec<&String>, Vec<&String>) = left.into_iter().partition(|k| {
                self.cmds[*k].externs.iter().all(|e| !dirty.contains(e) || order.contains(e))
            });
            if ready.is_empty() {
                return None;
            }
            let wave: Vec<String> = ready.into_iter().cloned().collect();
            order.extend(wave.iter().cloned());
            waves.push(wave);
            left = rest;
        }
        let width = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        for wave in &waves {
            for chunk in wave.chunks(width) {
                let outs: Vec<Option<std::process::Output>> = std::thread::scope(|scope| {
                    let jobs: Vec<_> = chunk
                        .iter()
                        .map(|key| {
                            let c = &self.cmds[key];
                            scope.spawn(move || {
                                let mut cmd = Command::new(&c.argv[0]);
                                cmd.args(&c.argv[1..])
                                    .current_dir(&c.cwd)
                                    .env_clear()
                                    .envs(
                                        c.env
                                            .iter()
                                            .filter(|(k, _)| {
                                                !matches!(k.as_str(), "CARGO_MAKEFLAGS" | "MAKEFLAGS" | "MFLAGS")
                                            })
                                            .map(|(k, v)| (k, v)),
                                    )
                                    .stdin(Stdio::null())
                                    .stdout(Stdio::piped())
                                    .stderr(Stdio::piped());
                                own_group(&mut cmd);
                                let child = cmd.spawn().ok()?;
                                let pid = child.id();
                                kill.set(pid);
                                let out = child.wait_with_output();
                                kill.release(pid);
                                out.ok()
                            })
                        })
                        .collect();
                    jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
                });
                if kill.cut() {
                    return Some(CargoRun { ok: false, duration: start.elapsed(), ..Default::default() });
                }
                for out in outs {
                    let out = out?;
                    for line in String::from_utf8_lossy(&out.stderr).lines() {
                        if line.contains("\"$message_type\":\"diagnostic\"")
                            && let Some(p) = parse_json_line(&format!(
                                r#"{{"reason":"compiler-message","message":{line}}}"#
                            ))
                        {
                            on(&p);
                        }
                    }
                    if !out.status.success() {
                        return None;
                    }
                }
            }
        }
        for key in &order {
            let c = &self.cmds[key];
            if !c.linked {
                continue;
            }
            let hit = self.finals.iter().find_map(|a| {
                let dst = a
                    .executable
                    .clone()
                    .or_else(|| a.files.iter().find(|f| f.extension().is_some_and(|e| e == "wasm")).cloned())?;
                let src = c.out_dir.join(dst.file_name()?);
                src.is_file().then_some((src, dst))
            });
            let (src, dst) = hit?;
            let _ = std::fs::remove_file(&dst);
            std::fs::hard_link(&src, &dst).or_else(|_| std::fs::copy(&src, &dst).map(|_| ())).ok()?;
        }
        let mut run = CargoRun { ok: true, duration: start.elapsed(), ..Default::default() };
        for a in &self.finals {
            let mut a = a.clone();
            a.fresh = false;
            on(&Parsed::Artifact(a.clone()));
            run.artifacts.push(a);
        }
        Some(run)
    }
}
