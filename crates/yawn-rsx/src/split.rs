use crate::manifest::Member;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Directory below `.yawn/` that holds the generated crates
pub const CRATES_DIR: &str = ".yawn/crates";

/// A top-level module
#[derive(Debug, Clone)]
pub struct Unit {
    pub name: String,
    /// Attribute lines before the declaration (`#[cfg(...)]`), each ending in a newline
    pub attrs: String,
    /// The file the module starts in
    pub file: PathBuf,
    /// Whether the module is generated from `.rsx`
    pub generated: bool,
}

thread_local! {
    /// The files of the project that are not written yet, which a unit's source may be made of
    static PENDING: std::cell::RefCell<Vec<(PathBuf, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn pending_text(file: &Path) -> Option<String> {
    PENDING.with(|p| p.borrow().iter().find(|(f, _)| f == file).map(|(_, t)| t.clone()))
}

impl Unit {
    pub(crate) fn source_text(&self) -> String {
        self.source()
    }

    fn source(&self) -> String {
        fn walk(dir: &Path, seen: &mut BTreeSet<PathBuf>, out: &mut String) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            entries.sort();
            for p in entries {
                if p.is_dir() {
                    walk(&p, seen, out);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    read(&p, seen, out);
                }
            }
        }
        fn read(file: &Path, seen: &mut BTreeSet<PathBuf>, out: &mut String) {
            if !seen.insert(file.to_path_buf()) {
                return;
            }
            let text = pending_text(file).or_else(|| fs::read_to_string(file).ok()).unwrap_or_default();
            // files named by `#[path = "..."]`
            let mut rest = text.as_str();
            while let Some(at) = rest.find("#[path = \"") {
                rest = &rest[at + "#[path = \"".len()..];
                if let Some(end) = rest.find('"') {
                    let target = PathBuf::from(&rest[..end]);
                    if target.is_absolute() {
                        read(&target, seen, out);
                    }
                }
            }
            out.push_str(&text);
            out.push('\n');
            // `name.rs` may have submodules in `name/`
            if file.file_name().is_some_and(|n| n != "mod.rs")
                && let Some(dir) =
                    file.with_extension("").to_str().map(PathBuf::from).filter(|d| d.is_dir())
            {
                walk(&dir, seen, out);
            }
        }
        let mut out = String::new();
        let mut seen = BTreeSet::new();
        if self.file.file_name().is_some_and(|n| n == "mod.rs") {
            read(&self.file, &mut seen, &mut out);
            if let Some(dir) = self.file.parent() {
                walk(dir, &mut seen, &mut out);
            }
        } else {
            read(&self.file, &mut seen, &mut out);
        }
        out
    }
}

/// The `#[macro_export]` macros `text` defines: they live at the root of the crate that holds them,
/// so `crate::name!` in another module is a use of this module
pub fn exported_macros(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("macro_rules!") else { continue };
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let exported = lines[i.saturating_sub(3)..i]
            .iter()
            .rev()
            .take_while(|l| {
                let t = l.trim();
                t.is_empty() || t.starts_with("#[") || t.starts_with("///") || t.starts_with("//")
            })
            .any(|l| l.contains("#[macro_export"));
        if exported {
            out.push(name);
        }
    }
    out
}

