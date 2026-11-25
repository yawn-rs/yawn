use super::*;
use crate::format;
use crate::symbols::{components, occurrences, word_at};
use lsp_server::ResponseError;
use serde_json::Map;

fn range_json(t: &Text, a: usize, b: usize) -> Value {
    json!(Range::new(t.position(a), t.position(b)))
}

impl Server {
    fn respond_error(&self, id: RequestId, message: &str) {
        self.to_client(Message::Response(Response {
            id,
            result: None,
            error: Some(ResponseError { code: -32803, message: message.into(), data: None }),
        }));
    }

    pub(super) fn rsx_range_request(&mut self, uri: &Url, r: &Request) -> bool {
        match r.method.as_str() {
            "textDocument/documentSymbol" => {
                let result = match self.docs.get(uri) {
                    Some(doc) => {
                        let t = &doc.text;
                        Value::Array(
                            components(&t.s)
                                .iter()
                                .map(|c| json!({"name": c.name, "detail": c.params, "kind": 5, "range": range_json(t, c.full.0, c.full.1), "selectionRange": range_json(t, c.name_at.0, c.name_at.1)}))
                                .collect(),
                        )
                    }
                    None => Value::Null,
                };
                self.respond(r.id.clone(), result);
                true
            }
            "textDocument/formatting" => {
                let result = match self.docs.get(uri) {
                    Some(doc) => {
                        let t = &doc.text;
                        Value::Array(format::format(&t.s).into_iter().map(|(a, b, text)| json!({"range": range_json(t, a, b), "newText": text})).collect())
                    }
                    None => Value::Null,
                };
                self.respond(r.id.clone(), result);
                true
            }
            "textDocument/codeAction" => {
                self.code_action(uri, r);
                true
            }
            "textDocument/inlayHint" | "textDocument/semanticTokens/full" => {
                let Some((guri, full)) =
                    self.docs.get(uri).and_then(|d| d.r#gen.as_ref()).map(|g| {
                        (
                            g.uri.clone(),
                            Range::new(Position::new(0, 0), g.text.position(g.text.s.len())),
                        )
                    })
                else {
                    self.respond(
                        r.id.clone(),
                        if r.method.ends_with("inlayHint") { json!([]) } else { Value::Null },
                    );
                    return true;
                };
                if self.ra_in.is_none() {
                    self.respond(
                        r.id.clone(),
                        if r.method.ends_with("inlayHint") { json!([]) } else { Value::Null },
                    );
                    return true;
                }
                let (kind, params) = if r.method.ends_with("inlayHint") {
                    (
                        Pending::InlayHints(uri.clone()),
                        json!({"textDocument": {"uri": guri}, "range": full}),
                    )
                } else {
                    (Pending::Tokens(uri.clone()), json!({"textDocument": {"uri": guri}}))
                };
                self.pending.insert(r.id.clone(), kind);
                self.send_ra(Message::Request(Request {
                    id: r.id.clone(),
                    method: r.method.clone(),
                    params,
                }));
                true
            }
            _ => false,
        }
    }

    fn code_action(&mut self, uri: &Url, r: &Request) {
        let range: Option<Range> =
            r.params.get("range").and_then(|v| serde_json::from_value(v.clone()).ok());
        let Some(range) = range else {
            return self.respond(r.id.clone(), json!([]));
        };
        let mut own = self.import_actions(uri, range);
        own.extend(self.prop_actions(uri, r));
        let forward = (|| {
            let doc = self.docs.get(uri)?;
            let g = doc.r#gen.as_ref()?;
            self.ra_in.as_ref()?;
            if !matches!(context(&doc.text.s, doc.text.offset(range.start)), Ctx::Rust) {
                return None;
            }
            let gs = src_to_gen(&g.map, &doc.text, &g.text, range.start)?;
            let ge = src_to_gen(&g.map, &doc.text, &g.text, range.end).unwrap_or(gs);
            let diags: Vec<&Diagnostic> = doc
                .ra_raw
                .iter()
                .filter(|d| {
                    map_diag((*d).clone(), g, &doc.text)
                        .is_some_and(|m| m.range.start <= range.end && m.range.end >= range.start)
                })
                .collect();
            let mut context = json!({"diagnostics": diags});
            if let Some(only) = r.params.pointer("/context/only") {
                context["only"] = only.clone();
            }
            if let Some(t) = r.params.pointer("/context/triggerKind") {
                context["triggerKind"] = t.clone();
            }
            Some(
                json!({"textDocument": {"uri": g.uri}, "range": Range::new(gs, ge), "context": context}),
            )
        })();
        match forward {
            Some(params) => {
                self.pending.insert(r.id.clone(), Pending::CodeAction(uri.clone(), own));
                self.send_ra(Message::Request(Request {
                    id: r.id.clone(),
                    method: r.method.clone(),
                    params,
                }));
            }
            None => self.respond(r.id.clone(), Value::Array(own)),
        }
    }

    fn prop_actions(&self, uri: &Url, r: &Request) -> Vec<Value> {
        let Some(diags) = r.params.pointer("/context/diagnostics").and_then(Value::as_array) else {
            return Vec::new();
        };
        diags
            .iter()
            .filter_map(|d| {
                let fields = missing_fields(d.get("message")?.as_str()?);
                if fields.is_empty() {
                    return None;
                }
                let range: Range = serde_json::from_value(d.get("range")?.clone()).ok()?;
                let doc = self.docs.get(uri)?;
                // the diagnostic sits on (the start of) the tag name; the props go right after the name
                let from = doc.text.offset(range.start);
                let name_len = doc.text.s[from..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(doc.text.s.len() - from);
                let at = doc.text.position(from + name_len);
                let text: String = fields.iter().map(|f| format!(" {f}={{todo!()}}")).collect();
                Some(json!({
                    "title": format!("Add missing props: {}", fields.join(", ")),
                    "kind": "quickfix",
                    "isPreferred": true,
                    "diagnostics": [d],
                    "edit": {"changes": {uri.to_string(): [{"range": Range::new(at, at), "newText": text}]}},
                }))
            })
            .collect()
    }

    fn import_actions(&self, uri: &Url, range: Range) -> Vec<Value> {
        let imports = self.component_imports(uri);
        let Some(doc) = self.docs.get(uri) else {
            return Vec::new();
        };
        let (start, end) = (doc.text.offset(range.start), doc.text.offset(range.end));
        let mut names: Vec<&String> = imports.keys().collect();
        names.sort();
        names
            .into_iter()
            .filter(|n| occurrences(&doc.text.s, n).iter().any(|(a, b)| *a <= end && *b >= start))
            .map(|n| {
                let line = &imports[n];
                let (at, text) = import_edit(&doc.text, line);
                json!({
                    "title": format!("Import `{}`", line.trim_start_matches("use ").trim_end_matches(';')),
                    "kind": "quickfix",
                    "isPreferred": true,
                    "edit": {"changes": {uri.to_string(): [{"range": Range::new(at, at), "newText": text}]}},
                })
            })
            .collect()
    }

    pub(super) fn component_imports(&self, uri: &Url) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some(doc) = self.docs.get(uri) else {
            return out;
        };
        let Some(root) = &doc.root else { return out };
        let src = &doc.text.s;
        let comps = components(src);
        let head = &src[..comps.first().map_or(src.len(), |c| c.full.0)];
        for h in self.index.components() {
            if comps.iter().any(|c| c.name == h.name) || !occurrences(head, &h.name).is_empty() {
                continue;
            }
            if let Some(p) = self.index.path_of(&h.name, root) {
                out.insert(h.name.clone(), format!("use {p};"));
            }
        }
        let has_router = yawn_rsx::project::uses_routes(root)
            || std::fs::read_to_string(root.join("Cargo.toml"))
                .is_ok_and(|t| t.contains("yawn-router"));
        if has_router {
            let imported = crate::complete::imported_names(head);
            for (name, _) in crate::complete::ROUTER_COMPONENTS {
                if !imported.iter().any(|n| n == name)
                    && !comps.iter().any(|c| c.name == *name)
                    && self.index.find(name).is_none()
                {
                    out.insert((*name).to_string(), format!("use yawn_router::{name};"));
                }
            }
        }
        out
    }

    fn project_texts(&self, root: &Path) -> Vec<(PathBuf, String)> {
        let mut paths: Vec<PathBuf> =
            self.index.files().filter(|p| p.starts_with(root)).cloned().collect();
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                if p.is_dir() {
                    if !name.starts_with('.') && name != "target" {
                        walk(&p, out);
                    }
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        walk(&root.join("src"), &mut paths);
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .filter_map(|p| {
                let open = Url::from_file_path(&p)
                    .ok()
                    .and_then(|u| self.docs.get(&u))
                    .map(|d| d.text.s.clone());
                open.or_else(|| std::fs::read_to_string(&p).ok()).map(|t| (p, t))
            })
            .collect()
    }

    pub(super) fn component_request(&self, uri: &Url, r: &Request, offset: usize) -> bool {
        let Some(doc) = self.docs.get(uri) else {
            return false;
        };
        let src = &doc.text.s;
        let Some((a, b)) = word_at(src, offset) else {
            return false;
        };
        let name = src[a..b].to_string();
        if !name.starts_with(|c: char| c.is_uppercase())
            || !(self.index.find(&name).is_some() || components(src).iter().any(|c| c.name == name))
        {
            return false;
        }
        let root = doc.root.clone();
        let word_range = range_json(&doc.text, a, b);
        match r.method.as_str() {
            "textDocument/definition"
            | "textDocument/declaration"
            | "textDocument/typeDefinition" => {
                let result = self.component_definition(&name);
                self.respond(r.id.clone(), result);
            }
            "textDocument/references" => {
                let mut locs = Vec::new();
                if let Some(root) = &root {
                    for (path, text) in self.project_texts(root) {
                        let t = Text::new(text);
                        let Ok(u) = Url::from_file_path(&path) else {
                            continue;
                        };
                        for (x, y) in occurrences(&t.s, &name) {
                            locs.push(json!({"uri": u, "range": range_json(&t, x, y)}));
                        }
                    }
                }
                self.respond(r.id.clone(), Value::Array(locs));
            }
            "textDocument/prepareRename" => {
                self.respond(r.id.clone(), json!({"range": word_range, "placeholder": name}))
            }
            "textDocument/rename" => {
                let new = r.params.get("newName").and_then(Value::as_str).unwrap_or("").to_string();
                if !new.starts_with(|c: char| c.is_uppercase())
                    || !new.chars().all(|c| c.is_alphanumeric() || c == '_')
                {
                    self.respond_error(r.id.clone(), "a component name is a PascalCase identifier");
                    return true;
                }
                if new != name && self.index.find(&new).is_some() {
                    self.respond_error(
                        r.id.clone(),
                        &format!("a component named `{new}` already exists"),
                    );
                    return true;
                }
                let mut changes = Map::new();
                if let Some(root) = &root {
                    for (path, text) in self.project_texts(root) {
                        let t = Text::new(text);
                        let Ok(u) = Url::from_file_path(&path) else {
                            continue;
                        };
                        let edits: Vec<Value> = occurrences(&t.s, &name)
                            .into_iter()
                            .map(|(x, y)| json!({"range": range_json(&t, x, y), "newText": new}))
                            .collect();
                        if !edits.is_empty() {
                            changes.insert(u.to_string(), Value::Array(edits));
                        }
                    }
                }
                self.respond(r.id.clone(), json!({"changes": changes}));
            }
            _ => return false,
        }
        true
    }

    pub(super) fn workspace_symbols(&mut self, r: Request) {
        let query = r.params.get("query").and_then(Value::as_str).unwrap_or("").to_lowercase();
        let mut out = Vec::new();
        let files: Vec<PathBuf> = self.index.files().cloned().collect();
        for path in files {
            let open = Url::from_file_path(&path)
                .ok()
                .and_then(|u| self.docs.get(&u))
                .map(|d| d.text.s.clone());
            let Some(text) = open.or_else(|| std::fs::read_to_string(&path).ok()) else {
                continue;
            };
            let Ok(uri) = Url::from_file_path(&path) else {
                continue;
            };
            let t = Text::new(text);
            for c in components(&t.s) {
                if query.is_empty() || c.name.to_lowercase().contains(&query) {
                    out.push(json!({"name": c.name, "kind": 5, "location": {"uri": uri, "range": range_json(&t, c.name_at.0, c.name_at.1)}}));
                }
            }
            if out.len() >= 300 {
                break;
            }
        }
        self.respond(r.id, Value::Array(out));
    }

    pub(super) fn map_ops(&self, kind: Pending, res: Value) -> Value {
        match kind {
            Pending::CodeAction(uri, own) => {
                let mut out: Vec<Value> = Vec::new();
                for mut a in res.as_array().cloned().unwrap_or_default() {
                    // a bare `Command` (run a test, open docs...) means nothing for a generated module
                    if a.get("command").is_some_and(Value::is_string) {
                        continue;
                    }
                    if let Some(edit) = a.get_mut("edit")
                        && !self.map_edit(edit)
                    {
                        continue;
                    }
                    if let Some(o) = a.as_object_mut() {
                        o.remove("diagnostics");
                        o.remove("command");
                    }
                    out.push(a);
                }
                trace!(
                    "code actions for {uri}: {} from rust-analyzer, {} own",
                    out.len(),
                    own.len()
                );
                out.extend(own);
                Value::Array(out)
            }
            Pending::ActionResolve => {
                let mut a = res;
                if let Some(edit) = a.get_mut("edit")
                    && !self.map_edit(edit)
                {
                    a.as_object_mut().map(|o| o.remove("edit"));
                }
                a
            }
            Pending::InlayHints(uri) => {
                let Some(doc) = self.docs.get(&uri) else {
                    return json!([]);
                };
                let Some(g) = &doc.r#gen else { return json!([]) };
                let mut out = Vec::new();
                for mut h in res.as_array().cloned().unwrap_or_default() {
                    let Some(p) = h
                        .get("position")
                        .and_then(|p| serde_json::from_value::<Position>(p.clone()).ok())
                    else {
                        continue;
                    };
                    let Some((sp, true)) = gen_to_src(&g.map, &g.text, &doc.text, p) else {
                        continue;
                    };
                    h["position"] = json!(sp);
                    if let Some(edits) = h.get("textEdits").and_then(Value::as_array).cloned() {
                        let mapped: Vec<Value> = edits
                            .iter()
                            .filter_map(|e| {
                                let r = map_range(
                                    serde_json::from_value::<Range>(e.get("range")?.clone())
                                        .ok()?,
                                    g,
                                    &doc.text,
                                )?;
                                Some(json!({"range": r, "newText": e.get("newText")?}))
                            })
                            .collect();
                        h["textEdits"] = Value::Array(mapped);
                    }
                    if let Some(parts) = h.get_mut("label").and_then(Value::as_array_mut) {
                        for part in parts {
                            if part.get("location").is_some() {
                                let mut loc = part["location"].clone();
                                let keep = loc
                                    .get("uri")
                                    .and_then(Value::as_str)
                                    .and_then(|u| Url::parse(u).ok())
                                    .is_none_or(|u| {
                                        self.resolve_generated(&u).is_some()
                                            || !u.path().contains("/.yawn/")
                                    });
                                if keep {
                                    self.map_location(&mut loc);
                                    part["location"] = loc;
                                } else {
                                    part.as_object_mut().map(|o| o.remove("location"));
                                }
                            }
                        }
                    }
                    out.push(h);
                }
                Value::Array(out)
            }
            Pending::Rename => {
                let mut edit = res;
                if edit.is_null() || !self.map_edit(&mut edit) {
                    return Value::Null;
                }
                edit
            }
            Pending::PrepareRename(uri) => {
                let Some(doc) = self.docs.get(&uri) else {
                    return Value::Null;
                };
                let Some(g) = &doc.r#gen else {
                    return Value::Null;
                };
                let conv = |v: &Value| {
                    serde_json::from_value::<Range>(v.clone())
                        .ok()
                        .and_then(|r| map_range(r, g, &doc.text))
                };
                if res.get("start").is_some() {
                    return conv(&res).map_or(Value::Null, |r| json!(r));
                }
                if let Some(r) = res.get("range") {
                    return conv(r).map_or(Value::Null, |m| {
                        let mut out = res.clone();
                        out["range"] = json!(m);
                        out
                    });
                }
                res
            }
            Pending::Tokens(uri) => {
                let Some(doc) = self.docs.get(&uri) else {
                    return Value::Null;
                };
                let Some(g) = &doc.r#gen else {
                    return Value::Null;
                };
                let data: Vec<u64> = res
                    .get("data")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                let (mut line, mut ch) = (0u32, 0u32);
                let mut toks: Vec<(u32, u32, u64, u64, u64)> = Vec::new();
                for c in data.as_chunks::<5>().0 {
                    if c[0] > 0 {
                        line += c[0] as u32;
                        ch = c[1] as u32;
                    } else {
                        ch += c[1] as u32;
                    }
                    if let Some((sp, true)) =
                        gen_to_src(&g.map, &g.text, &doc.text, Position::new(line, ch))
                    {
                        toks.push((sp.line, sp.character, c[2], c[3], c[4]));
                    }
                }
                toks.sort();
                let (mut pl, mut pc) = (0u32, 0u32);
                let mut out: Vec<u64> = Vec::with_capacity(toks.len() * 5);
                for (l, c, len, ty, mods) in toks {
                    let dl = l - pl;
                    let ds = if dl == 0 { c - pc } else { c };
                    out.extend([dl as u64, ds as u64, len, ty, mods]);
                    (pl, pc) = (l, c);
                }
                json!({"data": out})
            }
            Pending::RsLocations => {
                let items: Vec<Value> = match res {
                    Value::Array(a) => a,
                    v @ Value::Object(_) => vec![v],
                    _ => return Value::Null,
                };
                let mut out = Vec::new();
                for mut l in items {
                    let key = if l.get("targetUri").is_some() { "targetUri" } else { "uri" };
                    let generated = l
                        .get(key)
                        .and_then(Value::as_str)
                        .and_then(|u| Url::parse(u).ok())
                        .is_some_and(|u| self.resolve_generated(&u).is_some());
                    if generated {
                        self.map_location(&mut l);
                        out.push(l);
                    }
                }
                Value::Array(out)
            }
            _ => res,
        }
    }

    pub(super) fn map_edit(&self, edit: &mut Value) -> bool {
        self.try_map_edit(edit).is_some()
    }

    fn try_map_edit(&self, edit: &mut Value) -> Option<()> {
        if let Some(changes) = edit.get("changes").and_then(Value::as_object).cloned() {
            let mut out: Map<String, Value> = Map::new();
            for (u, edits) in changes {
                let uri = Url::parse(&u).ok()?;
                let (nu, ne) = self.map_edits_for(&uri, edits.as_array()?)?;
                out.entry(nu.to_string()).or_insert_with(|| json!([])).as_array_mut()?.extend(ne);
            }
            edit["changes"] = Value::Object(out);
        }
        if let Some(dc) = edit.get("documentChanges").and_then(Value::as_array).cloned() {
            let mut out = Vec::new();
            for item in dc {
                if let Some(td) = item.get("textDocument") {
                    let uri = Url::parse(td.get("uri")?.as_str()?).ok()?;
                    let (nu, ne) = self.map_edits_for(&uri, item.get("edits")?.as_array()?)?;
                    let version = if nu == uri {
                        td.get("version").cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    };
                    out.push(json!({"textDocument": {"uri": nu, "version": version}, "edits": ne}));
                } else {
                    for key in ["uri", "oldUri", "newUri"] {
                        if let Some(u) =
                            item.get(key).and_then(Value::as_str).and_then(|u| Url::parse(u).ok())
                            && self.resolve_generated(&u).is_some()
                        {
                            return None;
                        }
                    }
                    out.push(item);
                }
            }
            edit["documentChanges"] = Value::Array(out);
        }
        Some(())
    }

    fn map_edits_for(&self, uri: &Url, edits: &[Value]) -> Option<(Url, Vec<Value>)> {
        let Some((rsx, map, gen_text, src_text)) = self.resolve_generated(uri) else {
            return Some((uri.clone(), edits.to_vec()));
        };
        let mut out = Vec::new();
        for e in edits {
            let r: Range = serde_json::from_value(e.get("range")?.clone()).ok()?;
            let text = e.get("newText")?.as_str()?;
            let s = gen_to_src(&map, &gen_text, &src_text, r.start);
            let en = gen_to_src(&map, &gen_text, &src_text, r.end);
            match (s, en) {
                (Some((s, true)), Some((en, true))) => {
                    out.push(json!({"range": Range::new(s, en), "newText": text}))
                }
                _ if r.start == r.end && text.contains("use ") => {
                    let (at, t) = import_edit(&src_text, text);
                    out.push(json!({"range": Range::new(at, at), "newText": t}));
                }
                _ => return None,
            }
        }
        Some((rsx, out))
    }
}

pub(crate) fn missing_fields(message: &str) -> Vec<String> {
    let first = message.lines().next().unwrap_or("");
    if first.starts_with("missing structure fields") {
        return message
            .lines()
            .skip(1)
            .filter_map(|l| l.trim().strip_prefix("- "))
            .map(|f| f.trim().to_string())
            .collect();
    }
    if let Some(rest) = first.strip_prefix("missing field") {
        let head = rest.split(" in initializer").next().unwrap_or(rest);
        return head.split('`').skip(1).step_by(2).map(str::to_string).collect();
    }
    Vec::new()
}

#[cfg(test)]
mod missing_tests {
    use super::missing_fields;

    #[test]
    fn parses_both_message_styles() {
        assert_eq!(
            missing_fields("missing structure fields:\n- routes\n- extra\n"),
            ["routes", "extra"]
        );
        assert_eq!(missing_fields("missing field `routes` in initializer of `Router`"), ["routes"]);
        assert_eq!(
            missing_fields("missing fields `a`, `b` and `c` in initializer of `X`"),
            ["a", "b", "c"]
        );
        assert!(missing_fields("mismatched types").is_empty());
    }
}
