use crate::dev::fetch;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const WASM_OPT_FLAGS: [&str; 6] = [
    "-Oz",
    "--strip-debug",
    "--enable-reference-types",
    "--enable-bulk-memory",
    "--enable-mutable-globals",
    "--enable-nontrapping-float-to-int",
];

const COMPRESSIBLE: [&str; 9] = ["wasm", "js", "css", "html", "svg", "json", "txt", "xml", "map"];
const MIN_SIZE: u64 = 1024;

const BINARYEN: &str = "version_132";

fn on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "wasm-opt.exe" } else { "wasm-opt" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(name)).find(|p| p.is_file())
}

fn platform(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-linux"),
        ("linux", "aarch64") => Some("aarch64-linux"),
        ("macos", "x86_64") => Some("x86_64-macos"),
        ("macos", "aarch64") => Some("arm64-macos"),
        ("windows", "x86_64") => Some("x86_64-windows"),
        ("windows", "aarch64") => Some("arm64-windows"),
        _ => None,
    }
}

fn download(log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let target = platform(os, arch).ok_or_else(|| format!("no binaryen build for {os}/{arch}"))?;
    let dir = fetch::cache_dir("binaryen", BINARYEN)
        .ok_or("cannot find a cache directory (set HOME or XDG_CACHE_HOME)")?;
    let name = format!("binaryen-{BINARYEN}");
    let exe =
        dir.join(&name).join("bin").join(if cfg!(windows) { "wasm-opt.exe" } else { "wasm-opt" });
    if exe.is_file() {
        return Ok(exe);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let archive = format!("{name}-{target}.tar.gz");
    let url =
        format!("https://github.com/WebAssembly/binaryen/releases/download/{BINARYEN}/{archive}");
    let instead = "install binaryen and put wasm-opt on PATH";
    log(&format!(
        "downloading wasm-opt ({BINARYEN}, {target}) from github.com/WebAssembly/binaryen, once"
    ));
    let sums = fetch::curl(&format!("{url}.sha256"), None, instead)?;
    let want = sums
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("{archive}.sha256 is empty"))?;
    let part = dir.join(format!("{archive}.part"));
    fetch::curl(&url, Some(&part), instead)?;
    let got = fetch::sha256(&part)?;
    if got != want {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "checksum mismatch for {archive}: expected {want}, got {got}; the download was discarded"
        ));
    }
    let unpacked = fetch::run_ok(Command::new("tar").arg("-xzf").arg(&part).arg("-C").arg(&dir));
    let _ = std::fs::remove_file(&part);
    unpacked.map_err(|e| format!("cannot unpack {archive} ({e}); {instead}"))?;
    if exe.is_file() { Ok(exe) } else { Err(format!("{archive} has no {}", exe.display())) }
}

fn resolve(log: &dyn Fn(&str)) -> Option<PathBuf> {
    if let Some(p) = on_path() {
        return Some(p);
    }
    match download(log) {
        Ok(p) => Some(p),
        Err(e) => {
            log(&format!("wasm-opt unavailable, skipping the size pass: {e}"));
            None
        }
    }
}

pub fn wasm_opt(wasm: &Path, log: &dyn Fn(&str)) {
    let Some(exe) = resolve(log) else { return };
    run_wasm_opt(&exe, wasm, Some(log));
}

/// `wasm-opt` over the main module of a split build. The chunks come out of the splitter without
/// names, unused imports or unused types, which was nearly all `wasm-opt` found in them (1.8% of their
/// brotli size for ~25 s of build), so they skip it unless YAWN_CHUNK_OPT=1
pub fn wasm_opt_all(files: &[PathBuf], log: &dyn Fn(&str)) {
    let Some(exe) = resolve(log) else { return };
    let all = std::env::var_os("YAWN_CHUNK_OPT").is_some_and(|v| v != "0");
    let files = if all { files } else { &files[..files.len().min(1)] };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    std::thread::scope(|s| {
        for _ in 0..workers.min(files.len()) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(f) = files.get(i) else { break };
                    run_wasm_opt(&exe, f, None);
                }
            });
        }
    });
    log(&format!("wasm-opt {} module(s)", files.len()));
}

fn run_wasm_opt(exe: &Path, wasm: &Path, log: Option<&dyn Fn(&str)>) {
    let log = |m: &str| {
        if let Some(l) = log {
            l(m)
        }
    };
    let before = std::fs::metadata(wasm).map(|m| m.len()).unwrap_or(0);
    let tmp = wasm.with_extension("opt.wasm");
    let status = Command::new(exe)
        .args(WASM_OPT_FLAGS)
        .arg(wasm)
        .arg("-o")
        .arg(&tmp)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => match std::fs::metadata(&tmp).map(|m| m.len()) {
            Ok(after) if after < before && std::fs::rename(&tmp, wasm).is_ok() => {
                log(&format!("wasm-opt {} KB -> {} KB", before / 1024, after / 1024));
            }
            _ => {
                let _ = std::fs::remove_file(&tmp);
            }
        },
        _ => {
            let _ = std::fs::remove_file(&tmp);
            log("wasm-opt failed, keeping the unoptimized wasm");
        }
    }
}

