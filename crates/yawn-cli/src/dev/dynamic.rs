//! `[dev] dynamic`: a route edit that only changes
//! component bodies is built as a small position-independent side module the open page loads (see
//! `yawn::dynamic`) instead of rebuilding and re-linking the app. The app itself is built as usual.

use super::cargo::{self, KillGroup, Parsed};
use super::replay::{self, Recorded};
use std::collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LOADER: &str = include_str!("yawn-pic.js");

/// Lets the glue wasm-bindgen wrote load the side modules between creating the app's instance and
/// starting it. `false` when the glue doesn't have the expected shape
pub fn patch_glue(pkg: &Path) -> bool {
    let path = pkg.join("app.js");
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let anchor = "const { instance, module } = await __wbg_load(await module_or_path, imports);";
    if text.matches(anchor).count() != 1 {
        return false;
    }
    if text.contains("imports.__wbindgen_placeholder__ ??=") {
        return true;
    }
    let imports_anchor = "const imports = __wbg_get_imports();";
    let current = text.contains("globalThis.__yawn_sides");
    let text = text.replace(
        imports_anchor,
        &format!(
            "{imports_anchor}\n    imports.__wbindgen_placeholder__ ??= {{ __wbindgen_describe() {{}}, __wbindgen_describe_generic_import() {{}} }};"
        ),
    );
    let patched = if current {
        text
    } else {
        text.replace(
            anchor,
            &format!(
                "{anchor}\n                if (globalThis.__yawn_sides) await globalThis.__yawn_sides(instance, import.meta.url, imports);"
            ),
        )
    };
    let patched = if patched.starts_with("import \"./yawn-pic.js\";") {
        patched
    } else {
        format!("import \"./yawn-pic.js\";\n{patched}")
    };
    std::fs::write(path, patched).is_ok()
}

/// Whether this machine can link side modules: `rust-lld` ships with rustup's `rustc` component
pub fn available() -> Result<(), String> {
    if Dynamic::new(Path::new("."), Path::new(".")).linker().is_none() {
        return Err("rust-lld not found".into());
    }
    Ok(())
}

/// A route file in a crate of its own
#[derive(Clone, Debug)]
pub struct Leaf {
    pub dir: String,
    pub package: String,
}

pub enum Plan {
    /// No generated code changed
    Nothing,
    /// Only the bodies of these routes changed
    Leaves(Vec<Leaf>),
    /// Anything else: the app is rebuilt
    Full(String),
}

pub enum SideError {
    /// This edit can't be a side module (it needs the app rebuilt)
    Fallback(String),
    /// The build failed; the diagnostics went to the caller
    Failed,
}

type Snapshot = BTreeMap<PathBuf, (u64, u64)>;

pub struct Dynamic {
    root: PathBuf,
    dev_dir: PathBuf,
    exports: BTreeSet<String>,
    server_exports: BTreeSet<String>,
    blocked: BTreeSet<String>,
    seen: std::collections::HashMap<PathBuf, ((std::time::SystemTime, u64), BTreeSet<String>)>,
    snapshot: Option<Snapshot>,
    version: u64,
    lld: Option<PathBuf>,
    native: Vec<(String, PathBuf)>,
}

