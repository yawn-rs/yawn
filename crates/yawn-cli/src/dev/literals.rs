use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use syn::visit_mut::{self, VisitMut};

#[derive(Default)]
struct Literals(Vec<(String, String)>);

struct Listeners;

impl VisitMut for Listeners {
    fn visit_expr_call_mut(&mut self, call: &mut syn::ExprCall) {
        visit_mut::visit_expr_call_mut(self, call);
        let syn::Expr::Path(path) = &*call.func else {
            return;
        };
        let names: Vec<_> = path.path.segments.iter().map(|s| s.ident.to_string()).collect();
        if matches!(names.as_slice(), [rt, f] if rt == "__rt" && f == "listen")
            && let Some(handler) = call.args.iter_mut().nth(2)
        {
            *handler = syn::parse_quote!(());
        }
    }
}

impl VisitMut for Literals {
    fn visit_expr_call_mut(&mut self, call: &mut syn::ExprCall) {
        visit_mut::visit_expr_call_mut(self, call);
        let syn::Expr::Path(path) = &*call.func else {
            return;
        };
        let names: Vec<_> = path.path.segments.iter().map(|s| s.ident.to_string()).collect();
        let (kind, index) = match names.as_slice() {
            [rt, f] if rt == "__rt" && (f == "template" || f == "template_holes") => {
                ("template".to_string(), 0)
            }
            [rt, f] if rt == "__rt" && f == "text_lit" => ("text".to_string(), 0),
            [rt, f] if rt == "__rt" && f == "attr_lit" => {
                let Some(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(name), .. })) =
                    call.args.iter().nth(1)
                else {
                    return;
                };
                (format!("attr:{}", name.value()), 2)
            }
            _ => return,
        };
        if let Some(syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(value), .. })) =
            call.args.iter_mut().nth(index)
        {
            self.0.push((kind, value.value()));
            *value = syn::LitStr::new("", value.span());
        }
    }
}

#[derive(Default, Clone)]
pub struct Snapshot(BTreeMap<PathBuf, (String, Vec<(String, String)>)>, BTreeMap<PathBuf, String>);