/// The first segments of every `crate::...` path in `text`: `crate::ui::x` gives `ui`, `use
/// crate::{a, b::c}` gives `a`, `b`
pub fn crate_refs(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let ident = |s: &str| -> String {
        s.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
    };
    let mut rest = text;
    while let Some(at) = rest.find("crate::") {
        // `$crate::` and `mycrate::` are other paths
        let before = rest[..at].chars().next_back();
        let after = &rest[at + "crate::".len()..];
        rest = after;
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
            continue;
        }
        if let Some(group) = after.strip_prefix('{') {
            // the first identifier of each item at depth 1
            let (mut depth, mut item_start, mut cur) = (1, true, String::new());
            for c in group.chars() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    ',' if depth == 1 => {
                        if !cur.is_empty() {
                            out.insert(std::mem::take(&mut cur));
                        }
                        item_start = true;
                    }
                    c if depth == 1 && item_start && (c.is_ascii_alphanumeric() || c == '_') => {
                        cur.push(c)
                    }
                    _ if depth == 1 && item_start && !cur.is_empty() => {
                        out.insert(std::mem::take(&mut cur));
                        item_start = false;
                    }
                    _ => {}
                }
            }
            if !cur.is_empty() {
                out.insert(cur);
            }
        } else {
            let name = ident(after);
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

/// A crate of the plan
#[derive(Debug, Clone)]
pub struct Group {
    /// Directory name below `.yawn/crates/`
    pub dir: String,
    /// Indices into the plan's units
    pub units: Vec<usize>,
    /// Directory names of the crates it uses
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub units: Vec<Unit>,
    pub groups: Vec<Group>,
    /// The `#[macro_export]` macros of each unit
    pub macros: Vec<Vec<String>>,
}

impl Plan {
    pub fn members(&self) -> Vec<Member> {
        self.groups
            .iter()
            .map(|g| Member { dir: g.dir.clone(), deps: g.deps.iter().cloned().collect() })
            .collect()
    }

    /// The group a unit moved to, if it moved
    pub fn group_of(&self, unit: usize) -> Option<&Group> {
        self.groups.iter().find(|g| g.units.contains(&unit))
    }
}

/// Decides which units go to which crate; `None` when splitting wouldn't pay off (fewer than two
/// crates)
pub fn plan(units: Vec<Unit>) -> Option<Plan> {
    let n = units.len();
    let index: BTreeMap<&str, usize> =
        units.iter().enumerate().map(|(i, u)| (u.name.as_str(), i)).collect();
    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut pinned = vec![false; n];
    let sources: Vec<String> = units.iter().map(Unit::source).collect();
    let macros: Vec<Vec<String>> = sources.iter().map(|s| exported_macros(s)).collect();
    let macro_owner: BTreeMap<&str, usize> = macros
        .iter()
        .enumerate()
        .flat_map(|(i, ms)| ms.iter().map(move |m| (m.as_str(), i)))
        .collect();
    for (i, source) in sources.iter().enumerate() {
        for r in crate_refs(source) {
            match index.get(r.as_str()) {
                Some(&j) if j != i => {
                    deps[i].insert(j);
                }
                Some(_) => {}
                None if macro_owner.contains_key(r.as_str()) => {
                    let j = macro_owner[r.as_str()];
                    if j != i {
                        deps[i].insert(j);
                    }
                }
                // something of the root (`crate::App`): the unit has to stay in the root crate
                None => pinned[i] = true,
            }
        }
    }
    loop {
        let mut changed = false;
        for i in 0..n {
            if !pinned[i] && deps[i].iter().any(|&j| pinned[j]) {
                pinned[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let movable: Vec<usize> = (0..n).filter(|&i| !pinned[i]).collect();
    // plain Rust units that use only plain Rust units form `base`
    let mut base: BTreeSet<usize> =
        movable.iter().copied().filter(|&i| !units[i].generated).collect();
    loop {
        let drop: Vec<usize> =
            base.iter().copied().filter(|&i| deps[i].iter().any(|j| !base.contains(j))).collect();
        if drop.is_empty() {
            break;
        }
        for i in drop {
            base.remove(&i);
        }
    }
    let rest: Vec<usize> = movable.iter().copied().filter(|i| !base.contains(i)).collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    if !base.is_empty() {
        groups.push(base.iter().copied().collect());
    }
    groups.extend(strongly_connected(&rest, &deps));
    if groups.len() < 2 {
        return None;
    }
    let mut dirs: Vec<String> = Vec::new();
    for (k, g) in groups.iter().enumerate() {
        let name = if k == 0 && !base.is_empty() {
            "base".to_string()
        } else {
            g.iter().map(|&i| units[i].name.as_str()).collect::<Vec<_>>().join("_")
        };
        // a unit called `base` must not collide with the group of that name
        dirs.push(if dirs.contains(&name) { format!("{name}_{k}") } else { name });
    }
    let owner: BTreeMap<usize, usize> =
        groups.iter().enumerate().flat_map(|(k, g)| g.iter().map(move |&i| (i, k))).collect();
    let out = groups
        .iter()
        .enumerate()
        .map(|(k, g)| Group {
            dir: dirs[k].clone(),
            units: g.clone(),
            deps: g
                .iter()
                .flat_map(|&i| deps[i].iter())
                .filter_map(|j| owner.get(j))
                .filter(|&&o| o != k)
                .map(|&o| dirs[o].clone())
                .collect(),
        })
        .collect();
    Some(Plan { units, groups: out, macros })
}

pub(crate) fn strongly_connected(nodes: &[usize], deps: &[BTreeSet<usize>]) -> Vec<Vec<usize>> {
    struct S<'a> {
        deps: &'a [BTreeSet<usize>],
        set: BTreeSet<usize>,
        index: BTreeMap<usize, usize>,
        low: BTreeMap<usize, usize>,
        stack: Vec<usize>,
        on: BTreeSet<usize>,
        next: usize,
        out: Vec<Vec<usize>>,
    }
    fn visit(s: &mut S, v: usize) {
        s.index.insert(v, s.next);
        s.low.insert(v, s.next);
        s.next += 1;
        s.stack.push(v);
        s.on.insert(v);
        let edges: Vec<usize> = s.deps[v].iter().copied().filter(|w| s.set.contains(w)).collect();
        for w in edges {
            if !s.index.contains_key(&w) {
                visit(s, w);
                let l = s.low[&v].min(s.low[&w]);
                s.low.insert(v, l);
            } else if s.on.contains(&w) {
                let l = s.low[&v].min(s.index[&w]);
                s.low.insert(v, l);
            }
        }
        if s.low[&v] == s.index[&v] {
            let mut comp = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on.remove(&w);
                comp.push(w);
                if w == v {
                    break;
                }
            }
            comp.sort();
            s.out.push(comp);
        }
    }
    let mut s = S {
        deps,
        set: nodes.iter().copied().collect(),
        index: BTreeMap::new(),
        low: BTreeMap::new(),
        stack: Vec::new(),
        on: BTreeSet::new(),
        next: 0,
        out: Vec::new(),
    };
    for &v in nodes {
        if !s.index.contains_key(&v) {
            visit(&mut s, v);
        }
    }
    s.out
}

fn parse_client_mod(line: &str) -> Option<(String, &str, String)> {
    let rest = line.strip_prefix("#[path = \"")?;
    let (path, rest) = rest.split_once("\"] ")?;
    let (vis, rest) = if let Some(r) = rest.strip_prefix("pub(crate) ") {
        ("pub(crate) ", r)
    } else if let Some(r) = rest.strip_prefix("pub ") {
        ("pub ", r)
    } else {
        ("", rest)
    };
    let name = rest.strip_prefix("mod ")?.strip_suffix(';')?;
    Some((path.to_string(), vis, name.to_string()))
}

fn find_units(client: &str, modules: &str, rsx_dir: &Path) -> Vec<(Unit, bool, usize)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = client.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let (path, _vis, name) = match parse_client_mod(line) {
            Some(x) => x,
            None => {
                // `#[path = ".."]` and `pub mod x;` on two lines, as the server and api modules are written
                if let (Some(rest), Some(next)) =
                    (line.strip_prefix("#[path = \""), lines.get(i + 1))
                {
                    match (
                        rest.strip_suffix("\"]"),
                        next.strip_prefix("pub mod ").and_then(|r| r.strip_suffix(';')),
                    ) {
                        (Some(p), Some(name)) => (p.to_string(), "pub ", name.to_string()),
                        _ => continue,
                    }
                } else {
                    continue;
                }
            }
        };
        let attrs = if i > 0 && lines[i - 1].starts_with("#[cfg") {
            format!("{}\n", lines[i - 1])
        } else {
            String::new()
        };
        let file = PathBuf::from(&path);
        out.push((Unit { name, attrs, generated: file.starts_with(rsx_dir), file }, true, i));
    }
    let mlines: Vec<&str> = modules.lines().collect();
    let mut i = 0;
    while i + 1 < mlines.len() {
        if let (Some(rest), Some(name)) = (
            mlines[i].strip_prefix("#[path = \""),
            mlines[i + 1].strip_prefix("pub mod ").and_then(|r| r.strip_suffix(';')),
        ) && let Some(path) = rest.strip_suffix("\"]")
        {
            let file = PathBuf::from(path);
            out.push((
                Unit {
                    name: name.to_string(),
                    attrs: String::new(),
                    generated: file.starts_with(rsx_dir),
                    file,
                },
                false,
                i,
            ));
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// What the split changes in the generated files
pub struct Split {
    pub client: String,
    pub modules: String,
    /// `lib.rs` of every member crate
    pub files: Vec<(PathBuf, String)>,
    pub members: Vec<Member>,
    /// The module files of generated folders whose declarations moved to crates, by path
    pub folder_files: Vec<(PathBuf, String)>,
}

/// A route file that lives in a crate of its own: nothing else names it, so editing it rebuilds
/// only that crate and the thin `routes` crate that lists it
struct Leaf {
    dir: String,
    name: String,
    file: PathBuf,
    /// Directories of the crates it uses
    deps: BTreeSet<String>,
    /// The lines that bring the modules it uses into scope, when the crates are cut by files
    facade: Option<String>,
}

fn leaf_decls(text: &str) -> Vec<(usize, String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < lines.len() {
        if let (Some(rest), Some(name)) = (
            lines[i].strip_prefix("#[path = \""),
            lines[i + 1].strip_prefix("pub mod ").and_then(|r| r.strip_suffix(';')),
        ) && let Some(path) = rest.strip_suffix("\"]")
            && !path.ends_with("/mod.rs")
            && path.ends_with(".rs")
        {
            out.push((i, path.to_string(), name.to_string()));
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// The route files that can leave the `routes` crate, with the crates each one uses
fn find_leaves(
    root: &Path,
    plan: &Plan,
    folder_files: &[(PathBuf, String)],
    refined: Option<&crate::split_nodes::Refined>,
) -> Vec<(PathBuf, usize, Leaf)> {
    let rsx_dir = root.join(crate::project::OUT_DIR);
    let routes_dir = rsx_dir.join("routes");
    let index: BTreeMap<&str, usize> =
        plan.units.iter().enumerate().map(|(i, u)| (u.name.as_str(), i)).collect();
    let macro_owner: BTreeMap<&str, usize> = plan
        .macros
        .iter()
        .enumerate()
        .flat_map(|(i, ms)| ms.iter().map(move |m| (m.as_str(), i)))
        .collect();
    let mut out = Vec::new();
    for (path, text) in folder_files {
        if !path.starts_with(&routes_dir) || path.file_name().is_none_or(|n| n != "mod.rs") {
            continue;
        }
        for (at, file, name) in leaf_decls(text) {
            let file = PathBuf::from(file);
            if !file.starts_with(&routes_dir) {
                continue;
            }
            let Ok(source) = fs::read_to_string(&file) else { continue };
            let mut deps = BTreeSet::new();
            let mut stays = false;
            let mut facade = None;
            match refined {
                Some(r) => match r.needs_of(&source) {
                    Some(needs) if !crate_refs(&source).contains("routes") => {
                        let (text, dirs) = r.route_facade(&needs);
                        deps = dirs.into_iter().collect();
                        facade = Some(text);
                    }
                    _ => stays = true,
                },
                None => {
                    for r in crate_refs(&source) {
                        let unit = index.get(r.as_str()).or_else(|| macro_owner.get(r.as_str()));
                        match unit.and_then(|&u| (r != "routes").then(|| plan.group_of(u)).flatten()) {
                            Some(g) => {
                                deps.insert(g.dir.clone());
                            }
                            None => stays = true,
                        }
                    }
                }
            }
            if stays {
                continue;
            }
            let rel = file.strip_prefix(&routes_dir).unwrap_or(&file).with_extension("");
            let dir = format!(
                "routes_{}",
                rel.to_string_lossy().replace(['/', '-', '[', ']', '.'], "_")
            );
            out.push((path.clone(), at, Leaf { dir, name, file, deps, facade }));
        }
    }
    out
}

/// Rewrites the client root and `modules.rs` for a split project, and writes the member crates'
/// roots
pub fn apply(
    root: &Path,
    client: &str,
    modules: &str,
    folder_files: &[(PathBuf, String)],
) -> Option<Split> {
    let rsx_dir = root.join(crate::project::OUT_DIR);
    PENDING.with(|p| *p.borrow_mut() = folder_files.to_vec());
    let found = find_units(client, modules, &rsx_dir);
    // two declarations of one name can't be told apart
    let names: BTreeSet<&str> = found.iter().map(|(u, ..)| u.name.as_str()).collect();
    if names.len() != found.len() {
        return None;
    }
    let units: Vec<Unit> = found.iter().map(|(u, ..)| u.clone()).collect();
    let plan = plan(units);
    let refined = plan.as_ref().and_then(|p| crate::split_nodes::refine(root, p, &p.macros));
    PENDING.with(|p| p.borrow_mut().clear());
    let plan = plan?;

    let mut client_lines: Vec<String> = client.lines().map(str::to_string).collect();
    let mlines: Vec<&str> = modules.lines().collect();
    let mut drop_modules: BTreeSet<usize> = BTreeSet::new();
    for (k, (unit, in_client, at)) in found.iter().enumerate() {
        let krate = match &refined {
            Some(r) => match r.unit_crate(k) {
                Some(c) => c,
                None => continue,
            },
            None => {
                let Some(group) = plan.group_of(k) else {
                    continue;
                };
                crate::manifest::member_package(&package_ident(root), &group.dir).replace('-', "_")
            }
        };
        let reexport = format!("pub use {krate}::{};", unit.name);
        for m in &plan.macros[k] {
            client_lines.push(format!("{}pub use {krate}::{m};", unit.attrs));
        }
        if *in_client {
            // the `#[path]` form is one line; the two-line form is a line and the next
            if parse_client_mod(&client_lines[*at]).is_some() {
                client_lines[*at] = reexport;
            } else {
                client_lines[*at] = reexport;
                client_lines[*at + 1] = String::new();
            }
        } else {
            drop_modules.insert(*at);
            drop_modules.insert(*at + 1);
            client_lines.push(format!("{}{reexport}", unit.attrs));
        }
    }
    let mut client_text = client_lines.join("\n");
    client_text.push('\n');
    let modules_text: String = mlines
        .iter()
        .enumerate()
        .filter(|(i, _)| !drop_modules.contains(i))
        .map(|(_, l)| format!("{l}\n"))
        .collect();

    let pkg = package_ident(root);
    let mut files = Vec::new();
    let groups = if refined.is_some() { &[][..] } else { &plan.groups[..] };
    for g in groups {
        let mut text = String::from(
            "// @generated by yawn-rsx: a crate of the project's modules (see yawn_rsx::split). Do not edit.\n#![allow(unused_imports)]\n",
        );
        // the units this crate uses from the crates below it
        for dep in &g.deps {
            let dep_crate = crate::manifest::member_package(&pkg, dep).replace('-', "_");
            let dep_group = plan.groups.iter().find(|x| &x.dir == dep).unwrap();
            for &u in &dep_group.units {
                let unit = &plan.units[u];
                text.push_str(&format!("{}pub use {dep_crate}::{};\n", unit.attrs, unit.name));
                for m in &plan.macros[u] {
                    text.push_str(&format!("{}pub use {dep_crate}::{m};\n", unit.attrs));
                }
            }
        }
        for &u in &g.units {
            let unit = &plan.units[u];
            text.push_str(&format!(
                "{}#[path = {:?}]\npub mod {};\n",
                unit.attrs,
                crate::project::slashed(&unit.file),
                unit.name
            ));
        }
        files.push((root.join(CRATES_DIR).join(&g.dir).join("lib.rs"), text));
    }
    let mut members = match &refined {
        Some(r) => {
            files.extend(r.files.iter().cloned());
            r.members.clone()
        }
        None => plan.members(),
    };
    let mut folder_files: Vec<(PathBuf, String)> = folder_files.to_vec();
    let routes_unit = plan.units.iter().position(|u| u.name == "routes");
    let routes_dir: Option<String> = match &refined {
        Some(r) => routes_unit.and_then(|u| r.unit_dir(u)),
        None => plan
            .groups
            .iter()
            .find(|g| g.units.iter().any(|&u| plan.units[u].name == "routes"))
            .map(|g| g.dir.clone()),
    };
    let leaves = if routes_dir.is_some() {
        find_leaves(root, &plan, &folder_files, refined.as_ref())
    } else {
        Vec::new()
    };
    let taken: BTreeSet<&str> = match &refined {
        Some(_) => members.iter().map(|m| m.dir.as_str()).collect(),
        None => plan.groups.iter().map(|g| g.dir.as_str()).collect(),
    };
    if leaves.len() > 1 && leaves.iter().all(|(.., l)| !taken.contains(l.dir.as_str())) {
        let routes_dir = routes_dir.unwrap_or_default();
        let mut by_file: BTreeMap<PathBuf, Vec<(usize, &Leaf)>> = BTreeMap::new();
        for (path, at, leaf) in &leaves {
            by_file.entry(path.clone()).or_default().push((*at, leaf));
        }
        for (path, text) in folder_files.iter_mut() {
            let Some(decls) = by_file.get(path) else { continue };
            let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
            for (at, leaf) in decls {
                let krate = crate::manifest::member_package(&pkg, &leaf.dir).replace('-', "_");
                lines[*at] = format!("pub use {krate}::route::{};", leaf.name);
                lines[*at + 1] = String::new();
            }
            *text = lines.join("\n") + "\n";
        }
        for (_, _, leaf) in &leaves {
            let mut text = String::from(
                "// @generated by yawn-rsx: a route of the project (see yawn_rsx::split). Do not edit.\n#![allow(unused_imports)]\n",
            );
            if let Some(f) = &leaf.facade {
                text.push_str(f);
            }
            for dep in leaf.deps.iter().filter(|_| leaf.facade.is_none()) {
                let dep_crate = crate::manifest::member_package(&pkg, dep).replace('-', "_");
                let dep_group = plan.groups.iter().find(|x| &x.dir == dep).unwrap();
                for &u in &dep_group.units {
                    let unit = &plan.units[u];
                    text.push_str(&format!("{}pub use {dep_crate}::{};\n", unit.attrs, unit.name));
                    for m in &plan.macros[u] {
                        text.push_str(&format!("{}pub use {dep_crate}::{m};\n", unit.attrs));
                    }
                }
            }
            // inside `route`, so the route's name can equal that of a module it imports
            text.push_str(&format!(
                "pub mod route {{\n#[path = {:?}]\npub mod {};\n}}\n",
                crate::project::slashed(&leaf.file),
                leaf.name
            ));
            files.push((root.join(CRATES_DIR).join(&leaf.dir).join("lib.rs"), text));
            members.push(Member { dir: leaf.dir.clone(), deps: leaf.deps.iter().cloned().collect() });
        }
        if let Some(m) = members.iter_mut().find(|m| m.dir == routes_dir) {
            m.deps.extend(leaves.iter().map(|(.., l)| l.dir.clone()));
        }
    }
    Some(Split { client: client_text, modules: modules_text, files, members, folder_files })
}

pub(crate) fn package_ident_of(root: &Path) -> String {
    package_ident(root)
}

fn package_ident(root: &Path) -> String {
    crate::manifest::package_name(root).unwrap_or_else(|| "app".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_crate_paths() {
        let r = crate_refs(
            "use crate::ui::{Button, Card};\nlet x = crate::i18n::t(\"a\");\nuse crate::{auth, guilds::GuildView, model::{A, B}};\nmycrate::x; $crate::y; crate::App",
        );
        let v: Vec<_> = r.iter().map(String::as_str).collect();
        assert_eq!(v, ["App", "auth", "guilds", "i18n", "model", "ui"]);
    }

    fn unit(name: &str, generated: bool, source: &str) -> (Unit, String) {
        (
            Unit {
                name: name.into(),
                attrs: String::new(),
                file: PathBuf::from(format!("/nonexistent/{name}.rs")),
                generated,
            },
            source.into(),
        )
    }

    fn planned(specs: &[(&str, bool, &str)]) -> Option<Plan> {
        let dir =
            std::env::temp_dir().join(format!("yawn-split-{}-{}", std::process::id(), specs.len()));
        fs::create_dir_all(&dir).unwrap();
        let units = specs
            .iter()
            .map(|(n, g, src)| {
                let (mut u, _) = unit(n, *g, src);
                u.file = dir.join(format!("{n}.rs"));
                fs::write(&u.file, src).unwrap();
                u
            })
            .collect();
        let p = plan(units);
        let _ = fs::remove_dir_all(&dir);
        p
    }

    fn dirs(p: &Plan) -> Vec<(String, Vec<&str>, Vec<&str>)> {
        p.groups
            .iter()
            .map(|g| {
                (
                    g.dir.clone(),
                    g.units.iter().map(|&i| p.units[i].name.as_str()).collect(),
                    g.deps.iter().map(String::as_str).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn layers_plain_and_generated_units() {
        let p = planned(&[
            ("auth", false, "use crate::model::U;"),
            ("model", false, ""),
            ("ui", true, "use crate::{auth, model};"),
            ("landing", true, "use crate::ui::X; use crate::auth;"),
            ("routes", true, "use crate::landing; use crate::ui;"),
        ])
        .unwrap();
        assert_eq!(
            dirs(&p),
            [
                ("base".into(), vec!["auth", "model"], vec![]),
                ("ui".into(), vec!["ui"], vec!["base"]),
                ("landing".into(), vec!["landing"], vec!["base", "ui"]),
                ("routes".into(), vec!["routes"], vec!["landing", "ui"]),
            ]
        );
    }

    #[test]
    fn a_unit_that_uses_the_root_stays_and_pins_what_uses_it() {
        let p = planned(&[
            ("util", false, ""),
            ("ui", true, "use crate::util;"),
            ("page", true, "use crate::App; use crate::ui;"),
            ("top", true, "use crate::page;"),
        ])
        .unwrap();
        assert_eq!(
            dirs(&p),
            [("base".into(), vec!["util"], vec![]), ("ui".into(), vec!["ui"], vec!["base"])]
        );
        assert!(p.group_of(2).is_none() && p.group_of(3).is_none());
    }

    #[test]
    fn cycles_share_a_crate_and_small_projects_stay_whole() {
        let p = planned(&[
            ("a", true, "use crate::b;"),
            ("b", true, "use crate::a;"),
            ("c", true, "use crate::a;"),
        ])
        .unwrap();
        assert_eq!(
            dirs(&p),
            [("a_b".into(), vec!["a", "b"], vec![]), ("c".into(), vec!["c"], vec!["a_b"])]
        );
        assert!(planned(&[("only", true, "")]).is_none());
        assert!(
            planned(&[("a", false, ""), ("b", false, "use crate::a;")]).is_none(),
            "plain units alone are one crate"
        );
    }

    #[test]
    fn parses_the_declarations_of_the_client_root() {
        assert_eq!(
            parse_client_mod("#[path = \"/p/src/auth.rs\"] mod auth;"),
            Some(("/p/src/auth.rs".into(), "", "auth".into()))
        );
        assert_eq!(
            parse_client_mod("#[path = \"/p/x.rs\"] pub mod x;").map(|x| x.1.to_string()),
            Some("pub ".into())
        );
        assert_eq!(parse_client_mod("mod auth;"), None);
    }
}