fn hash<T: Hash + ?Sized>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// (everything, everything but the bodies of components)
fn hashes(text: &str) -> (u64, u64) {
    let mut h = DefaultHasher::new();
    let mut inside = false;
    for line in text.lines() {
        let t = line.trim();
        if t == yawn_rsx::codegen::BODY_BEGIN {
            inside = true;
        } else if t == yawn_rsx::codegen::BODY_END {
            inside = false;
        } else if !inside {
            line.hash(&mut h);
        }
    }
    (hash(text), h.finish())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

impl Dynamic {
    pub fn new(root: &Path, dev_dir: &Path) -> Dynamic {
        let mut d = Dynamic {
            root: root.to_path_buf(),
            dev_dir: dev_dir.to_path_buf(),
            exports: BTreeSet::new(),
            server_exports: BTreeSet::new(),
            blocked: BTreeSet::new(),
            seen: Default::default(),
            snapshot: None,
            version: 0,
            lld: None,
            native: Vec::new(),
        };
        d.reset_exports();
        d
    }

    fn scan(&self) -> Snapshot {
        let mut files = Vec::new();
        walk(&self.root.join(yawn_rsx::project::OUT_DIR), &mut files);
        files
            .into_iter()
            .filter_map(|p| Some((p.clone(), hashes(&std::fs::read_to_string(&p).ok()?))))
            .collect()
    }

    /// The route files that live in a crate of their own, with that crate
    /// Back to export lists that name nothing: after a change of the manifests the symbols of the
    /// old lists don't exist any more (and a new session starts without knowing what they were for)
    pub fn reset_exports(&mut self) {
        let _ = std::fs::create_dir_all(&self.dev_dir);
        let _ = std::fs::write(self.dynamic_list(), "{\n__yawn_dynamic;\n};\n");
        let _ = std::fs::write(self.rsp(), "--export-table\n--growable-table\n");
        self.exports.clear();
        self.server_exports.clear();
    }

    /// Every crate of the split project: the file or folder each one holds a module of, and the crate
    fn owners(&self) -> Vec<(PathBuf, bool, Leaf)> {
        let pkg = yawn_rsx::manifest::package_name(&self.root).unwrap_or_else(|| "app".into());
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.root.join(yawn_rsx::split::CRATES_DIR)) else {
            return out;
        };
        for e in rd.flatten() {
            let dir = e.file_name().to_string_lossy().into_owned();
            let mut text = String::new();
            let Ok(files) = std::fs::read_dir(e.path()) else { continue };
            for f in files.flatten() {
                if f.path().extension().is_some_and(|x| x == "rs") {
                    text.push_str(&std::fs::read_to_string(f.path()).unwrap_or_default());
                    text.push('\n');
                }
            }
            let leaf =
                Leaf { dir: dir.clone(), package: yawn_rsx::manifest::member_package(&pkg, &dir) };
            for line in text.lines() {
                if let Some(path) =
                    line.strip_prefix("#[path = \"").and_then(|r| r.strip_suffix("\"]"))
                {
                    let path = PathBuf::from(path);
                    if path.file_name().is_some_and(|n| n == "mod.rs") {
                        if let Some(parent) = path.parent() {
                            out.push((parent.to_path_buf(), true, leaf.clone()));
                        }
                    } else {
                        out.push((path, false, leaf.clone()));
                    }
                }
            }
        }
        out
    }

    fn crates(&self) -> Vec<Leaf> {
        let mut seen = BTreeSet::new();
        self.owners()
            .into_iter()
            .map(|(_, _, l)| l)
            .filter(|l| seen.insert(l.dir.clone()))
            .collect()
    }

    /// Records the generated code the app was just built from
    pub fn remember(&mut self) {
        self.snapshot = Some(self.scan());
    }

    /// Compares the generated code with what the app (or the last side module) was built from
    pub fn plan(&mut self) -> Plan {
        let next = self.scan();
        let Some(prev) = self.snapshot.replace(next.clone()) else {
            return Plan::Full("no baseline".into());
        };
        let mut changed = Vec::new();
        for (path, now) in &next {
            match prev.get(path) {
                Some(before) if before == now => {}
                Some(before) if before.1 != now.1 => {
                    return Plan::Full(format!("{} changed outside a body", path.display()));
                }
                Some(_) => changed.push(path.clone()),
                None => return Plan::Full(format!("{} is new", path.display())),
            }
        }
        if prev.keys().any(|p| !next.contains_key(p)) {
            return Plan::Full("a file went away".into());
        }
        if changed.is_empty() {
            return Plan::Nothing;
        }
        let owners = self.owners();
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for path in changed {
            let owner = owners
                .iter()
                .filter(|(p, dir, _)| if *dir { path.starts_with(p) } else { *p == path })
                .max_by_key(|(p, _, _)| p.components().count())
                .map(|(_, _, l)| l);
            match owner {
                Some(leaf) => {
                    if seen.insert(leaf.dir.clone()) {
                        out.push(leaf.clone());
                    }
                }
                None => return Plan::Full(format!("{} belongs to no crate", path.display())),
            }
        }
        Plan::Leaves(out)
    }

    /// The server was rebuilt: the route libraries loaded into the old one are gone with it
    pub fn reset_native(&mut self, dev_dir: &Path) {
        let _ = std::fs::remove_dir_all(dev_dir.join("native"));
        let _ = std::fs::create_dir_all(dev_dir);
        let _ = std::fs::write(dev_dir.join("native.json"), "[]");
        self.native.clear();
    }

    /// Builds one route for the running server as a shared library it loads on its next request
    pub fn build_native(
        &mut self,
        leaf: &Leaf,
        args: &[&str],
        env: &[(String, String)],
        dev_dir: &Path,
        kill: &KillGroup,
        on: &mut dyn FnMut(&Parsed),
    ) -> Result<(), SideError> {
        let fallback = |why: &str| SideError::Fallback(why.to_string());
        if !cfg!(unix) {
            return Err(fallback("route libraries need a unix server"));
        }
        let mut rlib = None;
        let run = cargo::run_with(args, &self.root, kill, env, |p| {
            if let Parsed::Artifact(a) = p {
                for f in &a.files {
                    let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    if name.ends_with(".rlib") && name.contains(&leaf.dir) {
                        rlib = Some(f.clone());
                    }
                }
            }
            on(p);
        })
        .map_err(|e| fallback(&e.to_string()))?;
        if !run.ok {
            return Err(SideError::Failed);
        }
        let rlib = rlib.ok_or_else(|| fallback("cargo reported no library"))?;
        let data = std::fs::read(&rlib).map_err(|e| fallback(&e.to_string()))?;
        let work = dev_dir.join("native");
        let tmp = work.join(format!("{}.objects", leaf.dir));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| fallback(&e.to_string()))?;
        let mut objects = Vec::new();
        for (i, (name, body)) in ar_objects(&data).into_iter().enumerate() {
            let path = tmp.join(format!("{i}-{}", name.replace('/', "_")));
            let body = elf_unshare(body).map_err(|e| fallback(&e))?;
            std::fs::write(&path, body).map_err(|e| fallback(&e.to_string()))?;
            objects.push(path);
        }
        if objects.is_empty() {
            return Err(fallback("the route library has no object code"));
        }
        self.version += 1;
        let out = work.join(format!("{}.{}.so", leaf.dir, self.version));
        let mut cc = Command::new("cc");
        if cfg!(target_os = "macos") {
            cc.args(["-dynamiclib", "-undefined", "dynamic_lookup", "-o"]);
        } else {
            cc.args(["-shared", "-Wl,-Bsymbolic", "-o"]);
        }
        let linked = cc.arg(&out).args(&objects).output().map_err(|e| fallback(&e.to_string()))?;
        let _ = std::fs::remove_dir_all(&tmp);
        if !linked.status.success() {
            return Err(fallback(&String::from_utf8_lossy(&linked.stderr)));
        }
        self.native.retain(|(d, old)| {
            if d == &leaf.dir {
                let _ = std::fs::remove_file(old);
            }
            d != &leaf.dir
        });
        self.native.push((leaf.dir.clone(), out));
        let list: Vec<String> =
            self.native.iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect();
        std::fs::write(
            dev_dir.join("native.json"),
            serde_json::to_string(&list).unwrap_or_default(),
        )
        .map_err(|e| fallback(&e.to_string()))
    }

    fn dyn_dir(&self, dist: &Path) -> PathBuf {
        dist.join("pkg").join("dyn")
    }

    /// The app was rebuilt: what side modules there were are older than it
    pub fn reset(&mut self, dist: &Path) -> std::io::Result<()> {
        let dir = self.dyn_dir(dist);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("manifest.json"), r#"{"modules":[]}"#)?;
        std::fs::write(dist.join("pkg").join("yawn-pic.js"), LOADER)?;
        Ok(())
    }

    pub fn linker(&mut self) -> Option<PathBuf> {
        if self.lld.is_none() {
            let out = |args: &[&str]| {
                Command::new("rustc")
                    .args(args)
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            };
            let sysroot = out(&["--print", "sysroot"])?;
            let host = out(&["-vV"])?
                .lines()
                .find_map(|l| l.strip_prefix("host: ").map(str::to_string))?;
            let lld = Path::new(sysroot.trim()).join("lib/rustlib").join(host).join("bin/rust-lld");
            lld.is_file().then(|| self.lld = Some(lld))?;
        }
        self.lld.clone()
    }

    /// Builds one route as a side module and adds it to the manifest the page reads. The route
    /// crate is compiled again as cargo compiled it, but position independent, so the module
    /// refers to the app's own code (`std` included) by name instead of carrying a copy
    pub fn build_side(
        &mut self,
        leaf: &Leaf,
        rec: &Recorded,
        dist: &Path,
        kill: &KillGroup,
        on: &mut dyn FnMut(&Parsed),
    ) -> Result<(), SideError> {
        let fallback = |why: &str| SideError::Fallback(why.to_string());
        let lld = self.linker().ok_or_else(|| fallback("rust-lld not found"))?;
        let work = self.dev_dir.join("dyn-tmp").join(&leaf.dir);
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).map_err(|e| fallback(&e.to_string()))?;
        let mut argv = Vec::new();
        let mut i = 0;
        while i < rec.argv.len() {
            let a = rec.argv[i].as_str();
            let next = rec.argv.get(i + 1).map(String::as_str).unwrap_or("");
            if a == "--emit" || a == "--out-dir" || (a == "-C" && next.starts_with("incremental="))
            {
                i += 2;
                continue;
            }
            if a.starts_with("--emit=") || a.starts_with("-Cincremental=") {
                i += 1;
                continue;
            }
            argv.push(rec.argv[i].clone());
            i += 1;
        }
        argv.extend([
            "--emit=link".into(),
            "--out-dir".into(),
            work.to_string_lossy().into_owned(),
        ]);
        argv.extend(["-C".into(), "relocation-model=pic".into()]);
        let incr = self.dev_dir.join("dyn-incr").join(&leaf.dir);
        argv.extend(["-C".into(), format!("incremental={}", incr.display())]);
        let ok = replay::run_recorded(&argv, &rec.env, &rec.cwd, kill, on)
            .map_err(|e| fallback(&e.to_string()))?;
        if !ok {
            return Err(SideError::Failed);
        }
        let rlib = std::fs::read_dir(&work)
            .ok()
            .and_then(|rd| {
                rd.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "rlib"))
            })
            .ok_or_else(|| fallback("rustc produced no library"))?;
        let data = std::fs::read(&rlib).map_err(|e| fallback(&e.to_string()))?;
        let mut objects = Vec::new();
        for (i, (name, body)) in ar_objects(&data).into_iter().enumerate() {
            let path = work.join(format!("{i}-{}", name.replace('/', "_")));
            let body = wasm_unshare(body).map_err(|e| fallback(&e))?;
            std::fs::write(&path, body).map_err(|e| fallback(&e.to_string()))?;
            objects.push(path);
        }
        if objects.is_empty() {
            return Err(fallback("the route library has no object code"));
        }
        self.version += 1;
        let file = format!("{}.{}.wasm", leaf.dir, self.version);
        let out = work.join("side.wasm");
        let status = Command::new(&lld)
            .args([
                "-flavor",
                "wasm",
                "-shared",
                "--experimental-pic",
                "--unresolved-symbols=import-dynamic",
                "--no-entry",
                "--export-dynamic",
                "-o",
            ])
            .arg(&out)
            .args(&objects)
            .output()
            .map_err(|e| fallback(&e.to_string()))?;
        if !status.status.success() {
            return Err(fallback(&String::from_utf8_lossy(&status.stderr)));
        }
        let bytes = std::fs::read(&out).map_err(|e| fallback(&e.to_string()))?;
        let _ = std::fs::remove_dir_all(&work);
        let imports = wasm_imports(&bytes).ok_or_else(|| fallback("unreadable side module"))?;
        let own: BTreeSet<String> = wasm_exports(&bytes).unwrap_or_default().into_iter().collect();
        let main: BTreeSet<String> = std::fs::read(dist.join("pkg").join("app_bg.wasm"))
            .ok()
            .and_then(|b| wasm_exports(&b))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let glue = std::fs::read_to_string(dist.join("pkg").join("app.js")).unwrap_or_default();
        const SHARED: [&str; 5] = [
            "memory",
            "__indirect_function_table",
            "__stack_pointer",
            "__memory_base",
            "__table_base",
        ];
        for (m, n) in &imports {
            if m == "env" && SHARED.contains(&n.as_str()) {
                continue;
            }
            // what wasm-bindgen reads the types of the app with; nothing calls it at run time
            if m == "__wbindgen_placeholder__"
                && (matches!(
                    n.as_str(),
                    "__wbindgen_describe" | "__wbindgen_describe_generic_import"
                ) || glue.contains(&format!("{n}: function")))
            {
                continue;
            }
            if m == "GOT.func" || m == "GOT.mem" {
                if own.contains(n) || main.contains(n) {
                    continue;
                }
            } else if m == "env" && main.contains(n) {
                continue;
            }
            return Err(fallback(&format!("the app does not export `{m}.{n}`")));
        }
        let dir = self.dyn_dir(dist);
        std::fs::create_dir_all(&dir).map_err(|e| fallback(&e.to_string()))?;
        std::fs::write(dir.join(&file), &bytes).map_err(|e| fallback(&e.to_string()))?;
        let manifest = dir.join("manifest.json");
        let mut modules: Vec<(String, String)> = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                v.get("modules")?.as_array().map(|a| {
                    a.iter()
                        .filter_map(|m| {
                            Some((
                                m.get("route")?.as_str()?.to_string(),
                                m.get("url")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        for (route, url) in &modules {
            if route == &leaf.dir && *url != file {
                let _ = std::fs::remove_file(dir.join(url));
            }
        }
        modules.retain(|(route, _)| route != &leaf.dir);
        modules.push((leaf.dir.clone(), file));
        let json = serde_json::json!({
            "modules": modules.iter().map(|(r, u)| serde_json::json!({"route": r, "url": u})).collect::<Vec<_>>()
        });
        std::fs::write(&manifest, json.to_string()).map_err(|e| fallback(&e.to_string()))?;
        Ok(())
    }

    /// The link argument that makes the app module export what route crates refer to
    pub fn link_arg(&self) -> String {
        format!("-Clink-arg=@{}", self.rsp().display())
    }

    fn rsp(&self) -> PathBuf {
        self.dev_dir.join("exports.rsp")
    }

    /// After the app was built: makes it export every symbol the crates' side modules import. When
    /// that list changed the app's last rustc step runs again with it. `Ok(true)` when it did
    pub fn refresh_exports(
        &mut self,
        out_dir_of: &mut dyn FnMut(&str) -> Option<PathBuf>,
        root: Option<Recorded>,
        wasm: &Path,
        kill: &KillGroup,
    ) -> std::io::Result<bool> {
        let mut wanted = BTreeSet::new();
        for leaf in self.crates() {
            let krate = leaf.package.replace('-', "_");
            let Some(dir) = out_dir_of(&krate) else { continue };
            wanted.extend(self.imports_of(&dir, &krate, None)?);
        }
        let routes = yawn_rsx::manifest::member_package(
            &yawn_rsx::manifest::package_name(&self.root).unwrap_or_else(|| "app".into()),
            "routes_",
        )
        .replace('-', "_");
        wanted.retain(|n| !n.contains(&routes));
        wanted.retain(|n| {
            !matches!(
                n.as_str(),
                "__stack_pointer"
                    | "__memory_base"
                    | "__table_base"
                    | "__indirect_function_table"
                    | "__heap_base"
                    | "__heap_end"
                    | "__linear_memory"
                    | "__tls_base"
            )
        });
        if wanted == self.exports {
            return Ok(false);
        }
        let Some(root) = root else { return Ok(false) };
        let mut text = String::from("--export-table\n--growable-table\n");
        for n in &wanted {
            text.push_str("--undefined=");
            text.push_str(n);
            text.push_str("\n--export=");
            text.push_str(n);
            text.push('\n');
        }
        std::fs::write(self.rsp(), text)?;
        let ok = replay::run_recorded(&root.argv, &root.env, &root.cwd, kill, &mut |_| {})?;
        if !ok {
            return Ok(false);
        }
        let name = wasm.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
        relink_copy(&root.out_dir.join(name), wasm)?;
        self.exports = wanted;
        Ok(true)
    }

    /// The same for the server binary: the symbols the route libraries take from it
    pub fn refresh_server_exports(
        &mut self,
        out_dir_of: &mut dyn FnMut(&str) -> Option<PathBuf>,
        bin: Option<Recorded>,
        exe: &Path,
        kill: &KillGroup,
    ) -> std::io::Result<bool> {
        if !cfg!(target_os = "linux") {
            return Ok(false);
        }
        let mut wanted = BTreeSet::new();
        for leaf in self.crates() {
            let krate = leaf.package.replace('-', "_");
            let Some(dir) = out_dir_of(&krate) else { continue };
            wanted.extend(self.imports_of(&dir, &krate, Some(&leaf.dir))?);
        }
        if wanted == self.server_exports {
            return Ok(false);
        }
        let Some(bin) = bin else { return Ok(false) };
        let mut text = String::from("{\n__yawn_dynamic;\n");
        for n in &wanted {
            text.push_str(n);
            text.push_str(";\n");
        }
        text.push_str("};\n");
        std::fs::write(self.dynamic_list(), text)?;
        let ok = replay::run_recorded(&bin.argv, &bin.env, &bin.cwd, kill, &mut |_| {})?;
        if !ok {
            return Ok(false);
        }
        let name = exe.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
        relink_copy(&bin.out_dir.join(name), exe)?;
        self.server_exports = wanted;
        Ok(true)
    }

    /// Whether the crate has statics a library of its own could not share with the server
    pub fn native_blocked(&self, dir: &str) -> bool {
        self.blocked.contains(dir)
    }

    fn dynamic_list(&self) -> PathBuf {
        self.dev_dir.join("server-dynamic.list")
    }

    /// What the server binary is linked with so route libraries can resolve against it
    pub fn server_link_arg(&self) -> Option<String> {
        if !cfg!(unix) {
            return None;
        }
        Some(if cfg!(target_os = "macos") {
            "-Clink-arg=-Wl,-export_dynamic".to_string()
        } else {
            format!("-Clink-arg=-Wl,--dynamic-list={}", self.dynamic_list().display())
        })
    }

    /// The symbols the objects of a crate's newest library take from the rest of the app: what it
    /// refers to and doesn't define, and the mutable statics it defines (a side module refers to
    /// the app's copy of those)
    fn imports_of(
        &mut self,
        dir: &Path,
        krate: &str,
        elf: Option<&str>,
    ) -> std::io::Result<BTreeSet<String>> {
        let prefix = format!("lib{krate}-");
        let newest = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(&prefix) && n.ends_with(".rlib")
            })
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some((m.modified().ok()?, m.len(), e.path()))
            })
            .max();
        let Some((mtime, len, rlib)) = newest else { return Ok(BTreeSet::new()) };
        if let Some((stamp, set)) = self.seen.get(&rlib)
            && *stamp == (mtime, len)
        {
            return Ok(set.clone());
        }
        let data = std::fs::read(&rlib)?;
        let (mut defined, mut undefined, mut writable) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for (_, obj) in ar_objects(&data) {
            let scan = if elf.is_some() { elf_scan(obj) } else { wasm_scan(obj) };
            if let Some(scan) = scan {
                if let Some(dir) = elf
                    && (scan.unshareable || !scan.writable.is_empty())
                {
                    self.blocked.insert(dir.to_string());
                }
                defined.extend(scan.defined);
                undefined.extend(scan.undefined);
                writable.extend(scan.writable);
            }
        }
        let mut set: BTreeSet<String> = undefined.difference(&defined).cloned().collect();
        set.extend(writable);
        self.seen.insert(rlib, ((mtime, len), set.clone()));
        Ok(set)
    }
}