fn compressible(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| COMPRESSIBLE.contains(&e))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), out)?;
        } else if compressible(&entry.path()) {
            out.push(entry.path());
        }
    }
    Ok(())
}

fn brotli(path: &Path) -> std::io::Result<bool> {
    let data = std::fs::read(path)?;
    if (data.len() as u64) < MIN_SIZE {
        return Ok(false);
    }
    let mut packed = Vec::with_capacity(data.len() / 3);
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        size_hint: data.len(),
        ..Default::default()
    };
    brotli::BrotliCompress(&mut data.as_slice(), &mut packed, &params)?;
    if packed.len() >= data.len() {
        return Ok(false);
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".br");
    std::fs::write(name, packed)?;
    Ok(true)
}

pub fn precompress(public: &Path) -> std::io::Result<usize> {
    let mut files = Vec::new();
    walk(public, &mut files)?;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(f) = files.get(i) else { break };
                    if brotli(f).unwrap_or(false) {
                        done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            });
        }
    });
    Ok(done.into_inner())
}

fn dcz(dict: &[u8], data: &[u8]) -> std::io::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let mut out = vec![0x5e, 0x2a, 0x4d, 0x18, 0x20, 0, 0, 0];
    out.extend_from_slice(&Sha256::digest(dict));
    out.append(&mut zstd_prefix(dict, data, 19)?);
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// How many earlier builds of a file are kept to send changes against
const GENERATIONS: usize = 3;

/// Compresses the route chunks and the main module as dictionary-coded files. `chunk.wasm.dcz` is a
/// chunk against the main module (the first visit); `name.from-<sha256>.dcz` is a file against the
/// same file of an earlier build, which a returning browser already holds, so a deploy sends only
/// what changed. `store` keeps the earlier builds between runs
pub fn dictionary_chunks(pkg: &Path, store: &Path) -> std::io::Result<(usize, usize)> {
    use sha2::{Digest, Sha256};
    let main = std::fs::read(pkg.join("app_bg.wasm"))?;
    let hashed =
        |n: &str| n.split('.').nth(1).is_some_and(|h| h.len() == 12 && n.ends_with(".wasm"));
    let (mut whole, mut deltas) = (0, 0);
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(pkg)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if name.starts_with("chunk_") && hashed(&name) {
            files.push((name, path));
        }
    }
    if let Ok(rd) = std::fs::read_dir(pkg) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("app_bg.") && hashed(&n) && !n.ends_with(".br") {
                files.push((n, e.path()));
            }
        }
    }
    let work = |name: String, path: PathBuf| -> std::io::Result<(usize, usize)> {
        let mut counts = (0usize, 0usize);
        let data = std::fs::read(&path)?;
        let is_chunk = name.starts_with("chunk_");
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX);
        let packed = {
            let mut p = path.as_os_str().to_owned();
            p.push(".br");
            size(Path::new(&p))
        };
        if is_chunk {
            let out = dcz(&main, &data)?;
            if (out.len() as u64) < packed.min(data.len() as u64) {
                let mut target = path.as_os_str().to_owned();
                target.push(".dcz");
                std::fs::write(target, out)?;
                counts.0 += 1;
            }
        }
        let logical = if is_chunk {
            name.split('.').next().unwrap_or("").to_string()
        } else {
            "main".to_string()
        };
        let dir = store.join(&logical);
        std::fs::create_dir_all(&dir)?;
        let own = hex(&Sha256::digest(&data));
        let mut old: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy() != format!("{own}.wasm"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        old.sort_by_key(|a| std::cmp::Reverse(a.0));
        for (_, previous) in old.iter().take(GENERATIONS) {
            let Ok(prior) = std::fs::read(previous) else { continue };
            let out = dcz(&prior, &data)?;
            if (out.len() as u64) < packed {
                let target = pkg.join(format!("{name}.from-{}.dcz", hex(&Sha256::digest(&prior))));
                std::fs::write(target, out)?;
                counts.1 += 1;
            }
        }
        std::fs::write(dir.join(format!("{own}.wasm")), &data)?;
        for (_, stale) in old.iter().skip(GENERATIONS) {
            let _ = std::fs::remove_file(stale);
        }
        Ok(counts)
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    let results: Vec<std::io::Result<(usize, usize)>> = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((name, path)) = files.get(i) else { break };
                        out.push(work(name.clone(), path.clone()));
                    }
                    out
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
    });
    for r in results {
        let (w, d) = r?;
        whole += w;
        deltas += d;
    }
    Ok((whole, deltas))
}

fn zstd_prefix(dict: &[u8], data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    let mut cctx = zstd::zstd_safe::CCtx::create();
    let io = |e| std::io::Error::other(format!("zstd: error code {e}"));
    cctx.set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(level)).map_err(io)?;
    cctx.set_parameter(zstd::zstd_safe::CParameter::WindowLog(25)).map_err(io)?;
    cctx.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true)).map_err(io)?;
    cctx.ref_prefix(dict).map_err(io)?;
    let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(data.len()));
    cctx.compress2(&mut out, data).map_err(io)?;
    Ok(out)
}
