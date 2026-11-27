//! Splits a project whose modules use each other in cycles. A crate can't be part of a cycle, so
//! when modules at the top of the tree do (`server` uses `i18n`, which uses `store`, which uses
//! `server`) they would all end up in one crate, and every edit to any of them would rebuild it
//! whole. The cycles are between modules, rarely between the files in them: here a module is cut
//! into its files, the files are put into crates in layers, and every crate rebuilds the module
//! tree it needs from `pub use` lines, so no source file needs a change.

use crate::manifest::Member;
use crate::split::{Plan, Unit, strongly_connected};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
struct Child {
    name: String,
    file: PathBuf,
    /// The attribute lines of the declaration except `#[path]`, each ending in a newline
    attrs: String,
}

/// A file of the project that goes into a crate as a whole
#[derive(Debug)]
struct Node {
    unit: usize,
    /// `None` for the root file of a module, or for a module that is one file
    child: Option<String>,
    file: PathBuf,
    text: String,
    /// The node of a split module that declares the others
    root: bool,
}

pub struct Refined {
    nodes: Vec<Node>,
    /// The `mod x;` lines of the roots of split modules
    children: BTreeMap<usize, Vec<Child>>,
    /// Node of a unit's root (or of the whole module when it isn't split)
    unit_root: BTreeMap<usize, usize>,
    /// Nodes of the children of a split module
    unit_children: BTreeMap<usize, BTreeMap<String, usize>>,
    node_crate: Vec<usize>,
    crates: Vec<RCrate>,
    pkg: String,
    units: Vec<Unit>,
    macros: Vec<Vec<String>>,
    macro_owner: BTreeMap<String, usize>,
    pub files: Vec<(PathBuf, String)>,
    pub members: Vec<Member>,
}

struct RCrate {
    dir: String,
    nodes: Vec<usize>,
    needs: BTreeSet<usize>,
}

fn read(file: &Path) -> String {
    crate::split::pending_text(file).or_else(|| fs::read_to_string(file).ok()).unwrap_or_default()
}

fn ident_at(s: &str) -> usize {
    s.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').map(char::len_utf8).sum()
}

/// The paths after every `marker` in `text` (`crate::` or `super::`), the items of a `{...}` group
/// as paths of their own; a glob ends the path
fn paths_after(text: &str, marker: &str) -> Vec<Vec<String>> {
    fn tree(s: &str, prefix: &[String], out: &mut Vec<Vec<String>>) -> usize {
        let mut i = 0;
        let mut path = prefix.to_vec();
        loop {
            let rest = &s[i..];
            let skipped = rest.len() - rest.trim_start().len();
            i += skipped;
            let rest = &s[i..];
            if let Some(r) = rest.strip_prefix('{') {
                i += 1;
                let mut at = 0;
                loop {
                    let r2 = &r[at..];
                    let sk = r2.len() - r2.trim_start().len();
                    at += sk;
                    if r[at..].starts_with('}') {
                        at += 1;
                        break;
                    }
                    if r[at..].starts_with(',') {
                        at += 1;
                        continue;
                    }
                    if at >= r.len() {
                        break;
                    }
                    let used = tree(&r[at..], &path, out);
                    if used == 0 {
                        at += 1;
                    } else {
                        at += used;
                    }
                }
                return i + at;
            }
            if rest.starts_with('*') {
                out.push(path);
                return i + 1;
            }
            let n = ident_at(rest);
            if n == 0 {
                if path.len() > prefix.len() {
                    out.push(path);
                }
                return i;
            }
            let name = rest[..n].to_string();
            i += n;
            path.push(name);
            if s[i..].starts_with("::") {
                i += 2;
                continue;
            }
            if s[i..].trim_start().starts_with("::") {
                i += s[i..].len() - s[i..].trim_start().len() + 2;
                continue;
            }
            if s[i..].trim_start().starts_with(" as ") || s[i..].starts_with(" as ") {
                i += s[i..].find(" as ").map_or(0, |p| p + 4);
                let a = ident_at(&s[i..]);
                i += a;
            }
            out.push(path);
            return i;
        }
    }
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let before = rest[..at].chars().next_back();
        let after = &rest[at + marker.len()..];
        rest = after;
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == ':') {
            continue;
        }
        tree(after, &[], &mut out);
    }
    out
}