fn relink_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dst);
    std::fs::hard_link(src, dst).or_else(|_| std::fs::copy(src, dst).map(|_| ()))
}

/// The object files of a static library
fn ar_objects(data: &[u8]) -> Vec<(String, &[u8])> {
    let mut out = Vec::new();
    if !data.starts_with(b"!<arch>\n") {
        return out;
    }
    let mut i = 8;
    let mut names: &[u8] = &[];
    while i + 60 <= data.len() {
        let header = &data[i..i + 60];
        let name = String::from_utf8_lossy(&header[..16]).trim_end().to_string();
        let size: usize = String::from_utf8_lossy(&header[48..58]).trim().parse().unwrap_or(0);
        let start = i + 60;
        let Some(body) = data.get(start..start + size) else { break };
        if name == "//" {
            names = body;
        } else if name != "/" && !name.starts_with("/SYM64/") {
            let real = match name
                .strip_prefix('/')
                .filter(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
            {
                Some(offset) => {
                    let at: usize = offset.parse().unwrap_or(0);
                    let rest = names.get(at..).unwrap_or_default();
                    let end =
                        rest.iter().position(|&b| b == b'/' || b == b'\n').unwrap_or(rest.len());
                    String::from_utf8_lossy(&rest[..end]).into_owned()
                }
                None => name.trim_end_matches('/').to_string(),
            };
            if real.ends_with(".o") {
                out.push((real, body));
            }
        }
        i = start + size + (size & 1);
    }
    out
}

struct Reader<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Option<u32> {
        let (mut r, mut s) = (0u32, 0);
        loop {
            let b = *self.d.get(self.i)?;
            self.i += 1;
            r |= ((b & 0x7f) as u32).checked_shl(s).unwrap_or(0);
            s += 7;
            if b & 0x80 == 0 {
                return Some(r);
            }
        }
    }
    fn u8(&mut self) -> Option<u8> {
        let b = *self.d.get(self.i)?;
        self.i += 1;
        Some(b)
    }
    fn name(&mut self) -> Option<String> {
        let l = self.u32()? as usize;
        let s = String::from_utf8_lossy(self.d.get(self.i..self.i + l)?).into_owned();
        self.i += l;
        Some(s)
    }
    fn limits(&mut self) -> Option<()> {
        let f = self.u32()?;
        self.u32()?;
        if f & 1 != 0 {
            self.u32()?;
        }
        Some(())
    }
}

