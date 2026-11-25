use crate::event::{Level, Location};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

/// Tracks the leaders of the process groups currently running (cargo, or the dev server) so a
/// shutdown can kill the whole tree, rustc included
#[derive(Default)]
pub struct KillGroup {
    pids: Mutex<Vec<i32>>,
}

impl KillGroup {
    fn pids(&self) -> std::sync::MutexGuard<'_, Vec<i32>> {
        self.pids.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, pid: u32) {
        self.pids().push(pid as i32);
    }

    /// Forgets one group that has ended
    pub fn release(&self, pid: u32) {
        self.pids().retain(|p| *p != pid as i32);
    }

    pub fn clear(&self) {
        self.pids().clear();
    }

    /// SIGKILLs the groups
    pub fn kill(&self) {
        let pids = std::mem::take(&mut *self.pids());
        for pid in pids.into_iter().filter(|p| *p > 0) {
            kill_group(pid);
        }
    }
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    // SAFETY: `killpg` takes only integers and reads no memory of ours; a stale pid returns ESRCH.
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(pid: i32) {
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
}

#[cfg(unix)]
pub fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub fn own_group(_cmd: &mut Command) {}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub location: Option<Location>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub kinds: Vec<String>,
    pub files: Vec<PathBuf>,
    pub executable: Option<PathBuf>,
    /// Cargo found it up to date and didn't recompile it
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Diagnostic(Diagnostic),
    Artifact(Artifact),
    Compiling(String),
    Progress { done: u32, total: u32, running: Vec<String> },
    Other(String),
}

/// Parses one line of `--message-format=json` output
pub fn parse_json_line(line: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(line).ok()?;
    match v.get("reason")?.as_str()? {
        "compiler-message" => {
            let m = v.get("message")?;
            let level = match m.get("level")?.as_str()? {
                "error" | "error: internal compiler error" => Level::Error,
                "warning" => Level::Warn,
                _ => return None,
            };
            let message = m.get("message")?.as_str()?.to_string();
            // Summary lines ("aborting due to ...", "2 warnings emitted") aren't diagnostics.
            if message.starts_with("aborting due to")
                || message.contains(" emitted") && m.get("spans")?.as_array()?.is_empty()
            {
                return None;
            }
            let location = m
                .get("spans")?
                .as_array()?
                .iter()
                .find(|s| s.get("is_primary").and_then(Value::as_bool) == Some(true))
                .and_then(|s| {
                    Some(Location::new(
                        s.get("file_name")?.as_str()?,
                        s.get("line_start")?.as_u64()? as u32,
                        s.get("column_start")?.as_u64()? as u32,
                    ))
                });
            let code = m
                .get("code")
                .and_then(|c| c.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Parsed::Diagnostic(Diagnostic { level, message, location, code }))
        }
        "compiler-artifact" => {
            let t = v.get("target")?;
            let strings = |x: &Value| {
                x.as_array().map(|a| {
                    a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>()
                })
            };
            Some(Parsed::Artifact(Artifact {
                name: t.get("name")?.as_str()?.to_string(),
                kinds: strings(t.get("kind")?)?,
                files: strings(v.get("filenames")?)?.into_iter().map(PathBuf::from).collect(),
                executable: v.get("executable").and_then(Value::as_str).map(PathBuf::from),
                fresh: v.get("fresh").and_then(Value::as_bool).unwrap_or(false),
            }))
        }
        _ => None,
    }
}

/// Classifies one line of cargo's human-readable stderr
pub fn parse_stderr_line(line: &str) -> Option<Parsed> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(p) = parse_progress(t) {
        return Some(p);
    }
    if let Some(rest) = t.strip_prefix("Compiling ") {
        return Some(Parsed::Compiling(rest.split_whitespace().next().unwrap_or(rest).to_string()));
    }
    for quiet in
        ["Finished ", "Fresh ", "Running ", "Downloaded ", "Downloading ", "Locking ", "Adding "]
    {
        if t.starts_with(quiet) {
            return None;
        }
    }
    Some(Parsed::Other(t.to_string()))
}