/// The `mod x;` declarations of a module's file; `None` when something about them can't be followed
fn children_of(file: &Path, text: &str) -> Option<Vec<Child>> {
    let dir = file.parent()?;
    let home = if file.file_name().is_some_and(|n| n == "mod.rs") {
        dir.to_path_buf()
    } else {
        dir.join(file.file_stem()?)
    };
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut attrs = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#[path = \"") {
            path = Some(rest.strip_suffix("\"]")?.to_string());
            continue;
        }
        if t.starts_with("#[") {
            attrs.push_str(t);
            attrs.push('\n');
            continue;
        }
        let rest = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub(super) "))
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        if let Some(decl) = rest.strip_prefix("mod ") {
            if let Some(name) = decl.trim().strip_suffix(';') {
                let name = name.trim().to_string();
                let found = match path.take() {
                    Some(p) => {
                        let p = PathBuf::from(p);
                        Some(if p.is_absolute() { p } else { dir.join(p) })
                    }
                    None => [home.join(format!("{name}.rs")), home.join(&name).join("mod.rs")]
                        .into_iter()
                        .find(|f| f.is_file() || crate::split::pending_text(f).is_some()),
                };
                let file = found?;
                out.push(Child { name, file, attrs: std::mem::take(&mut attrs) });
                continue;
            }
            // an inline module keeps its own files
            return None;
        }
        if !t.is_empty() && !t.starts_with("//") {
            path = None;
            attrs.clear();
        }
    }
    Some(out)
}

fn stateful(text: &str) -> bool {
    text.lines().filter(|l| !l.contains("__YAWN_REGISTER")).any(|l| {
        let t = l.trim_start();
        let t = t.strip_prefix("pub ").or_else(|| t.strip_prefix("pub(crate) ")).unwrap_or(t);
        (t.starts_with("static ") && !t.starts_with("static ref") && {
            let ty = t.split_once(':').map_or("", |(_, r)| r.trim_start());
            !(ty.starts_with('&')
                || ty.starts_with('[') && !ty.contains("Mutex")
                || ty.starts_with("fn"))
        }) || t.contains("thread_local!")
            || t.contains("lazy_static!")
            || t.contains("LazyLock<")
            || t.contains("OnceLock<")
            || t.contains("OnceCell<")
            || t.starts_with("static mut ")
    })
}

fn defined_types(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t)
            .trim_start();
        for kw in ["struct ", "enum ", "trait ", "type ", "union "] {
            if let Some(rest) = t.strip_prefix(kw) {
                let n = ident_at(rest.trim_start());
                if n > 0 {
                    out.push(rest.trim_start()[..n].to_string());
                }
            }
        }
    }
    out
}

/// The identifiers of the header of every `impl`, and whether the impl is of a trait
fn impl_headers(text: &str) -> Vec<(Vec<String>, bool)> {
    let mut out = Vec::new();
    for l in text.lines() {
        let t = l.trim_start();
        let Some(rest) = t.strip_prefix("impl") else { continue };
        if !rest.starts_with(['<', ' ']) {
            continue;
        }
        let head = rest.split('{').next().unwrap_or(rest);
        let head = head.split(" where ").next().unwrap_or(head);
        let mut depth = 0;
        let mut flat = String::new();
        for c in head.chars() {
            match c {
                '<' => {
                    depth += 1;
                    flat.push(' ');
                }
                '>' => {
                    depth -= 1;
                    flat.push(' ');
                }
                _ if depth > 0 => flat.push(' '),
                c => flat.push(c),
            }
        }
        let is_trait = flat.contains(" for ");
        let idents = flat
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|s| !s.is_empty() && *s != "for" && *s != "dyn" && *s != "mut")
            .map(str::to_string)
            .collect();
        out.push((idents, is_trait));
    }
    out
}

