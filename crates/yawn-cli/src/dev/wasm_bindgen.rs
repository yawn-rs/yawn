//! The `wasm-bindgen` the project needs: it must be the same version as the `wasm-bindgen` crate in
//! its `Cargo.lock`. The version built into Yawn when it matches (it runs in this process, and the
//! prebuilt Linux CLI is a musl binary whose allocator makes it about three times slower), else the
//! CLI on `PATH` when that matches, else that release downloaded once into the cache.

use super::fetch;
use std::path::{Path, PathBuf};
use std::process::Command;
use yawn::config;

const EXE: &str = if cfg!(windows) { "wasm-bindgen.exe" } else { "wasm-bindgen" };

/// Target triple of the release archive for a platform
pub fn target_name(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        // the only aarch64 Linux build in older releases
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// Version from `wasm-bindgen --version` output (`wasm-bindgen 0.2.129`)
pub fn parse_version(out: &str) -> Option<&str> {
    out.split_whitespace().nth(1)
}

fn install_hint(version: &str) -> String {
    format!("run: cargo install -f wasm-bindgen-cli --version {version}")
}

/// The version of `wasm-bindgen` that is part of Yawn
pub const BUILT_IN: &str = "0.2.129";

/// What runs `wasm-bindgen` for a project
#[derive(Clone)]
pub enum Tool {
    BuiltIn,
    Exe(PathBuf),
}

/// What to ask of `wasm-bindgen`: a `--target web` bundle called `out_name`
pub struct Options<'a> {
    pub out_name: &'a str,
    /// Keep readable function names in the module (`--no-demangle` turns them off)
    pub demangle: bool,
    /// Drop the name section, over half of a dev module
    pub remove_names: bool,
    /// Keep the exports the linker added (the function table), which route modules loaded later
    /// need
    pub keep_linker_exports: bool,
}

/// The tool for the project at `root`
pub fn tool(root: &Path, log: &dyn Fn(&str)) -> Result<Tool, String> {
    if config::locked_version(root, "wasm-bindgen").as_deref() == Some(BUILT_IN) {
        return Ok(Tool::BuiltIn);
    }
    resolve(root, log).map(Tool::Exe)
}

/// Writes `<out_name>.js` and `<out_name>_bg.wasm` into `out`
pub fn run(tool: &Tool, wasm: &Path, out: &Path, opts: &Options) -> Result<(), String> {
    match tool {
        Tool::BuiltIn => {
            let (wasm, out) = (wasm.to_path_buf(), out.to_path_buf());
            let (name, demangle, remove) = (opts.out_name.to_string(), opts.demangle, opts.remove_names);
            let keep = opts.keep_linker_exports;
            // a bug in the library must not take the whole `yawn dev` down
            std::panic::catch_unwind(move || -> Result<(), String> {
                let mut b = wasm_bindgen_cli_support::Bindgen::new();
                b.input_path(&wasm)
                    .typescript(false)
                    .demangle(demangle)
                    // the CLI's default: `init()` without an argument finds `<name>_bg.wasm`
                    .omit_default_module_path(false)
                    .remove_name_section(remove)
                    .keep_lld_exports(keep)
                    .out_name(&name);
                b.web(true).map_err(|e| e.to_string())?;
                b.generate(&out).map_err(|e| format!("{e:#}"))
            })
            .map_err(|_| "wasm-bindgen panicked".to_string())?
            .map_err(|e| format!("wasm-bindgen failed: {}", e.lines().next().unwrap_or("unknown error")))
        }
        Tool::Exe(exe) => {
            let mut cmd = Command::new(exe);
            cmd.args(["--target", "web", "--no-typescript", "--out-name", opts.out_name, "--out-dir"])
                .arg(out)
                .arg(wasm)
                .stdin(std::process::Stdio::null());
            if !opts.demangle {
                cmd.arg("--no-demangle");
            }
            if opts.remove_names {
                cmd.arg("--remove-name-section");
            }
            if opts.keep_linker_exports {
                cmd.arg("--keep-lld-exports");
            }
            // The prebuilt CLI is a thread-per-core pass whose extra threads only fight cargo
            if std::env::var_os("RAYON_NUM_THREADS").is_none() {
                cmd.env("RAYON_NUM_THREADS", "2");
            }
            super::cargo::own_group(&mut cmd);
            let output = cmd.output().map_err(|e| format!("cannot run wasm-bindgen: {e}"))?;
            if output.status.success() {
                Ok(())
            } else {
                let text = String::from_utf8_lossy(&output.stderr);
                Err(format!("wasm-bindgen failed: {}", text.lines().next().unwrap_or("unknown error")))
            }
        }
    }
}