/// `(id, body)` of each section of a wasm module
fn sections(d: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut r = Reader { d, i: 8 };
    while r.i < d.len() {
        let Some(id) = r.u8() else { break };
        let Some(size) = r.u32() else { break };
        let Some(body) = d.get(r.i..r.i + size as usize) else { break };
        out.push((id, body));
        r.i += size as usize;
    }
    out
}

/// `(module, name, kind)` of every import of a wasm module
fn wasm_imports_kind(d: &[u8]) -> Option<Vec<(String, String, u8)>> {
    let mut out = Vec::new();
    for (id, body) in sections(d) {
        if id != 2 {
            continue;
        }
        let mut r = Reader { d: body, i: 0 };
        for _ in 0..r.u32()? {
            let module = r.name()?;
            let name = r.name()?;
            let kind = r.u8()?;
            match kind {
                0 | 4 => {
                    if kind == 4 {
                        r.u8()?;
                    }
                    r.u32()?;
                }
                1 => {
                    r.u8()?;
                    r.limits()?;
                }
                2 => r.limits()?,
                3 => {
                    r.u8()?;
                    r.u8()?;
                }
                _ => return None,
            }
            out.push((module, name, kind));
        }
    }
    Some(out)
}

fn wasm_imports(d: &[u8]) -> Option<Vec<(String, String)>> {
    Some(wasm_imports_kind(d)?.into_iter().map(|(m, n, _)| (m, n)).collect())
}