fn parse_progress(t: &str) -> Option<Parsed> {
    let rest = t.strip_prefix("Building [")?;
    let (_, rest) = rest.split_once("] ")?;
    let (counts, names) = rest.split_once(':').unwrap_or((rest, ""));
    let (done, total) = counts.trim().split_once('/')?;
    let (done, total) = (done.trim().parse().ok()?, total.trim().parse().ok()?);
    let truncated = names.trim_end().ends_with('…');
    let mut running: Vec<String> = names
        .split(',')
        .map(|n| n.trim().trim_end_matches('…').to_string())
        .filter(|n| !n.is_empty())
        .collect();
    if truncated {
        running.pop();
    }
    Some(Parsed::Progress { done, total, running })
}

pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn for_each_piece(mut reader: impl std::io::Read, mut f: impl FnMut(String) -> bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' || b == b'\r' {
                if !buf.is_empty() {
                    let line = strip_ansi(&String::from_utf8_lossy(&buf));
                    buf.clear();
                    if !f(line) {
                        return;
                    }
                }
            } else {
                buf.push(b);
            }
        }
    }
    if !buf.is_empty() {
        f(strip_ansi(&String::from_utf8_lossy(&buf)));
    }
}

#[derive(Debug, Default)]
pub struct CargoRun {
    pub ok: bool,
    pub artifacts: Vec<Artifact>,
    pub errors: u32,
    pub warnings: u32,
    /// Non-diagnostic stderr lines (useful when cargo itself fails)
    pub other: Vec<String>,
    pub duration: Duration,
}

