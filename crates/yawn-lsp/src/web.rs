use crossbeam_channel::{Receiver, unbounded};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

const SCRIPT: &str = include_str!("../web/dist/web.cjs");

pub struct Web {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next: u64,
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Web {
    pub fn start() -> Result<Web, String> {
        let script =
            write_script().map_err(|e| format!("cannot write the HTML/CSS helper: {e}"))?;
        let mut last = String::from("no `node` or `bun` found");
        for runtime in runtimes() {
            let mut cmd = Command::new(&runtime);
            if runtime.file_name().is_some_and(|n| n == "bun") {
                cmd.arg("run");
            }
            match cmd
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let stdin = child.stdin.take().expect("piped");
                    let out = child.stdout.take().expect("piped");
                    let (tx, rx) = unbounded();
                    std::thread::spawn(move || {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            if let Ok(v) = serde_json::from_str::<Value>(&line)
                                && tx.send(v).is_err()
                            {
                                break;
                            }
                        }
                    });
                    return Ok(Web { child, stdin, rx, next: 0 });
                }
                Err(e) => last = format!("cannot run {}: {e}", runtime.display()),
            }
        }
        Err(last)
    }

    /// One request; `None` on timeout or error (the caller answers without HTML/CSS data)
    pub fn call(
        &mut self,
        op: &str,
        text: &str,
        line: u32,
        character: u32,
        timeout: Duration,
    ) -> Option<Value> {
        self.next += 1;
        let id = self.next;
        let req = json!({"id": id, "op": op, "text": text, "line": line, "character": character});
        writeln!(self.stdin, "{req}").ok()?;
        self.stdin.flush().ok()?;
        loop {
            let v = self.rx.recv_timeout(timeout).ok()?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v.get("result").cloned().filter(|r| !r.is_null());
            }
            // an answer to an earlier request that timed out: drop it
        }
    }
}

fn runtimes() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("YAWN_NODE") {
        v.push(p.into());
    }
    v.push("node".into());
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    // Zed downloads its own Node for extensions.
    if let Ok(rd) = std::fs::read_dir(home.join(".local/share/zed/node")) {
        let mut found: Vec<PathBuf> =
            rd.flatten().map(|e| e.path().join("bin/node")).filter(|p| p.is_file()).collect();
        found.sort();
        v.extend(found.into_iter().rev());
    }
    v.push("bun".into());
    v.push(home.join(".bun/bin/bun"));
    v
}

fn write_script() -> std::io::Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("yawn-lsp");
    std::fs::create_dir_all(&dir)?;
    let hash = SCRIPT
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    let path = dir.join(format!("web-{hash:016x}.cjs"));
    if std::fs::metadata(&path).map(|m| m.len() as usize).ok() != Some(SCRIPT.len()) {
        std::fs::write(&path, SCRIPT)?;
    }
    Ok(path)
}