fn wasm_exports(d: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for (id, body) in sections(d) {
        if id != 7 {
            continue;
        }
        let mut r = Reader { d: body, i: 0 };
        for _ in 0..r.u32()? {
            out.push(r.name()?);
            r.u8()?;
            r.u32()?;
        }
    }
    Some(out)
}

/// What an object file defines and refers to
#[derive(Default)]
struct Scan {
    defined: BTreeSet<String>,
    undefined: BTreeSet<String>,
    /// Defined mutable statics: a second copy of one would be a second variable
    writable: BTreeSet<String>,
    /// Mutable statics that have no name to share by
    unshareable: bool,
}

fn writable_segment(name: &str) -> bool {
    name.starts_with(".bss")
        || name.starts_with(".tbss")
        || name.starts_with(".tdata")
        || (name.starts_with(".data") && !name.starts_with(".data.rel.ro"))
}

fn put_u32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    put_u32(out, name.len() as u32);
    out.extend_from_slice(name.as_bytes());
}

/// Walks the symbol table of a relocatable wasm object. `rewrite` gets the new bytes of the module
/// with every mutable static turned into a reference to the app's copy
fn wasm_symbols(d: &[u8], rewrite: bool) -> Option<(Scan, Option<Vec<u8>>)> {
    let imports = wasm_imports_kind(d)?;
    let func_imports: Vec<&(String, String, u8)> = imports.iter().filter(|i| i.2 == 0).collect();
    let global_imports: Vec<&(String, String, u8)> = imports.iter().filter(|i| i.2 == 3).collect();
    let mut scan = Scan::default();
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(d.get(..8)?);
    let mut r = Reader { d, i: 8 };
    let mut changed = false;
    while r.i < d.len() {
        let start = r.i;
        let id = r.u8()?;
        let size = r.u32()? as usize;
        let body_at = r.i;
        let body = d.get(body_at..body_at + size)?;
        r.i = body_at + size;
        let mut replaced: Option<Vec<u8>> = None;
        if id == 0 {
            let mut b = Reader { d: body, i: 0 };
            if b.name()? == "linking" {
                let version = b.u32()?;
                let mut segments: Vec<String> = Vec::new();
                let mut rebuilt = Vec::new();
                put_name(&mut rebuilt, "linking");
                put_u32(&mut rebuilt, version);
                while b.i < body.len() {
                    let sub = b.u8()?;
                    let sub_size = b.u32()? as usize;
                    let sub_body = body.get(b.i..b.i + sub_size)?;
                    b.i += sub_size;
                    let mut new_body: Option<Vec<u8>> = None;
                    if sub == 5 {
                        let mut s = Reader { d: sub_body, i: 0 };
                        for _ in 0..s.u32()? {
                            segments.push(s.name()?);
                            s.u32()?;
                            s.u32()?;
                        }
                    } else if sub == 8 {
                        let mut s = Reader { d: sub_body, i: 0 };
                        let count = s.u32()?;
                        let mut table = Vec::new();
                        put_u32(&mut table, count);
                        for _ in 0..count {
                            let entry = s.i;
                            let kind = s.u8()?;
                            let flags = s.u32()?;
                            let is_undefined = flags & 0x10 != 0;
                            let explicit = flags & 0x40 != 0;
                            let local = flags & 3 == 2;
                            let mut name = None;
                            let mut unshare = false;
                            match kind {
                                1 => {
                                    let n = s.name()?;
                                    if !is_undefined {
                                        let seg = s.u32()? as usize;
                                        s.u32()?;
                                        s.u32()?;
                                        if segments.get(seg).is_some_and(|n| writable_segment(n)) {
                                            if local {
                                                scan.unshareable = true;
                                            } else {
                                                scan.writable.insert(n.clone());
                                                unshare = true;
                                            }
                                        }
                                    }
                                    name = Some((n, is_undefined));
                                }
                                3 => {
                                    s.u32()?;
                                }
                                _ => {
                                    let index = s.u32()? as usize;
                                    let written = if !is_undefined || explicit {
                                        Some(s.name()?)
                                    } else {
                                        None
                                    };
                                    let import = match kind {
                                        0 => func_imports.get(index),
                                        2 => global_imports.get(index),
                                        _ => None,
                                    };
                                    if !(is_undefined && import.is_some_and(|i| i.0 != "env"))
                                        && let Some(n) =
                                            written.or_else(|| import.map(|i| i.1.clone()))
                                    {
                                        name = Some((n, is_undefined));
                                    }
                                }
                            }
                            if let Some((n, undef)) = name {
                                if undef {
                                    scan.undefined.insert(n.clone())
                                } else {
                                    scan.defined.insert(n.clone())
                                };
                                if unshare && rewrite {
                                    table.push(1);
                                    put_u32(&mut table, flags | 0x10);
                                    put_name(&mut table, &n);
                                    changed = true;
                                    continue;
                                }
                            }
                            table.extend_from_slice(&sub_body[entry..s.i]);
                        }
                        new_body = Some(table);
                    }
                    rebuilt.push(sub);
                    match &new_body {
                        Some(nb) => {
                            put_u32(&mut rebuilt, nb.len() as u32);
                            rebuilt.extend_from_slice(nb);
                        }
                        None => {
                            put_u32(&mut rebuilt, sub_size as u32);
                            rebuilt.extend_from_slice(sub_body);
                        }
                    }
                }
                replaced = Some(rebuilt);
            }
        }
        match replaced {
            Some(b) if rewrite => {
                out.push(id);
                put_u32(&mut out, b.len() as u32);
                out.extend_from_slice(&b);
            }
            _ => out.extend_from_slice(&d[start..r.i]),
        }
    }
    Some((scan, (rewrite && changed).then_some(out)))
}

