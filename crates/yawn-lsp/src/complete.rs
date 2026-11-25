use crate::context::Ctx;
use crate::index::Index;
use crate::position::Text;
use serde_json::{Value, json};
use yawn_rsx::parser::VOID_TAGS;

fn item(
    text: &Text,
    start: usize,
    offset: usize,
    label: &str,
    kind: u32,
    detail: Option<String>,
    insert: String,
) -> Value {
    json!({
        "label": label,
        "kind": kind,
        "detail": detail,
        "insertTextFormat": 2,
        "filterText": label,
        "textEdit": {"range": {"start": text.position(start.min(offset)), "end": text.position(offset)}, "newText": insert},
    })
}

/// Prop completions for a component: one item per field, plus "Fill all props" with a tab stop for
/// each
pub fn prop_items(
    text: &Text,
    start: usize,
    offset: usize,
    fields: &[(String, String)],
) -> Vec<Value> {
    let mut out: Vec<Value> = fields
        .iter()
        .map(|(n, t)| {
            item(
                text,
                start,
                offset,
                n,
                KIND_FIELD,
                Some(t.clone()),
                format!("{n}={{{}}}", placeholder(1, t)),
            )
        })
        .collect();
    if !fields.is_empty() {
        let all: Vec<String> = fields
            .iter()
            .enumerate()
            .map(|(i, (n, t))| format!("{n}={{{}}}", placeholder(i + 1, t)))
            .collect();
        let names = fields.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ");
        let mut it =
            item(text, start, offset, "Fill all props", KIND_KEYWORD, Some(names), all.join(" "));
        it["sortText"] = json!("0");
        it["filterText"] = json!("");
        out.insert(0, it);
    }
    out
}

fn placeholder(n: usize, ty: &str) -> String {
    if ty.is_empty() {
        return format!("${n}");
    }
    let esc: String = ty
        .chars()
        .flat_map(|c| if matches!(c, '\\' | '$' | '}') { vec!['\\', c] } else { vec![c] })
        .collect();
    format!("${{{n}:{esc}}}")
}

/// Components of `yawn-router`, offered (with their `use`) in projects that depend on it
pub const ROUTER_COMPONENTS: &[(&str, &str)] =
    &[("Router", "Router $0/>"), ("Outlet", "Outlet />"), ("Link", "Link href=\"$1\">$0</Link>")];

