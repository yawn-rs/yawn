use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use yawn::assets::{MANIFEST_FILE, Manifest, content_hash, hashed_name};

/// What to hash and where it comes from
pub struct Inputs<'a> {
    /// The project's assets directory, served at `/assets`
    pub assets: &'a Path,
    /// Compiled client and generated CSS: `app.js`, `app_bg.wasm`, `tailwind.css`, `rsx.css`
    pub pkg: &'a Path,
    /// Where the result goes (`public`)
    pub out: &'a Path,
}

struct Entry {
    url: String,
    source: PathBuf,
}

fn walk(dir: &Path, base: &str, out: &mut Vec<Entry>) -> std::io::Result<()> {
    let mut paths: Vec<_> = fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().unwrap_or_default().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let url = format!("{base}/{name}");
        if p.is_dir() {
            walk(&p, &url, out)?;
        } else {
            out.push(Entry { url, source: p });
        }
    }
    Ok(())
}

/// `url(...)` references in `css` (which is served at `css_url`) replaced by their hashed names
pub fn rewrite_css(css: &str, css_url: &str, manifest: &Manifest) -> String {
    let dir = css_url.rsplit_once('/').map_or("", |(d, _)| d);
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(at) = rest.find("url(") {
        out.push_str(&rest[..at + 4]);
        let after = &rest[at + 4..];
        let Some(end) = after.find(')') else {
            rest = after;
            break;
        };
        let inner = &after[..end];
        let trimmed = inner.trim();
        let (quote, value) = match trimmed.chars().next() {
            Some(q @ ('"' | '\'')) if trimmed.len() >= 2 && trimmed.ends_with(q) => {
                (Some(q), &trimmed[1..trimmed.len() - 1])
            }
            _ => (None, trimmed),
        };
        let served = resolve_reference(value, dir).map(|u| manifest.resolve(&u)).filter(|served| {
            manifest.iter().any(|(_, v)| served.split(['?', '#']).next() == Some(v))
        });
        match served {
            Some(served) => match quote {
                Some(q) => out.push_str(&format!("{q}{served}{q}")),
                None => out.push_str(&served),
            },
            None => out.push_str(inner),
        }
        out.push(')');
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn resolve_reference(value: &str, dir: &str) -> Option<String> {
    if value.is_empty() || value.starts_with('#') || value.starts_with("//") || value.contains(':')
    {
        return None;
    }
    if value.starts_with('/') {
        return Some(value.to_string());
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in value.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

fn is_css(url: &str) -> bool {
    url.split(['?', '#']).next().is_some_and(|p| p.ends_with(".css"))
}

fn write(out: &Path, url: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = out.join(url.trim_start_matches('/'));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

/// Hashes everything and writes `out`; returns the manifest
pub fn run(inputs: &Inputs) -> std::io::Result<Manifest> {
    if inputs.out.exists() {
        fs::remove_dir_all(inputs.out)?;
    }
    fs::create_dir_all(inputs.out)?;
    let mut assets = Vec::new();
    if inputs.assets.is_dir() {
        walk(inputs.assets, "/assets", &mut assets)?;
    }
    let mut manifest = Manifest::new();

    // 1. everything that doesn't refer to anything else
    let (css, leaves): (Vec<&Entry>, Vec<&Entry>) = assets.iter().partition(|e| is_css(&e.url));
    for e in &leaves {
        let bytes = fs::read(&e.source)?;
        let served = hashed_name(&e.url, &content_hash(&bytes));
        write(inputs.out, &served, &bytes)?;
        write(inputs.out, &e.url, &bytes)?;
        manifest.insert(e.url.clone(), served);
    }
    let wasm = inputs.pkg.join("app_bg.wasm");
    if wasm.is_file() {
        let bytes = fs::read(&wasm)?;
        let served = hashed_name("/pkg/app_bg.wasm", &content_hash(&bytes));
        write(inputs.out, &served, &bytes)?;
        write(inputs.out, "/pkg/app_bg.wasm", &bytes)?;
        manifest.insert("/pkg/app_bg.wasm", served);
    }

    let mut chunk_files: Vec<String> = Vec::new();
    if inputs.pkg.is_dir() {
        let mut names: Vec<String> = fs::read_dir(inputs.pkg)?
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("chunk_") && n.ends_with(".wasm"))
            .collect();
        names.sort();
        for name in names {
            let bytes = fs::read(inputs.pkg.join(&name))?;
            let url = format!("/pkg/{name}");
            let served = hashed_name(&url, &content_hash(&bytes));
            write(inputs.out, &served, &bytes)?;
            write(inputs.out, &url, &bytes)?;
            manifest.insert(url, served);
            chunk_files.push(name);
        }
    }

    // 2. stylesheets: their references are known now
    let mut sheets: Vec<(String, PathBuf)> =
        css.iter().map(|e| (e.url.clone(), e.source.clone())).collect();
    for name in ["tailwind.css", "rsx.css"] {
        let p = inputs.pkg.join(name);
        if p.is_file() {
            sheets.push((format!("/pkg/{name}"), p));
        }
    }
    let mut done: BTreeMap<String, String> = BTreeMap::new();
    for (url, source) in &sheets {
        let text = fs::read_to_string(source)?;
        let text = rewrite_css(&text, url, &manifest);
        let served = hashed_name(url, &content_hash(text.as_bytes()));
        write(inputs.out, &served, text.as_bytes())?;
        write(inputs.out, url, fs::read(source)?.as_slice())?;
        done.insert(url.clone(), served);
    }
    for (url, served) in done {
        manifest.insert(url, served);
    }

    // 3. app.js names the module, so it comes last
    let js = inputs.pkg.join("app.js");
    if js.is_file() {
        let mut text = fs::read_to_string(&js)?;
        if let Some(served) = manifest.get("/pkg/app_bg.wasm") {
            let file = served.rsplit('/').next().unwrap_or(served);
            text = text.replace("app_bg.wasm", file);
        }
        for name in &chunk_files {
            if let Some(served) = manifest.get(&format!("/pkg/{name}")) {
                let file = served.rsplit('/').next().unwrap_or(served);
                text = text.replace(&format!("\"{name}\""), &format!("\"{file}\""));
            }
        }
        let served = hashed_name("/pkg/app.js", &content_hash(text.as_bytes()));
        write(inputs.out, &served, text.as_bytes())?;
        write(inputs.out, "/pkg/app.js", fs::read(&js)?.as_slice())?;
        manifest.insert("/pkg/app.js", served);
    }
    // anything else the bundler wrote (JS snippets) is served under its own name
    if inputs.pkg.is_dir() {
        let mut rest = Vec::new();
        walk(inputs.pkg, "/pkg", &mut rest)?;
        for e in rest {
            let known = matches!(
                e.url.as_str(),
                "/pkg/app.js" | "/pkg/app_bg.wasm" | "/pkg/tailwind.css" | "/pkg/rsx.css"
            ) || e.url.starts_with("/pkg/chunk_");
            if !known {
                write(inputs.out, &e.url, &fs::read(&e.source)?)?;
            }
        }
    }
    fs::write(inputs.out.join(MANIFEST_FILE), manifest.to_json())?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        let mut m = Manifest::new();
        m.insert("/assets/img/a.png", "/assets/img/a.111111111111.png");
        m.insert("/assets/fonts/f.woff2", "/assets/fonts/f.222222222222.woff2");
        m
    }

    #[test]
    fn css_urls_become_hashed_names() {
        let css = "a{background:url(/assets/img/a.png)}b{background:url( '../img/a.png?v=2' )}c{src:url(\"f.woff2\") format(\"woff2\")}d{background:url(data:image/png;base64,AAA)}e{background:url(https://x/y.png)}f{background:url(/assets/none.png)}g{mask:url(#m)}";
        let out = rewrite_css(css, "/assets/css/site.css", &manifest());
        assert!(out.contains("url(/assets/img/a.111111111111.png)"), "{out}");
        assert!(
            out.contains("url( '/assets/img/a.111111111111.png?v=2' )")
                || out.contains("'/assets/img/a.111111111111.png?v=2'"),
            "{out}"
        );
        assert!(
            out.contains("url(data:image/png;base64,AAA)")
                && out.contains("url(https://x/y.png)")
                && out.contains("url(/assets/none.png)")
                && out.contains("url(#m)"),
            "{out}"
        );
        // `f.woff2` is relative to /assets/css/, where it doesn't exist: left alone
        assert!(out.contains("url(\"f.woff2\")"), "{out}");
        let fonts = rewrite_css("@font-face{src:url(f.woff2)}", "/assets/fonts/x.css", &manifest());
        assert!(fonts.contains("url(/assets/fonts/f.222222222222.woff2)"), "{fonts}");
    }

    #[test]
    fn builds_hashed_files_manifest_and_fallbacks() {
        let dir = std::env::temp_dir().join(format!("yawn-fin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (assets, pkg, out) = (dir.join("assets"), dir.join("pkg"), dir.join("public"));
        fs::create_dir_all(assets.join("img")).unwrap();
        fs::create_dir_all(&pkg).unwrap();
        fs::write(assets.join("img/a.png"), b"PNG-1").unwrap();
        fs::write(assets.join("site.css"), "a{background:url(/assets/img/a.png)}").unwrap();
        fs::write(pkg.join("app_bg.wasm"), b"\0asm-1").unwrap();
        fs::write(pkg.join("app.js"), "const u = new URL('app_bg.wasm', import.meta.url);")
            .unwrap();
        fs::write(pkg.join("rsx.css"), "p{background:url(/assets/img/a.png)}").unwrap();
        let m = run(&Inputs { assets: &assets, pkg: &pkg, out: &out }).unwrap();

        let png = m.get("/assets/img/a.png").unwrap().to_string();
        assert!(yawn::assets::is_hashed(&png), "{png}");
        assert_eq!(fs::read(out.join(png.trim_start_matches('/'))).unwrap(), b"PNG-1");
        assert_eq!(
            fs::read(out.join("assets/img/a.png")).unwrap(),
            b"PNG-1",
            "the written name still works"
        );
        let css = fs::read_to_string(
            out.join(m.get("/assets/site.css").unwrap().trim_start_matches('/')),
        )
        .unwrap();
        assert!(css.contains(&png), "the stylesheet points at the hashed image: {css}");
        let js =
            fs::read_to_string(out.join(m.get("/pkg/app.js").unwrap().trim_start_matches('/')))
                .unwrap();
        let wasm = m.get("/pkg/app_bg.wasm").unwrap();
        assert!(
            js.contains(wasm.rsplit('/').next().unwrap()) && !js.contains("'app_bg.wasm'"),
            "{js}"
        );
        assert!(
            Manifest::parse(&fs::read_to_string(out.join(MANIFEST_FILE)).unwrap()).unwrap() == m
        );

        // changing the image changes its name, and the stylesheet's, and nothing else's
        fs::write(assets.join("img/a.png"), b"PNG-2").unwrap();
        let m2 = run(&Inputs { assets: &assets, pkg: &pkg, out: &out }).unwrap();
        assert_ne!(m2.get("/assets/img/a.png"), m.get("/assets/img/a.png"));
        assert_ne!(
            m2.get("/assets/site.css"),
            m.get("/assets/site.css"),
            "its content changed with the reference"
        );
        assert_eq!(m2.get("/pkg/app.js"), m.get("/pkg/app.js"), "unrelated files keep their names");
        let _ = fs::remove_dir_all(&dir);
    }
}