/// Cuts the modules of `plan` that sit in cycles into files and lays the files out in crates.
/// `None` when nothing is in a cycle or the project can't be cut
pub fn refine(root: &Path, plan: &Plan, macros: &[Vec<String>]) -> Option<Refined> {
    if !plan.groups.iter().any(|g| g.units.len() > 1) {
        return None;
    }
    let movable: BTreeSet<usize> =
        plan.groups.iter().flat_map(|g| g.units.iter().copied()).collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut children: BTreeMap<usize, Vec<Child>> = BTreeMap::new();
    let mut unit_root = BTreeMap::new();
    let mut unit_children: BTreeMap<usize, BTreeMap<String, usize>> = BTreeMap::new();
    for &u in &movable {
        let unit = &plan.units[u];
        let text = read(&unit.file);
        let kids = (unit.name != "routes" && macros[u].is_empty())
            .then(|| children_of(&unit.file, &text))
            .flatten()
            .filter(|k| !k.is_empty());
        let kids = kids.filter(|k| {
            let texts: Vec<String> = k
                .iter()
                .map(|c| Unit { file: c.file.clone(), ..unit.clone() }.source_text())
                .collect();
            !text.contains("include!")
                && !text.contains("include_str!")
                && !texts.iter().chain(std::iter::once(&text)).any(|t| {
                    t.contains("pub(crate)")
                        || t.contains("pub(super)")
                        || t.contains("pub(in ")
                        || t.contains("super::super::")
                        || t.contains("#[macro_use]")
                        || t.contains("#[macro_export")
                })
        });
        match kids {
            Some(kids) => {
                let r = nodes.len();
                nodes.push(Node {
                    unit: u,
                    child: None,
                    file: unit.file.clone(),
                    text,
                    root: true,
                });
                unit_root.insert(u, r);
                let mut map = BTreeMap::new();
                for c in &kids {
                    map.insert(c.name.clone(), nodes.len());
                    let text = Unit { file: c.file.clone(), ..unit.clone() }.source_text();
                    nodes.push(Node {
                        unit: u,
                        child: Some(c.name.clone()),
                        file: c.file.clone(),
                        text,
                        root: false,
                    });
                }
                unit_children.insert(u, map);
                children.insert(u, kids);
            }
            None => {
                unit_root.insert(u, nodes.len());
                nodes.push(Node {
                    unit: u,
                    child: None,
                    file: unit.file.clone(),
                    text: unit.source_text(),
                    root: false,
                });
            }
        }
    }
    let by_name: BTreeMap<&str, usize> =
        movable.iter().map(|&u| (plan.units[u].name.as_str(), u)).collect();
    let macro_owner: BTreeMap<String, usize> =
        movable.iter().flat_map(|&u| macros[u].iter().map(move |m| (m.clone(), u))).collect();
    let n = nodes.len();
    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let resolve = |path: &[String]| -> Option<usize> {
        let first = path.first()?;
        let u = by_name.get(first.as_str()).copied().or_else(|| macro_owner.get(first).copied())?;
        match unit_children.get(&u) {
            Some(map) => match path.get(1).and_then(|s| map.get(s)) {
                Some(&c) => Some(c),
                None => unit_root.get(&u).copied(),
            },
            None => unit_root.get(&u).copied(),
        }
    };
    for i in 0..n {
        let unit = nodes[i].unit;
        let text = nodes[i].text.clone();
        for p in paths_after(&text, "crate::") {
            if let Some(j) = resolve(&p)
                && j != i
            {
                deps[i].insert(j);
            }
        }
        if nodes[i].child.is_some() {
            let map = &unit_children[&unit];
            for p in paths_after(&text, "super::") {
                let target = match p.first().and_then(|s| map.get(s)) {
                    Some(&c) => c,
                    None => unit_root[&unit],
                };
                if target != i {
                    deps[i].insert(target);
                }
            }
            if text.contains("use super::*") || text.contains("super::{") && text.contains("self") {
                deps[i].insert(unit_root[&unit]);
            }
        }
        if nodes[i].root {
            deps[i].extend(unit_children[&unit].values().copied());
        }
    }
    let mut defs: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        for name in defined_types(&node.text) {
            defs.entry(name).or_default().insert(i);
        }
    }
    let mut glue: Vec<(usize, usize)> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        for (idents, _) in impl_headers(&node.text) {
            let known: Vec<&BTreeSet<usize>> =
                idents.iter().filter_map(|id| defs.get(id)).collect();
            if known.is_empty() || known.iter().any(|set| set.contains(&i)) {
                continue;
            }
            for set in known {
                for &j in set {
                    glue.push((i, j));
                }
            }
        }
    }
    for (a, b) in glue {
        deps[a].insert(b);
        deps[b].insert(a);
    }
    let all: Vec<usize> = (0..n).collect();
    let comps = strongly_connected(&all, &deps);
    let mut comp_of = vec![0usize; n];
    for (c, comp) in comps.iter().enumerate() {
        for &v in comp {
            comp_of[v] = c;
        }
    }
    let comp_deps: Vec<BTreeSet<usize>> = comps
        .iter()
        .enumerate()
        .map(|(c, comp)| {
            comp.iter()
                .flat_map(|&v| deps[v].iter().map(|&d| comp_of[d]))
                .filter(|&d| d != c)
                .collect()
        })
        .collect();
    let mut level: Vec<Option<usize>> = vec![None; comps.len()];
    fn level_of(c: usize, deps: &[BTreeSet<usize>], memo: &mut Vec<Option<usize>>) -> usize {
        if let Some(l) = memo[c] {
            return l;
        }
        let l = deps[c].iter().map(|&d| level_of(d, deps, memo) + 1).max().unwrap_or(0);
        memo[c] = Some(l);
        l
    }
    let comp_state: Vec<bool> =
        comps.iter().map(|comp| comp.iter().any(|&v| stateful(&nodes[v].text))).collect();
    let mut crate_of: BTreeMap<(usize, bool), usize> = BTreeMap::new();
    let mut crates: Vec<RCrate> = Vec::new();
    let mut node_crate = vec![0usize; n];
    let mut order: Vec<usize> = (0..comps.len()).collect();
    let levels: Vec<usize> =
        (0..comps.len()).map(|c| level_of(c, &comp_deps, &mut level)).collect();
    order.sort_by_key(|&c| (levels[c], comps[c][0]));
    for c in order {
        let key = (levels[c], comp_state[c]);
        let k = *crate_of.entry(key).or_insert_with(|| {
            crates.push(RCrate {
                dir: if key.1 {
                    format!("layer{}_state", key.0)
                } else {
                    format!("layer{}", key.0)
                },
                nodes: Vec::new(),
                needs: BTreeSet::new(),
            });
            crates.len() - 1
        });
        for &v in &comps[c] {
            crates[k].nodes.push(v);
            node_crate[v] = k;
        }
    }
    for (k, cr) in crates.iter_mut().enumerate() {
        for &v in &cr.nodes {
            cr.needs.extend(deps[v].iter().copied().filter(|&d| node_crate[d] != k));
        }
        cr.nodes.sort();
    }
    let pkg = crate::split::package_ident_of(root);
    let mut refined = Refined {
        nodes,
        children,
        unit_root,
        unit_children,
        node_crate,
        crates,
        pkg,
        units: plan.units.clone(),
        macros: macros.to_vec(),
        macro_owner,
        files: Vec::new(),
        members: Vec::new(),
    };
    for k in 0..refined.crates.len() {
        let own = refined.crates[k].nodes.clone();
        let needs = refined.crates[k].needs.clone();
        let dir = refined.crates[k].dir.clone();
        let (text, shadows) = refined.facade(k, &own, &needs);
        let base = root.join(crate::split::CRATES_DIR).join(&dir);
        refined.files.push((base.join("lib.rs"), header(text)));
        for (name, body) in shadows {
            refined.files.push((base.join(name), body));
        }
        let deps = needs.iter().map(|&d| refined.crates[refined.node_crate[d]].dir.clone());
        let deps: BTreeSet<String> = deps.collect();
        refined.members.push(Member { dir, deps: deps.into_iter().collect() });
    }
    Some(refined)
}

