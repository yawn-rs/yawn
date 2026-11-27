use std::path::{Path, PathBuf};

const LOADER: &str = r#"
;(() => {
  const ROUTES = __ROUTES__;
  const FILES = __FILES__;
  const started = new Map();
  const done = new Set();
  const chunk = (i) => {
    if (!started.has(i)) {
      started.set(i, (async () => {
        const main = globalThis.__yawnMain;
        const imports = Object.assign({}, globalThis.__yawnImports, {
          main: new Proxy({}, { get: (_, name) => main[name] }),
        });
        const res = await fetch(new URL(FILES[i], import.meta.url));
        if (!res.ok) throw new Error("failed to fetch " + FILES[i]);
        let out;
        try {
          out = await WebAssembly.instantiateStreaming(res.clone(), imports);
        } catch (_) {
          out = await WebAssembly.instantiate(await res.arrayBuffer(), imports);
        }
        done.add(i);
        return out;
      })());
    }
    return started.get(i);
  };
  globalThis.__yawnSplit = {
    ready: (key) => !(key in ROUTES) || ROUTES[key].every((i) => done.has(i)),
    load: (key) => Promise.all((ROUTES[key] || []).map(chunk)),
  };
  globalThis.__yawnPrepare = async () => {
    const meta = document.querySelector('meta[name="yawn-routes"]');
    if (meta && meta.content) {
      await Promise.all(meta.content.split("|").map((k) => globalThis.__yawnSplit.load(k)));
    }
  };
})();
"#;

/// The module without its `name` section, which is over half of a dev build and only a debugger reads
fn without_names(wasm: &[u8]) -> Vec<u8> {
    fn leb(d: &[u8], i: &mut usize) -> Option<usize> {
        let (mut r, mut s) = (0usize, 0);
        loop {
            let b = *d.get(*i)?;
            *i += 1;
            r |= ((b & 0x7f) as usize) << s;
            if b & 0x80 == 0 {
                return Some(r);
            }
            s += 7;
        }
    }
    let Some(head) = wasm.get(..8) else { return wasm.to_vec() };
    let mut out = head.to_vec();
    let mut i = 8;
    while i < wasm.len() {
        let start = i;
        let id = wasm[i];
        i += 1;
        let Some(size) = leb(wasm, &mut i) else { return wasm.to_vec() };
        let body = i;
        i += size;
        if i > wasm.len() {
            return wasm.to_vec();
        }
        let mut j = body;
        let named = id == 0
            && leb(wasm, &mut j).is_some_and(|n| wasm.get(j..j + n) == Some(b"name".as_slice()));
        if !named {
            out.extend_from_slice(&wasm[start..i]);
        }
    }
    out
}

fn js_string(s: &str) -> String {
    format!("{s:?}")
}

fn patch(js: &str, routes: &[(String, Vec<usize>)], files: &[String]) -> Result<String, String> {
    let init_at = js.find("async function __wbg_init").ok_or("app.js has no __wbg_init")?;
    let (head, tail) = js.split_at(init_at);
    let needle = "const imports = __wbg_get_imports();";
    if !head.contains(needle) || !tail.contains(needle) {
        return Err("app.js builds its imports in a way the splitter does not know".into());
    }
    let capture = "const imports = (globalThis.__yawnImports = __wbg_get_imports());";
    let head = head.replace(needle, capture);
    let tail = tail.replace(needle, capture);
    let finish = "return __wbg_finalize_init(instance, module);";
    if !tail.contains(finish) {
        return Err("app.js finishes its init in a way the splitter does not know".into());
    }
    let tail = tail.replacen(
        finish,
        "globalThis.__yawnMain = instance.exports;\n    if (globalThis.__yawnPrepare) await globalThis.__yawnPrepare();\n    return __wbg_finalize_init(instance, module);",
        1,
    );
    let mut table = String::from("{");
    for (i, (key, chunks)) in routes.iter().enumerate() {
        if i > 0 {
            table.push(',');
        }
        let list: Vec<String> = chunks.iter().map(|c| c.to_string()).collect();
        table.push_str(&format!("{}:[{}]", js_string(key), list.join(",")));
    }
    table.push('}');
    let names: Vec<String> = files.iter().map(|f| js_string(f)).collect();
    let loader = LOADER
        .replace("__ROUTES__", &table)
        .replace("__FILES__", &format!("[{}]", names.join(",")));
    Ok(format!("{head}{tail}\n{loader}"))
}

/// Cuts the route code out of `pkg/app_bg.wasm` into `pkg/chunk_N.wasm` modules and teaches
/// `pkg/app.js` to load them. `linked` is the wasm as the linker wrote it (with relocations).
/// Returns false, leaving everything as it was, when the build cannot be split
pub fn apply(pkg: &Path, linked: &Path, log: &dyn Fn(&str), dev: bool) -> bool {
    if std::env::var_os("YAWN_NO_SPLIT").is_some() {
        return false;
    }
    let attempt = || -> Result<(), String> {
        let original = std::fs::read(linked).map_err(|e| e.to_string())?;
        let bindgened = std::fs::read(pkg.join("app_bg.wasm")).map_err(|e| e.to_string())?;
        let js_path = pkg.join("app.js");
        let js = std::fs::read_to_string(&js_path).map_err(|e| e.to_string())?;
        let split = yawn_split::split(&original, &bindgened)?;
        if dev && let Ok(rd) = std::fs::read_dir(pkg) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("chunk_") && n.ends_with(".wasm") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let files: Vec<String> = split
            .chunks
            .iter()
            .enumerate()
            .map(|(i, bytes)| {
                if dev {
                    let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| {
                        (h ^ *b as u64).wrapping_mul(0x100000001b3)
                    });
                    format!("chunk_{i}_{hash:016x}.wasm")
                } else {
                    format!("chunk_{i}.wasm")
                }
            })
            .collect();
        let patched = patch(&js, &split.routes, &files)?;
        let (main_bytes, chunk_bytes): (Vec<u8>, Vec<Vec<u8>>) = if dev {
            (without_names(&split.main), split.chunks.iter().map(|c| without_names(c)).collect())
        } else {
            (split.main.clone(), split.chunks.iter().map(|c| without_names(c)).collect())
        };
        std::fs::write(pkg.join("app_bg.wasm"), &main_bytes).map_err(|e| e.to_string())?;
        let mut written: Vec<PathBuf> = vec![pkg.join("app_bg.wasm")];
        for (name, bytes) in files.iter().zip(&chunk_bytes) {
            std::fs::write(pkg.join(name), bytes).map_err(|e| e.to_string())?;
            written.push(pkg.join(name));
        }
        std::fs::write(&js_path, patched).map_err(|e| e.to_string())?;
        log(&format!(
            "split into a main module and {} route modules ({} routes, {} functions not matched; graph {:.1}s, plan {:.1}s, emit {:.1}s)",
            split.chunks.len(),
            split.routes.len(),
            split.unmatched,
            split.seconds[0],
            split.seconds[1],
            split.seconds[2]
        ));
        if !dev {
            crate::optimize::wasm_opt_all(&written, log);
        }
        Ok(())
    };
    match attempt() {
        Ok(()) => true,
        Err(e) => {
            log(&format!("not splitting the wasm: {e}"));
            false
        }
    }
}