impl Snapshot {
    pub fn read(root: &Path) -> Option<Self> {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Option<()> {
            for entry in std::fs::read_dir(dir).ok()? {
                let path = entry.ok()?.path();
                if path.is_dir() {
                    walk(&path, files)?;
                } else if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
            Some(())
        }
        let mut files = Vec::new();
        walk(&root.join(".yawn/rsx"), &mut files)?;
        walk(&root.join("src"), &mut files)?;
        if root.join(".yawn/client.rs").is_file() {
            files.push(root.join(".yawn/client.rs"));
        }
        let mut out = Self::default();
        for path in files {
            let source = std::fs::read_to_string(&path).ok()?;
            out.1.insert(path.clone(), server_form(&source)?);
            out.0.insert(path, normalize(&source)?);
        }
        Some(out)
    }

    pub fn replacements(&self, next: &Self) -> Option<BTreeMap<String, String>> {
        self.diff(next, false)
    }

    /// The literals that changed for the server, or `None` if anything but literals changed there
    pub fn server_replacements(&self, next: &Self) -> Option<BTreeMap<String, String>> {
        self.diff(next, true)
    }

    fn diff(&self, next: &Self, server: bool) -> Option<BTreeMap<String, String>> {
        if self.0.keys().ne(next.0.keys()) {
            return None;
        }
        let mut replacements = BTreeMap::new();
        let mut unchanged = BTreeSet::new();
        for (path, (code, values)) in &self.0 {
            let (next_code, next_values) = next.0.get(path)?;
            let same =
                if server { self.1.get(path)? == next.1.get(path)? } else { code == next_code };
            if !same || values.len() != next_values.len() {
                return None;
            }
            for ((kind, old), (next_kind, new)) in values.iter().zip(next_values) {
                if kind != next_kind {
                    return None;
                }
                let key = format!("{kind}:{old}");
                if old == new {
                    unchanged.insert(key.clone());
                }
                if let Some(previous) = replacements.insert(key, new.clone())
                    && previous != *new
                {
                    return None;
                }
            }
        }
        replacements.retain(|key, _| !unchanged.contains(key));
        Some(replacements)
    }
}

fn server_form(source: &str) -> Option<String> {
    let mut file = syn::parse_file(source).ok()?;
    Listeners.visit_file_mut(&mut file);
    let mut literals = Literals::default();
    literals.visit_file_mut(&mut file);
    Some(file.into_token_stream().to_string())
}

fn normalize(source: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut file = syn::parse_file(source).ok()?;
    let mut literals = Literals::default();
    literals.visit_file_mut(&mut file);
    Some((file.into_token_stream().to_string(), literals.0))
}

pub fn write_server(dev_dir: &Path, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    store(dev_dir, "literals-server.json", values)
}

pub fn write(dev_dir: &Path, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    store(dev_dir, "literals.json", values)
}

fn store(dev_dir: &Path, name: &str, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    std::fs::create_dir_all(dev_dir)?;
    let temp = dev_dir.join(format!("{name}.tmp"));
    std::fs::write(&temp, serde_json::to_vec(values)?)?;
    std::fs::rename(temp, dev_dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(source: &str) -> Snapshot {
        Snapshot(
            BTreeMap::from([(PathBuf::from("a.rs"), normalize(source).unwrap())]),
            BTreeMap::from([(PathBuf::from("a.rs"), server_form(source).unwrap())]),
        )
    }
    #[test]
    fn a_handler_is_not_a_change_of_what_the_server_renders() {
        let a = snapshot(
            "fn f() { __rt::listen(n, \"click\", move || one()); __rt::text_lit(\"a\"); }",
        );
        let b = snapshot(
            "fn f() { __rt::listen(n, \"click\", move || two()); __rt::text_lit(\"b\"); }",
        );
        assert!(a.replacements(&b).is_none());
        assert_eq!(a.server_replacements(&b).unwrap()["text:a"], "b");
        let c = snapshot(
            "fn f() { __rt::listen(n, \"click\", move || one()); __rt::text_lit(\"a\"); other(); }",
        );
        assert!(a.server_replacements(&c).is_none());
    }

    #[test]
    fn patches_from_compiled_baseline_and_rejects_logic_and_conflicts() {
        let a = snapshot(
            "fn f() { __rt::text_lit(\"a\"); __rt::template_holes(\"<b></b>\", &[&[]]); }",
        );
        let b = snapshot(
            "fn f() { __rt::text_lit(\"b\"); __rt::template_holes(\"<i></i>\", &[&[]]); }",
        );
        let patch = a.replacements(&b).unwrap();
        assert_eq!(patch["text:a"], "b");
        assert_eq!(patch["template:<b></b>"], "<i></i>");
        assert!(a.replacements(&a).unwrap().is_empty());
        let c = snapshot(
            "fn f() { __rt::text_lit(\"c\"); __rt::template_holes(\"<i></i>\", &[&[]]); }",
        );
        assert_eq!(a.replacements(&c).unwrap()["text:a"], "c");
        let attrs = snapshot("fn f() { __rt::attr_lit(n, \"class\", \"old\"); }");
        assert!(attrs.replacements(&attrs).unwrap().is_empty());
        let next_attrs = snapshot("fn f() { __rt::attr_lit(n, \"class\", \"new\"); }");
        assert_eq!(attrs.replacements(&next_attrs).unwrap()["attr:class:old"], "new");
        assert!(a.replacements(&snapshot("fn f() { __rt::text_lit(\"b\"); other(); }")).is_none());
        assert!(
            a.replacements(&snapshot(
                "fn f() { __rt::text_lit(\"b\"); __rt::template_holes(\"<i></i>\", &[&[1]]); }"
            ))
            .is_none()
        );
        let duplicate = snapshot("fn f() { __rt::text_lit(\"a\"); __rt::text_lit(\"a\"); }");
        assert!(
            duplicate
                .replacements(&snapshot("fn f() { __rt::text_lit(\"b\"); __rt::text_lit(\"a\"); }"))
                .is_none()
        );
    }
}