fn header(body: String) -> String {
    format!(
        "// @generated by yawn-rsx: a crate of the project's modules (see yawn_rsx::split_nodes). Do not edit.\n#![allow(unused_imports)]\n{body}"
    )
}

impl Refined {
    pub fn crate_ident(&self, k: usize) -> String {
        crate::manifest::member_package(&self.pkg, &self.crates[k].dir).replace('-', "_")
    }

    /// The crate that holds the module `unit`'s root, which is where its name is taken from
    pub fn unit_crate(&self, unit: usize) -> Option<String> {
        self.unit_root.get(&unit).map(|&n| self.crate_ident(self.node_crate[n]))
    }

    pub fn unit_dir(&self, unit: usize) -> Option<String> {
        self.unit_root.get(&unit).map(|&n| self.crates[self.node_crate[n]].dir.clone())
    }

    pub fn macros_of(&self, unit: usize) -> &[String] {
        &self.macros[unit]
    }

    /// The `pub mod` and `pub use` lines a crate that owns `own` and uses `needs` starts with, and
    /// the files of its own they refer to
    fn facade(
        &self,
        k: usize,
        own: &[usize],
        needs: &BTreeSet<usize>,
    ) -> (String, Vec<(String, String)>) {
        let mut text = String::new();
        let mut shadows = Vec::new();
        let mut units: BTreeMap<usize, ()> = BTreeMap::new();
        for &v in own.iter().chain(needs.iter()) {
            units.insert(self.nodes[v].unit, ());
        }
        let owned: BTreeSet<usize> = own.iter().copied().collect();
        for &u in units.keys() {
            let unit = &self.units[u];
            let root = self.unit_root[&u];
            let kids = self.unit_children.get(&u);
            let present = |n: usize| owned.contains(&n) || needs.contains(&n);
            let ident = |n: usize| self.crate_ident(self.node_crate[n]);
            match kids {
                None => {
                    if owned.contains(&root) {
                        text.push_str(&format!(
                            "{}#[path = {:?}]\npub mod {};\n",
                            unit.attrs,
                            crate::project::slashed(&self.nodes[root].file),
                            unit.name
                        ));
                    } else {
                        text.push_str(&format!(
                            "{}pub use {}::{};\n",
                            unit.attrs,
                            ident(root),
                            unit.name
                        ));
                        for m in &self.macros[u] {
                            text.push_str(&format!(
                                "{}pub use {}::{m};\n",
                                unit.attrs,
                                ident(root)
                            ));
                        }
                    }
                }
                Some(map) => {
                    let decls = &self.children[&u];
                    if owned.contains(&root) {
                        let all_here = map.values().all(|n| owned.contains(n));
                        if all_here {
                            text.push_str(&format!(
                                "{}#[path = {:?}]\npub mod {};\n",
                                unit.attrs,
                                crate::project::slashed(&self.nodes[root].file),
                                unit.name
                            ));
                        } else {
                            let file = format!("{}_root.rs", unit.name);
                            shadows.push((file.clone(), self.shadow_root(u, k, &owned)));
                            let dir =
                                format!("{}/{}", crate::split::CRATES_DIR, self.crates[k].dir);
                            let _ = dir;
                            text.push_str(&format!(
                                "{}#[path = \"{file}\"]\npub mod {};\n",
                                unit.attrs, unit.name
                            ));
                        }
                    } else {
                        text.push_str(&format!("{}pub mod {} {{\n", unit.attrs, unit.name));
                        for d in decls {
                            let n = map[&d.name];
                            if owned.contains(&n) {
                                text.push_str(&format!(
                                    "{}#[path = {:?}]\npub mod {};\n",
                                    d.attrs,
                                    crate::project::slashed(&d.file),
                                    d.name
                                ));
                            } else if present(n) {
                                text.push_str(&format!(
                                    "{}pub use {}::{}::{};\n",
                                    d.attrs,
                                    ident(n),
                                    unit.name,
                                    d.name
                                ));
                            }
                        }
                        if needs.contains(&root) {
                            text.push_str(&format!("pub use {}::{}::*;\n", ident(root), unit.name));
                        }
                        text.push_str("}\n");
                    }
                }
            }
        }
        (text, shadows)
    }