/// Runs `cargo <args> --message-format=json` in `cwd`, feeding every parsed line to `on` as it
/// arrives
pub fn run(
    args: &[&str],
    cwd: &Path,
    kill: &KillGroup,
    mut on: impl FnMut(&Parsed),
) -> std::io::Result<CargoRun> {
    let start = Instant::now();
    let mut cmd = Command::new("cargo");
    cmd.args(args)
        .arg("--message-format=json")
        .current_dir(cwd)
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "always")
        .env("CARGO_TERM_PROGRESS_WIDTH", "400")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let mut child =
        cmd.spawn().map_err(|e| std::io::Error::new(e.kind(), format!("cannot run cargo: {e}")))?;
    kill.set(child.id());
    let (tx, rx) = mpsc::channel::<Parsed>();
    let out = child.stdout.take().expect("piped");
    let err = child.stderr.take().expect("piped");
    let tx_out = tx.clone();
    let t1 = std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some(p) = parse_json_line(&line)
                && tx_out.send(p).is_err()
            {
                break;
            }
        }
    });
    let t2 = std::thread::spawn(move || {
        for_each_piece(err, |line| match parse_stderr_line(&line) {
            Some(p) => tx.send(p).is_ok(),
            None => true,
        });
    });
    let mut run = CargoRun::default();
    for p in rx {
        on(&p);
        match p {
            Parsed::Diagnostic(d) => match d.level {
                Level::Error => run.errors += 1,
                _ => run.warnings += 1,
            },
            Parsed::Artifact(a) => run.artifacts.push(a),
            Parsed::Other(l) => run.other.push(l),
            Parsed::Compiling(_) | Parsed::Progress { .. } => {}
        }
    }
    let _ = t1.join();
    let _ = t2.join();
    let status = child.wait();
    kill.release(child.id());
    let status = status?;
    run.ok = status.success();
    run.duration = start.elapsed();
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_error_with_location_and_code() {
        let line = r#"{"reason":"compiler-message","package_id":"x","message":{"message":"mismatched types","code":{"code":"E0308","explanation":null},"level":"error","spans":[{"file_name":"src/lib.rs","line_start":12,"column_start":5,"is_primary":true},{"file_name":"src/other.rs","line_start":1,"column_start":1,"is_primary":false}],"rendered":"..."}}"#;
        let Some(Parsed::Diagnostic(d)) = parse_json_line(line) else { panic!() };
        assert_eq!(d.level, Level::Error);
        assert_eq!(d.message, "mismatched types");
        assert_eq!(d.code.as_deref(), Some("E0308"));
        assert_eq!(d.location.unwrap().to_string(), "src/lib.rs:12:5");
    }

    #[test]
    fn skips_summaries_notes_and_noise() {
        let msg = |level: &str, text: &str| {
            format!(
                r#"{{"reason":"compiler-message","message":{{"message":"{text}","code":null,"level":"{level}","spans":[]}}}}"#
            )
        };
        assert_eq!(parse_json_line(&msg("error", "aborting due to 1 previous error")), None);
        assert_eq!(parse_json_line(&msg("warning", "2 warnings emitted")), None);
        assert_eq!(parse_json_line(&msg("note", "something")), None);
        assert!(matches!(
            parse_json_line(&msg("warning", "unused variable")),
            Some(Parsed::Diagnostic(_))
        ));
        assert_eq!(parse_json_line("not json"), None);
        assert_eq!(parse_json_line(r#"{"reason":"build-finished","success":true}"#), None);
    }

    #[test]
    fn parses_artifacts() {
        let line = r#"{"reason":"compiler-artifact","package_id":"p","target":{"kind":["cdylib","rlib"],"name":"counter"},"filenames":["/t/counter.wasm"],"executable":null,"fresh":true}"#;
        let Some(Parsed::Artifact(a)) = parse_json_line(line) else { panic!() };
        assert_eq!((a.name.as_str(), a.fresh, a.executable.is_none()), ("counter", true, true));
        assert_eq!(a.files, vec![PathBuf::from("/t/counter.wasm")]);
        let bin = r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"counter"},"filenames":["/t/counter"],"executable":"/t/counter","fresh":false}"#;
        let Some(Parsed::Artifact(b)) = parse_json_line(bin) else { panic!() };
        assert_eq!(b.executable, Some(PathBuf::from("/t/counter")));
    }

    #[test]
    fn progress_lines() {
        let p = parse_stderr_line(
            "    Building [=====>                    ] 12/80: serde, tokio(build.rs), axum",
        );
        assert_eq!(
            p,
            Some(Parsed::Progress {
                done: 12,
                total: 80,
                running: vec!["serde".into(), "tokio(build.rs)".into(), "axum".into()]
            })
        );
        // a name cut by the width isn't a name
        let cut = parse_stderr_line("    Building [>   ] 1/9: a, b, cde…");
        assert_eq!(
            cut,
            Some(Parsed::Progress { done: 1, total: 9, running: vec!["a".into(), "b".into()] })
        );
        assert_eq!(
            parse_stderr_line("Building [===] 9/9:"),
            Some(Parsed::Progress { done: 9, total: 9, running: vec![] })
        );
        assert_eq!(
            parse_stderr_line("Building [===] x/9: a"),
            Some(Parsed::Other("Building [===] x/9: a".into()))
        );
    }

    #[test]
    fn pieces_split_on_carriage_returns_and_drop_escapes() {
        let mut got = Vec::new();
        for_each_piece(
            "   Compiling a v1\n    Building [>] 1/2: a\r\u{1b}[K    Building [=>] 2/2: b\r\n"
                .as_bytes(),
            |l| {
                got.push(l);
                true
            },
        );
        assert_eq!(
            got,
            ["   Compiling a v1", "    Building [>] 1/2: a", "    Building [=>] 2/2: b"]
        );
    }

    #[test]
    fn stderr_lines() {
        assert_eq!(
            parse_stderr_line("   Compiling axum v0.8.9"),
            Some(Parsed::Compiling("axum".into()))
        );
        assert_eq!(parse_stderr_line("    Finished `dev` profile in 1s"), None);
        assert_eq!(parse_stderr_line(""), None);
        assert!(matches!(
            parse_stderr_line("error: could not find `Cargo.toml`"),
            Some(Parsed::Other(_))
        ));
    }

    #[test]
    fn missing_cargo_directory_is_a_failed_run_not_a_panic() {
        let kill = KillGroup::default();
        let r = run(&["build"], Path::new("/definitely/not/here"), &kill, |_| {});
        assert!(r.is_err() || !r.unwrap().ok);
    }
}