/// Uppercase names a file brings in with `use` (`use a::b::{C, D as E};` -> `C`, `E`)
pub fn imported_names(src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for stmt in src.split(';') {
        let stmt = stmt.trim_start();
        let Some(body) = stmt.strip_prefix("use ").or_else(|| stmt.strip_prefix("pub use ")) else {
            continue;
        };
        if body.contains('(') || body.contains("component ") {
            continue; // ran past the `use` into other code
        }
        let mut after_as = false;
        let mut chars = body.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if !(c.is_alphabetic() || c == '_') {
                continue;
            }
            let mut end = i + c.len_utf8();
            while let Some(&(j, d)) = chars.peek() {
                if d.is_alphanumeric() || d == '_' {
                    end = j + d.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let w = &body[i..end];
            if w == "as" {
                after_as = true;
            } else if after_as {
                if let Some(last) = out.last_mut() {
                    *last = w.to_string();
                }
                after_as = false;
            } else if !body[end..].trim_start().starts_with("::")
                && !matches!(w, "self" | "crate" | "super")
            {
                out.push(w.to_string());
            }
        }
    }
    out.retain(|n| n.starts_with(|c: char| c.is_uppercase()));
    out.sort();
    out.dedup();
    out
}

const KIND_KEYWORD: u32 = 14;
const KIND_CLASS: u32 = 7;
const KIND_FIELD: u32 = 5;
const KIND_EVENT: u32 = 23;

/// `html` are the items the HTML service returned for the same position (`None` when it isn't
/// available)
pub fn complete(
    ctx: &Ctx,
    text: &Text,
    offset: usize,
    index: &Index,
    html: Option<Vec<Value>>,
    imports: &std::collections::HashMap<String, String>,
) -> Vec<Value> {
    let mut html = html.unwrap_or_default();
    match ctx {
        Ctx::Tag { start } => {
            // The service completes `<div`; a Yawn element must be closed, so insert the pair.
            for it in &mut html {
                let Some(name) = it.get("label").and_then(Value::as_str).map(str::to_string) else {
                    continue;
                };
                let insert = if name == "slot" || VOID_TAGS.contains(&name.as_str()) {
                    format!("{name} $0/>")
                } else {
                    format!("{name}>$0</{name}>")
                };
                it["detail"] = json!("tag"); // the Zed extension colours it like a tag (editors/zed-rsx)
                if let Some(edit) = it.get_mut("textEdit") {
                    edit["newText"] = json!(insert);
                    it["insertTextFormat"] = json!(2);
                } else {
                    *it = item(text, *start, offset, &name, 10, None, insert);
                }
            }
            if !html.iter().any(|i| i["label"] == "slot") {
                html.push(item(
                    text,
                    *start,
                    offset,
                    "slot",
                    10,
                    Some("children of the component".into()),
                    "slot $0/>".into(),
                ));
            }
            for c in index.components() {
                let props: String = c
                    .params
                    .iter()
                    .filter(|(n, _)| n != "children")
                    .enumerate()
                    .map(|(i, (n, _))| format!(" {n}={{${}}}", i + 1))
                    .collect();
                let insert = if c.params.iter().any(|(n, _)| n == "children") {
                    format!("{}{props}>\n\t$0\n</{}>", c.name, c.name)
                } else {
                    format!("{}{props} />", c.name)
                };
                let sig = c
                    .params
                    .iter()
                    .map(|(n, t)| format!("{n}: {t}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut it = item(
                    text,
                    *start,
                    offset,
                    &c.name,
                    KIND_CLASS,
                    Some(format!("component ({sig})")),
                    insert,
                );
                if !c.doc.is_empty() {
                    it["documentation"] = json!({"kind": "markdown", "value": format!("```rust\ncomponent {}({sig})\n```\n\n---\n\n{}", c.name, c.doc)});
                }
                if let Some(line) = imports.get(&c.name) {
                    let (at, new_text) = crate::server::import_edit(text, line);
                    it["additionalTextEdits"] =
                        json!([{"range": {"start": at, "end": at}, "newText": new_text}]);
                    it["labelDetails"] = json!({"description": line.trim_start_matches("use ").trim_end_matches(';')});
                }
                html.push(it);
            }
            for (name, line) in imports {
                if index.find(name).is_none()
                    && let Some((_, insert)) = ROUTER_COMPONENTS.iter().find(|(n, _)| n == name)
                {
                    let mut it = item(
                        text,
                        *start,
                        offset,
                        name,
                        KIND_CLASS,
                        Some("component (yawn_router)".into()),
                        (*insert).to_string(),
                    );
                    let (at, new_text) = crate::server::import_edit(text, line);
                    it["additionalTextEdits"] =
                        json!([{"range": {"start": at, "end": at}, "newText": new_text}]);
                    it["labelDetails"] = json!({"description": line.trim_start_matches("use ").trim_end_matches(';')});
                    html.push(it);
                }
            }
            // components of other crates (`use yawn_router::Router;`): their props aren't known
            // here, rust-analyzer supplies them once the tag is typed
            for name in imported_names(&text.s) {
                if index.find(&name).is_none() {
                    let it = item(
                        text,
                        *start,
                        offset,
                        &name,
                        KIND_CLASS,
                        Some("component (imported)".into()),
                        format!("{name} $0/>"),
                    );
                    html.push(it);
                }
            }
            html
        }
        Ctx::Attr { tag, start } if tag.starts_with(|c: char| c.is_uppercase()) => index
            .find(tag)
            .map(|h| {
                let fields: Vec<(String, String)> =
                    h.params.iter().filter(|(n, _)| n != "children").cloned().collect();
                prop_items(text, *start, offset, &fields)
            })
            .unwrap_or_default(),
        Ctx::Attr { start, .. } => {
            for it in &mut html {
                it["detail"] = json!("attribute");
            }
            html.retain(|i| {
                !i["label"].as_str().is_some_and(|l| {
                    l.starts_with("on")
                        && l.len() > 2
                        && l[2..].chars().all(|c| c.is_ascii_lowercase())
                })
            });
            html.push(item(
                text,
                *start,
                offset,
                "on:",
                KIND_KEYWORD,
                Some("event listener".into()),
                "on:".into(),
            ));
            html.push(item(
                text,
                *start,
                offset,
                "class:",
                KIND_KEYWORD,
                Some("toggle a class".into()),
                "class:${1:name}={${2:condition}}".into(),
            ));
            html.push(item(
                text,
                *start,
                offset,
                "prop:",
                KIND_KEYWORD,
                Some("DOM property".into()),
                "prop:${1:value}={$2}".into(),
            ));
            html.push(item(
                text,
                *start,
                offset,
                "ref",
                KIND_KEYWORD,
                Some("handle to this element".into()),
                "ref={$1}".into(),
            ));
            html
        }
        // `on:` events are the DOM's `onclick`, `oninput`... attributes of the HTML data, minus `on`.
        Ctx::Event { start } => html
            .iter()
            .filter_map(|i| i["label"].as_str())
            .filter(|l| {
                l.starts_with("on") && l.len() > 2 && l[2..].chars().all(|c| c.is_ascii_lowercase())
            })
            .map(|l| {
                let name = &l[2..];
                let doc = html
                    .iter()
                    .find(|i| i["label"] == l)
                    .and_then(|i| i.get("documentation"))
                    .cloned();
                let mut it = item(
                    text,
                    *start,
                    offset,
                    name,
                    KIND_EVENT,
                    Some("attribute".into()),
                    format!("{name}={{move || $0}}"),
                );
                it["documentation"] = doc.unwrap_or(Value::Null);
                it
            })
            .collect(),
        Ctx::Children { start } => {
            let mut v = vec![
                item(
                    text,
                    *start,
                    offset,
                    "if",
                    KIND_KEYWORD,
                    Some("conditional block".into()),
                    "if ${1:condition} {\n\t$0\n}".into(),
                ),
                item(
                    text,
                    *start,
                    offset,
                    "if let",
                    KIND_KEYWORD,
                    Some("block while a pattern matches".into()),
                    "if let ${1:Some(value)} = ${2:expr} {\n\t$0\n}".into(),
                ),
                item(
                    text,
                    *start,
                    offset,
                    "for",
                    KIND_KEYWORD,
                    Some("keyed list".into()),
                    "for ${1:item} in ${2:items.get()} key ${3:item.id} {\n\t$0\n}".into(),
                ),
            ];
            v.extend(
                html.into_iter()
                    .filter(|i| i["label"].as_str().is_some_and(|l| l.starts_with("</"))),
            );
            v
        }
        Ctx::Class { start } => crate::tailwind::complete(text, *start, offset),
        Ctx::Css { .. } => html,
        Ctx::Rust | Ctx::None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_names_from_use_lines() {
        let src = "use yawn_router::{Link, Outlet as Slot};\nuse a::b::Router;\nuse std::fmt;\nuse x::*;\ncomponent A() { view { <b/> } }";
        assert_eq!(imported_names(src), vec!["Link", "Router", "Slot"]);
    }

    #[test]
    fn fill_all_has_a_tab_stop_per_prop() {
        let t = Text::new("<A  />".into());
        let items =
            prop_items(&t, 3, 3, &[("a".into(), "u32".into()), ("b".into(), "String".into())]);
        assert_eq!(items[0]["label"], "Fill all props");
        assert_eq!(items[0]["textEdit"]["newText"], "a={${1:u32}} b={${2:String}}");
        assert_eq!(placeholder(1, "Vec<u8>"), "${1:Vec<u8>}");
        assert_eq!(placeholder(2, ""), "$2");
        assert_eq!(items.len(), 3);
        assert!(prop_items(&t, 3, 3, &[]).is_empty());
    }
}
