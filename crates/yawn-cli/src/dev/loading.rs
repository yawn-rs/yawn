use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const STATUS_PATH: &str = "/__yawn/status";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub done: u32,
    pub total: u32,
    pub label: String,
    /// Set when the last build failed
    pub error: Option<String>,
}

pub struct Loading {
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Loading {
    /// Starts listening on `host:port`
    pub fn start(host: IpAddr, port: u16) -> Option<Loading> {
        Self::serve_on(TcpListener::bind(SocketAddr::new(host, port)).ok()?)
    }

    fn serve_on(listener: TcpListener) -> Option<Loading> {
        listener.set_nonblocking(true).ok()?;
        let status = Arc::new(Mutex::new(Status::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (status.clone(), stop.clone());
        let thread = std::thread::spawn(move || serve(listener, &s, &st));
        Some(Loading { status, stop, thread: Some(thread) })
    }

    pub fn set(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    /// Releases the port; returns once nothing listens any more
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Loading {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn serve(listener: TcpListener, status: &Mutex<Status>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((conn, _)) => answer(conn, status),
            Err(_) => std::thread::sleep(Duration::from_millis(15)),
        }
    }
}

fn answer(mut conn: TcpStream, status: &Mutex<Status>) {
    let _ = conn.set_nonblocking(false);
    let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 2048];
    let n = conn.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    let path = head.split_whitespace().nth(1).unwrap_or("/");
    let (ctype, body) = if path.split('?').next() == Some(STATUS_PATH) {
        ("application/json", status_json(&status.lock().unwrap()))
    } else {
        ("text/html; charset=utf-8", PAGE.replace("__STATUS__", STATUS_PATH))
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(resp.as_bytes());
}

fn status_json(s: &Status) -> String {
    let error = match &s.error {
        Some(e) => format!("\"{}\"", json_escape(e)),
        None => "null".to_string(),
    };
    format!(
        "{{\"yawn\":true,\"done\":{},\"total\":{},\"label\":\"{}\",\"error\":{error}}}",
        s.done,
        s.total,
        json_escape(&s.label)
    )
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

const PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Yawn · building</title>
<style>
:root{color-scheme:light dark;--bg:#fff;--fg:#1c1c1c;--dim:#777;--track:#e4e4e4;--bar:#3d6df2;--err:#b3261e;--errbg:#fdecea}
@media(prefers-color-scheme:dark){:root{--bg:#141414;--fg:#e8e8e8;--dim:#999;--track:#2a2a2a;--bar:#6ea0ff;--err:#ffb4b4;--errbg:#1b0d0d}}
body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,sans-serif}
main{width:min(560px,calc(100vw - 32px))}
h1{font-size:18px;margin:0 0 14px;font-weight:600}
.track{height:6px;border-radius:3px;background:var(--track);overflow:hidden}
.fill{height:100%;background:var(--bar);transition:width .2s}
.fill.wait{width:30%!important;animation:slide 1.1s linear infinite}
@keyframes slide{0%{margin-left:-30%}100%{margin-left:100%}}
p{margin:10px 0 0;color:var(--dim);font:13px ui-monospace,monospace;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
pre{display:none;margin:16px 0 0;padding:12px 14px;border-radius:6px;background:var(--errbg);color:var(--err);font:13px/1.45 ui-monospace,monospace;white-space:pre-wrap;max-height:60vh;overflow:auto}
</style></head><body><main>
<h1 id="t">Building…</h1>
<div class="track"><div class="fill wait" id="f"></div></div>
<p id="n"></p>
<pre id="e"></pre>
</main><script>
var f=document.getElementById('f'),n=document.getElementById('n'),t=document.getElementById('t'),e=document.getElementById('e');
function poll(){
  fetch('__STATUS__',{cache:'no-store'}).then(function(r){return r.text()}).then(function(x){
    var s=null;try{s=JSON.parse(x)}catch(_){}
    if(!s||!s.yawn){location.reload();return}
    if(s.error){t.textContent='Build failed';f.className='fill';f.style.width='100%';e.style.display='block';e.textContent=s.error;n.textContent=''}
    else{
      t.textContent='Building…';e.style.display='none';
      if(s.total){f.className='fill';f.style.width=Math.min(100,Math.round(s.done*100/s.total))+'%'}
      else{f.className='fill wait'}
      n.textContent=(s.total?s.done+'/'+s.total+' · ':'')+s.label;
    }
    setTimeout(poll,250);
  }).catch(function(){setTimeout(poll,250)});
}
poll();
</script></body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn get(addr: SocketAddr, path: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nhost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_page_and_status_then_releases_the_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let l = Loading::serve_on(listener).expect("listener accepts non-blocking mode");
        l.set(|s| {
            s.done = 3;
            s.total = 9;
            s.label = "client: serde \"x\"".into();
        });
        assert!(get(addr, "/").contains("<title>Yawn"));
        let status = get(addr, "/__yawn/status?t=1");
        assert!(
            status.contains("\"yawn\":true")
                && status.contains("\"done\":3")
                && status.contains("\\\"x\\\""),
            "{status}"
        );
        l.set(|s| s.error = Some("line1\nline2".into()));
        assert!(get(addr, STATUS_PATH).contains("\"error\":\"line1\\nline2\""));
        l.stop();
        assert!(TcpListener::bind(addr).is_ok(), "port released");
    }

    #[test]
    fn busy_port_gives_none() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = held.local_addr().unwrap();
        assert!(Loading::start(addr.ip(), addr.port()).is_none());
    }
}