/// The executable to run for the project at `root`
pub fn resolve(root: &Path, log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let on_path = fetch::run_ok(Command::new(EXE).arg("--version")).ok();
    let have = on_path.as_deref().and_then(parse_version);
    match config::locked_version(root, "wasm-bindgen") {
        Some(want) if have != Some(want.as_str()) => download(&want, log),
        // no lock to match: whatever is installed
        None if have.is_none() => {
            Err("wasm-bindgen not found. Install it: cargo install wasm-bindgen-cli".to_string())
        }
        _ => Ok(PathBuf::from(EXE)),
    }
}

fn download(version: &str, log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let target = target_name(os, arch).ok_or_else(|| {
        format!(
            "no wasm-bindgen {version} release build for {os}/{arch}; {}",
            install_hint(version)
        )
    })?;
    let dir = fetch::cache_dir("wasm-bindgen", version)
        .ok_or("cannot find a cache directory (set HOME or XDG_CACHE_HOME)")?;
    let exe = dir.join(EXE);
    if exe.is_file() {
        return Ok(exe);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = format!("wasm-bindgen-{version}-{target}");
    let url = format!(
        "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/{version}/{name}.tar.gz"
    );
    log(&format!(
        "downloading wasm-bindgen {version} ({target}) from github.com/wasm-bindgen/wasm-bindgen, once"
    ));
    let instead = install_hint(version);
    // `<hex>  gh-release/<archive>`
    let sums = fetch::curl(&format!("{url}.sha256sum"), None, &instead)?;
    let want = sums
        .split_whitespace()
        .next()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("{name}.tar.gz.sha256sum is empty"))?;
    let part = dir.join(format!("{name}.tar.gz.part"));
    fetch::curl(&url, Some(&part), &instead)?;
    let got = fetch::sha256(&part)?;
    if got != want {
        let _ = std::fs::remove_file(&part);
        return Err(format!(
            "checksum mismatch for {name}.tar.gz: expected {want}, got {got}; the download was discarded"
        ));
    }
    let unpacked = fetch::run_ok(Command::new("tar").arg("-xzf").arg(&part).arg("-C").arg(&dir));
    let _ = std::fs::remove_file(&part);
    unpacked.map_err(|e| format!("cannot unpack {name}.tar.gz ({e}); {instead}"))?;
    std::fs::rename(dir.join(&name).join(EXE), &exe)
        .map_err(|e| format!("cannot install {}: {e}", exe.display()))?;
    let _ = std::fs::remove_dir_all(dir.join(&name));
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names() {
        assert_eq!(target_name("linux", "x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(target_name("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_name("windows", "x86_64"), Some("x86_64-pc-windows-msvc"));
        assert_eq!(target_name("windows", "aarch64"), None);
        assert_eq!(target_name("freebsd", "x86_64"), None);
    }

    #[test]
    fn version_output() {
        assert_eq!(parse_version("wasm-bindgen 0.2.129\n"), Some("0.2.129"));
        assert_eq!(parse_version(""), None);
    }

    #[test]
    #[ignore]
    fn real_wasm_bindgen_download() {
        let cache = std::env::temp_dir().join(format!("yawn-wb-{}", std::process::id()));
        // SAFETY: an ignored test run on its own, no other thread reads the environment
        unsafe { std::env::set_var("XDG_CACHE_HOME", &cache) };
        let exe = download("0.2.129", &|m| eprintln!("{m}")).unwrap();
        let out = fetch::run_ok(Command::new(&exe).arg("--version")).unwrap();
        assert_eq!(parse_version(&out), Some("0.2.129"));
        assert_eq!(download("0.2.129", &|_| panic!("downloaded twice")).unwrap(), exe);
        std::fs::remove_dir_all(&cache).unwrap();
    }
}