fn wasm_scan(d: &[u8]) -> Option<Scan> {
    wasm_symbols(d, false).map(|(s, _)| s)
}

/// The object with its mutable statics left to the app; `Err` when one can't be
fn wasm_unshare(d: &[u8]) -> Result<Vec<u8>, String> {
    let (scan, out) = wasm_symbols(d, true).ok_or("unreadable object")?;
    if scan.unshareable {
        return Err("the crate has a private mutable static".into());
    }
    Ok(out.unwrap_or_else(|| d.to_vec()))
}

struct Elf<'a> {
    d: &'a [u8],
}

impl<'a> Elf<'a> {
    fn u16(&self, at: usize) -> Option<usize> {
        Some(u16::from_le_bytes(self.d.get(at..at + 2)?.try_into().ok()?) as usize)
    }
    fn u32(&self, at: usize) -> Option<usize> {
        Some(u32::from_le_bytes(self.d.get(at..at + 4)?.try_into().ok()?) as usize)
    }
    fn u64(&self, at: usize) -> Option<usize> {
        Some(u64::from_le_bytes(self.d.get(at..at + 8)?.try_into().ok()?) as usize)
    }
    fn cstr(&self, at: usize) -> String {
        let rest = self.d.get(at..).unwrap_or_default();
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        String::from_utf8_lossy(&rest[..end]).into_owned()
    }
}