    /// The root file of a module whose files are in several crates: the `mod x;` lines of the files
    /// that moved say where they are now
    fn shadow_root(&self, u: usize, k: usize, owned: &BTreeSet<usize>) -> String {
        let root = &self.nodes[self.unit_root[&u]];
        let unit = &self.units[u];
        let map = &self.unit_children[&u];
        let decls: BTreeMap<&str, &Child> =
            self.children[&u].iter().map(|c| (c.name.as_str(), c)).collect();
        let mut out = String::new();
        let mut pending: Vec<String> = Vec::new();
        for line in root.text.lines() {
            let t = line.trim();
            if t.starts_with("#[") {
                pending.push(line.to_string());
                continue;
            }
            let body = t
                .strip_prefix("pub(crate) ")
                .or_else(|| t.strip_prefix("pub(super) "))
                .or_else(|| t.strip_prefix("pub "))
                .unwrap_or(t);
            let decl = body.strip_prefix("mod ").and_then(|r| r.trim().strip_suffix(';'));
            match decl.map(str::trim).and_then(|name| decls.get(name).map(|c| (name, *c))) {
                Some((name, c)) => {
                    pending.clear();
                    out.push_str(&c.attrs);
                    let n = map[name];
                    if owned.contains(&n) {
                        out.push_str(&format!(
                            "#[path = {:?}]\npub mod {name};\n",
                            crate::project::slashed(&c.file)
                        ));
                    } else {
                        out.push_str(&format!(
                            "pub use {}::{}::{name};\n",
                            self.crate_ident(self.node_crate[n]),
                            unit.name
                        ));
                    }
                }
                None => {
                    for p in pending.drain(..) {
                        out.push_str(&p);
                        out.push('\n');
                    }
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        let _ = k;
        out
    }

    /// The nodes a source file of the project uses, `None` when it uses something that has to stay
    /// in the root of the app
    pub fn needs_of(&self, source: &str) -> Option<BTreeSet<usize>> {
        let by_name: BTreeMap<&str, usize> =
            self.unit_root.keys().map(|&u| (self.units[u].name.as_str(), u)).collect();
        let mut out = BTreeSet::new();
        for p in paths_after(source, "crate::") {
            let first = p.first()?;
            let u = by_name
                .get(first.as_str())
                .copied()
                .or_else(|| self.macro_owner.get(first).copied())?;
            let n = match self.unit_children.get(&u).and_then(|m| p.get(1).and_then(|s| m.get(s))) {
                Some(&c) => c,
                None => self.unit_root[&u],
            };
            out.insert(n);
        }
        Some(out)
    }

    /// The re-exports a crate of one route needs for the files `needs` and the directories of the
    /// crates they live in
    pub fn route_facade(&self, needs: &BTreeSet<usize>) -> (String, Vec<String>) {
        let mut text = String::new();
        let dirs: BTreeSet<usize> = needs.iter().map(|&n| self.node_crate[n]).collect();
        let (body, _) = self.facade(usize::MAX, &[], needs);
        text.push_str(&body);
        (text, dirs.into_iter().map(|k| self.crates[k].dir.clone()).collect())
    }
}
