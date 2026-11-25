use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use yawn_rsx::parser::{Header, headers};

#[derive(Default)]
pub struct Index {
    files: HashMap<PathBuf, Vec<Header>>,
}

impl Index {
    pub fn set(&mut self, file: PathBuf, text: &str) {
        self.files.insert(file, headers(text));
    }

    pub fn scan(&mut self, root: &Path) {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rsx") {
                    out.push(p);
                }
            }
        }
        let mut found = Vec::new();
        walk(&root.join("src"), &mut found);
        for p in found {
            if !self.files.contains_key(&p)
                && let Ok(text) = fs::read_to_string(&p)
            {
                self.set(p, &text);
            }
        }
    }

    pub fn components(&self) -> impl Iterator<Item = &Header> {
        self.files.values().flatten()
    }

    /// The file declaring `component name` and the byte offset of the name in it
    pub fn definition(
        &self,
        name: &str,
        open: &dyn Fn(&Path) -> Option<String>,
    ) -> Option<(PathBuf, String, usize)> {
        let (path, _) = self.files.iter().find(|(_, hs)| hs.iter().any(|h| h.name == name))?;
        let text = open(path).or_else(|| fs::read_to_string(path).ok())?;
        let mut from = 0;
        while let Some(n) = text[from..].find("component") {
            let at = from + n;
            let after = text[at + 9..].trim_start();
            let name_at = text.len() - after.len();
            let boundary_before = !text[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            if boundary_before
                && text[at + 9..].starts_with(char::is_whitespace)
                && after.starts_with(name)
                && !after[name.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
            {
                return Some((path.clone(), text, name_at));
            }
            from = at + 9;
        }
        None
    }

    /// `crate::ui::card::Card` for a component of the project at `root`
    pub fn path_of(&self, name: &str, root: &Path) -> Option<String> {
        let (path, _) = self
            .files
            .iter()
            .find(|(p, hs)| p.starts_with(root) && hs.iter().any(|h| h.name == name))?;
        let rel = path.strip_prefix(root.join("src")).ok()?.with_extension("");
        if rel == Path::new("main") {
            return Some(format!("crate::{name}")); // main.rsx is the crate root
        }
        if rel.starts_with(yawn_rsx::routes::DIR) && yawn_rsx::project::folders_enabled(root) {
            // `[id].rsx` is module `param_id`, and so on
            let stem = rel.to_string_lossy().replace('\\', "/");
            if let Ok(route) = yawn_rsx::routes::analyze(&stem, name) {
                return Some(format!("crate::{}::{name}", route.module.join("::")));
            }
        }
        let module: Vec<String> =
            rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        Some(format!("crate::{}::{name}", module.join("::")))
    }

    /// The `.rsx` files known to the index
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.keys()
    }

    pub fn find(&self, name: &str) -> Option<&Header> {
        self.components().find(|h| h.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_headers_even_when_the_view_is_broken() {
        let mut ix = Index::default();
        ix.set("a.rsx".into(), "component Card(title: &'static str, n: Sig<i32>) {\n view { <p> }\n}\ncomponent B() { view { <b/> } }");
        let card = ix.find("Card").unwrap();
        assert_eq!(
            card.params,
            [("title".into(), "&'static str".into()), ("n".into(), "Sig<i32>".into())]
        );
        assert!(ix.find("B").unwrap().params.is_empty());
    }

    #[test]
    fn keeps_the_doc_comment_above_a_component() {
        let mut ix = Index::default();
        ix.set("a.rsx".into(), "/// First line\n/// second\npub component Card() {\n view { <p/> }\n}\n// plain\ncomponent B() { view { <b/> } }\n\n/// detached\n\ncomponent C() { view { <b/> } }\n/// one\ncomponent D() { view { <b/> } }");
        assert_eq!(ix.find("Card").unwrap().doc, "First line\nsecond");
        assert_eq!(ix.find("B").unwrap().doc, "");
        assert_eq!(ix.find("C").unwrap().doc, "");
        assert_eq!(ix.find("D").unwrap().doc, "one");
    }
}