/// Walks the symbols of a 64 bit ELF object; with `rewrite` the mutable statics become undefined
fn elf_symbols(d: &mut [u8], rewrite: bool) -> Option<Scan> {
    if d.get(..4)? != b"\x7fELF" || d.get(4) != Some(&2) {
        return None;
    }
    let (shoff, shentsize, shnum, shstrndx) = {
        let e = Elf { d };
        (e.u64(0x28)?, e.u16(0x3a)?, e.u16(0x3c)?, e.u16(0x3e)?)
    };
    let header = |i: usize| shoff + i * shentsize;
    let mut sections = Vec::new();
    {
        let e = Elf { d };
        let strtab = e.u64(header(shstrndx) + 0x18)?;
        for i in 0..shnum {
            let h = header(i);
            sections.push((
                e.cstr(strtab + e.u32(h)?),
                e.u32(h + 4)?,
                e.u64(h + 0x18)?,
                e.u64(h + 0x20)?,
                e.u32(h + 0x28)?,
            ));
        }
    }
    let mut scan = Scan::default();
    let mut patches = Vec::new();
    for (_, kind, offset, size, link) in sections.iter().filter(|s| s.1 == 2) {
        let _ = kind;
        let e = Elf { d };
        let strtab = sections.get(*link)?.2;
        for i in 1..size / 24 {
            let at = offset + i * 24;
            let name = e.u32(at)?;
            let info = *d.get(at + 4)?;
            let shndx = e.u16(at + 6)?;
            let (bind, kind) = (info >> 4, info & 0xf);
            if matches!(kind, 3 | 4) || name == 0 {
                continue;
            }
            let n = e.cstr(strtab + name);
            if bind == 0 {
                if shndx != 0
                    && shndx < 0xff00
                    && sections.get(shndx).is_some_and(|s| writable_segment(&s.0))
                    && kind == 1
                {
                    scan.unshareable = true;
                }
                continue;
            }
            if shndx == 0 {
                scan.undefined.insert(n);
            } else {
                scan.defined.insert(n.clone());
                if shndx < 0xff00 && sections.get(shndx).is_some_and(|s| writable_segment(&s.0)) {
                    if kind == 6
                        || sections[shndx].0.starts_with(".tbss")
                        || sections[shndx].0.starts_with(".tdata")
                    {
                        scan.unshareable = true;
                    } else if kind == 1 && !n.starts_with("DW.ref.") {
                        scan.writable.insert(n);
                        patches.push(at);
                    }
                }
            }
        }
    }
    if rewrite {
        for at in patches {
            d[at + 6..at + 8].copy_from_slice(&0u16.to_le_bytes());
            d[at + 8..at + 24].fill(0);
        }
    }
    Some(scan)
}

fn elf_scan(d: &[u8]) -> Option<Scan> {
    let mut copy = d.to_vec();
    elf_symbols(&mut copy, false)
}

/// The object itself, or `Err` when it defines a mutable static: a library linked from it would
/// have a second copy of the variable, and code of the app can't be made to share it
fn elf_unshare(d: &[u8]) -> Result<Vec<u8>, String> {
    let mut copy = d.to_vec();
    let scan = elf_symbols(&mut copy, false).ok_or("unreadable object")?;
    if scan.unshareable || !scan.writable.is_empty() {
        let name = scan.writable.iter().next().cloned().unwrap_or_default();
        return Err(format!("the crate has a mutable static {name}"));
    }
    Ok(d.to_vec())
}
