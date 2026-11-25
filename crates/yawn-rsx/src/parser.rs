use crate::ast::*;
use crate::diag::{Diagnostic, LineIndex};
use crate::scan::{self, Kind};

type Result<T> = std::result::Result<T, Diagnostic>;

pub const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

pub fn parse(src: &str) -> Result<File> {
    let mut items = Vec::new();
    let mut i = 0;
    let mut rust_from = 0;
    let mut depth = 0usize;
    while i < src.len() {
        let (kind, end) = scan::token(src, i)?;
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident
                if depth == 0 && &src[i..end] == "component" && is_component_start(src, end) =>
            {
                // `pub component` is accepted: components are always public.
                let mut start = i;
                let before = src[rust_from..i].trim_end();
                if before.ends_with("pub")
                    && !before[..before.len() - 3]
                        .ends_with(|c: char| c.is_alphanumeric() || c == '_')
                {
                    start = rust_from + before.len() - 3;
                }
                if start > rust_from {
                    items.push(Item::Rust(rust_from..start));
                }
                let (comp, next) = parse_component(src, end)?;
                items.push(Item::Component(comp));
                rust_from = next;
                i = next;
                continue;
            }
            _ => {}
        }
        i = end;
    }
    if rust_from < src.len() {
        items.push(Item::Rust(rust_from..src.len()));
    }
    Ok(File { items })
}

/// A component's name and props, for tools
pub struct Header {
    pub name: String,
    /// (name, type)
    pub params: Vec<(String, String)>,
    /// The props that may be left out when the component is used
    pub defaults: Vec<String>,
    /// The `///` lines directly above the component, without the slashes; empty when there are none
    pub doc: String,
}

fn doc_above(src: &str, at: usize) -> String {
    let before = src[..at].trim_end_matches([' ', '\t']);
    let before = before.strip_suffix("pub").map_or(before, |b| b.trim_end_matches([' ', '\t']));
    let mut lines: Vec<&str> = Vec::new();
    for line in before.lines().rev() {
        match line.trim_start().strip_prefix("///") {
            Some(text) => lines.push(text.strip_prefix(' ').unwrap_or(text).trim_end()),
            None => break,
        }
    }
    lines.reverse();
    lines.join("\n")
}

pub fn headers(src: &str) -> Vec<Header> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    while i < src.len() {
        let Ok((kind, end)) = scan::token(src, i) else {
            break;
        };
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident
                if depth == 0 && &src[i..end] == "component" && is_component_start(src, end) =>
            {
                let Ok(n) = scan::skip_trivia(src, end, src.len()) else {
                    break;
                };
                let Ok((_, name_end)) = scan::token(src, n) else {
                    break;
                };
                let mut params = Vec::new();
                let mut defaults = Vec::new();
                let mut next = name_end;
                if let Ok(p) = scan::skip_trivia(src, name_end, src.len())
                    && src[p..].starts_with('(')
                    && let Ok(close) = scan::find_close(src, p, src.len())
                {
                    if let Ok(list) = parse_params(src, p + 1, close) {
                        defaults = list
                            .iter()
                            .filter(|q| q.default.is_some())
                            .map(|q| q.name.clone())
                            .collect();
                        params =
                            list.into_iter().map(|q| (q.name, src[q.ty].to_string())).collect();
                    }
                    next = close + 1;
                }
                out.push(Header {
                    name: src[n..name_end].to_string(),
                    params,
                    defaults,
                    doc: doc_above(src, i),
                });
                i = next;
                continue;
            }
            _ => {}
        }
        i = end;
    }
    out
}

fn is_component_start(src: &str, after_kw: usize) -> bool {
    let rest = &src[after_kw..];
    rest.starts_with(|c: char| c.is_whitespace())
        && rest.trim_start().starts_with(|c: char| c.is_uppercase())
}

fn parse_component(src: &str, after_kw: usize) -> Result<(Component, usize)> {
    let mut i = scan::skip_trivia(src, after_kw, src.len())?;
    let (_, name_end) = scan::token(src, i)?;
    let name = src[i..name_end].to_string();
    let name_span = i..name_end;
    i = scan::skip_trivia(src, name_end, src.len())?;

    let mut params = Vec::new();
    if src[i..].starts_with('(') {
        let close = scan::find_close(src, i, src.len())?;
        params = parse_params(src, i + 1, close)?;
        i = scan::skip_trivia(src, close + 1, src.len())?;
    }
    if !src[i..].starts_with('{') {
        return Err(Diagnostic::new(
            format!("expected `{{` to start the body of component `{name}`"),
            i..i + 1,
        )
        .label("component body expected here")
        .help(
            "a component looks like `component Name(prop: Type) { setup code; view { <div /> } }`",
        ));
    }
    let open = i;
    let close = scan::find_close(src, open, src.len())?;
    let body = open + 1..close;

    // `view { }` and an optional `style { }` end the body, in either order.
    let mut blocks: Vec<(&str, usize, usize, usize)> = Vec::new(); // (keyword, kw, open, close)
    let mut cursor = body.start;
    let mut depth = 0usize;
    while cursor < body.end {
        let (kind, end) = scan::token(&src[..body.end], cursor)?;
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident if depth == 0 && matches!(&src[cursor..end], "view" | "style") => {
                let after = scan::skip_trivia(src, end, body.end)?;
                if src[after..body.end].starts_with('{') {
                    let bclose = scan::find_close(src, after, body.end)?;
                    blocks.push((
                        if &src[cursor..end] == "view" { "view" } else { "style" },
                        cursor,
                        after,
                        bclose,
                    ));
                }
            }
            _ => {}
        }
        cursor = end;
    }
    // Keep only blocks that form the tail of the body.
    let mut tail: Vec<(&str, usize, usize, usize)> = Vec::new();
    let mut limit = body.end;
    for blk in blocks.iter().rev() {
        if scan::skip_trivia(src, blk.3 + 1, limit)? == limit {
            tail.push(*blk);
            limit = blk.1;
        }
    }
    tail.reverse();
    let Some(&(_, vkw, vopen, vclose)) = tail.iter().find(|b| b.0 == "view") else {
        return Err(Diagnostic::new(
            format!("component `{name}` has no `view {{ ... }}` block"),
            name_span.clone(),
        )
        .label("missing `view`")
        .help("end the component body with `view { <div>\"hello\"</div> }`"));
    };
    let style = tail.iter().find(|b| b.0 == "style").map(|b| b.2 + 1..b.3);
    let setup = body.start..tail[0].1;
    validate_block(src, setup.clone())?;
    let _ = vkw;
    let mut p = Template { src, pos: vopen + 1, end: vclose };
    let view = p.children(&Close::Eof)?;
    Ok((Component { name, name_span, params, setup, view, style }, close + 1))
}

fn parse_params(src: &str, from: usize, to: usize) -> Result<Vec<Param>> {
    let text = &src[from..to];
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut params = Vec::new();
    let mut start = from;
    loop {
        let comma = scan::find_top_level(src, start, to, |k, t, _| k == Kind::Punct && t == ",")?;
        let piece_end = comma.unwrap_or(to);
        let piece = &src[start..piece_end];
        if !piece.trim().is_empty() {
            let lead = piece.len() - piece.trim_start().len();
            let ps = start + lead;
            let colon = scan::find_top_level(src, ps, piece_end, |k, t, i| {
                k == Kind::Punct
                    && t == ":"
                    && src.as_bytes().get(i + 1) != Some(&b':')
                    && (i == 0 || src.as_bytes()[i - 1] != b':')
            })?;
            let Some(colon) = colon else {
                return Err(Diagnostic::new("parameter needs a type", ps..piece_end)
                    .help("write `name: Type`"));
            };
            // `name: Type = default`: the first `=` outside brackets that isn't part of `==`, `=>`, `<=`, `>=`
            let eq = scan::find_top_level(src, colon + 1, piece_end, |k, t, i| {
                let b = src.as_bytes();
                k == Kind::Punct
                    && t == "="
                    && b.get(i + 1) != Some(&b'=')
                    && b.get(i + 1) != Some(&b'>')
                    && (i == 0 || !matches!(b[i - 1], b'=' | b'<' | b'>' | b'!'))
            })?;
            let decl_end = start + src[start..eq.unwrap_or(piece_end)].trim_end().len();
            let typed = &src[ps..decl_end];
            if let Err(e) = syn::parse_str::<syn::FnArg>(typed) {
                return Err(map_wrapped(&e, typed, 0, ps, typed).help(
                    "parameters are `name: Type` or `name: Type = default`, separated by commas",
                ));
            }
            let default = match eq {
                Some(e) => {
                    let value_start = e
                        + 1
                        + (src[e + 1..piece_end].len() - src[e + 1..piece_end].trim_start().len());
                    let value_end = start + piece.trim_end().len();
                    if value_start >= value_end {
                        return Err(Diagnostic::new("a default needs a value", e..piece_end)
                            .help("write `name: Type = value`"));
                    }
                    Some(value_start..value_end)
                }
                None => None,
            };
            let before = &src[ps..colon];
            let name = before.trim().trim_start_matches("mut ").trim().to_string();
            let name_at = ps + before.rfind(name.as_str()).unwrap_or(0);
            let ty_start = colon
                + 1
                + (src[colon + 1..decl_end].len() - src[colon + 1..decl_end].trim_start().len());
            params.push(Param {
                name,
                ty: ty_start..decl_end,
                span: ps..start + piece.trim_end().len(),
                decl: name_at..decl_end,
                default,
            });
        }
        match comma {
            Some(c) => start = c + 1,
            None => break,
        }
    }
    Ok(params)
}

fn validate_block(src: &str, range: Span) -> Result<()> {
    let text = &src[range.clone()];
    if text.trim().is_empty() {
        return Ok(());
    }
    let wrapped = format!("{{{text}}}");
    match syn::parse_str::<syn::Block>(&wrapped) {
        Ok(_) => Ok(()),
        Err(e) => Err(map_wrapped(&e, &wrapped, 1, range.start, text)),
    }
}

fn map_wrapped(
    e: &syn::Error,
    wrapped: &str,
    prefix_len: usize,
    base: usize,
    text: &str,
) -> Diagnostic {
    let idx = LineIndex::new(wrapped);
    let s = idx.offset(wrapped, e.span().start().line.saturating_sub(1), e.span().start().column);
    let en = idx.offset(wrapped, e.span().end().line.saturating_sub(1), e.span().end().column);
    let s = s.saturating_sub(prefix_len).min(text.len());
    let en = en.saturating_sub(prefix_len).min(text.len());
    // "unexpected end of input": the code is unfinished, so point at its last token
    let trimmed = text.trim_end();
    if s >= trimmed.len()
        && let Some((i, c)) = trimmed.char_indices().last()
    {
        return Diagnostic::new(e.to_string(), base + i..base + i + c.len_utf8());
    }
    let en = if en > s { en } else { (s + 1).min(text.len()).max(s) };
    Diagnostic::new(e.to_string(), base + s..base + en)
}

/// Validates `{expr}`-like Rust as an expression, or as block contents if it isn't one
pub fn validate_expr(src: &str, range: Span) -> Result<bool> {
    let text = &src[range.clone()];
    if text.trim().is_empty() {
        return Err(Diagnostic::new(
            "empty expression",
            range.start.saturating_sub(1)..range.end + 1,
        )
        .label("expected a Rust expression between the braces"));
    }
    match syn::parse_str::<syn::Expr>(text) {
        Ok(_) => Ok(true),
        Err(e) => {
            let wrapped = format!("{{{text}}}");
            match syn::parse_str::<syn::Block>(&wrapped) {
                Ok(_) => Ok(false),
                Err(_) => Err(map_wrapped(&e, text, 0, range.start, text)),
            }
        }
    }
}

enum Close {
    Eof,
    Brace(Span),
    Tag(String, Span),
}

struct Template<'a> {
    src: &'a str,
    pos: usize,
    end: usize,
}

impl Template<'_> {
    fn trivia(&mut self) -> Result<()> {
        self.pos = scan::skip_trivia(self.src, self.pos, self.end)?;
        Ok(())
    }

    fn rest(&self) -> &str {
        &self.src[self.pos..self.end]
    }

    fn children(&mut self, close: &Close) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        loop {
            self.trivia()?;
            if self.pos >= self.end {
                return match close {
                    Close::Eof => Ok(nodes),
                    Close::Brace(open) => Err(Diagnostic::new("unclosed block", open.clone())
                        .label("this `{` is never closed")),
                    Close::Tag(name, open) => {
                        Err(Diagnostic::new(format!("unclosed tag `<{name}>`"), open.clone())
                            .label(format!("expected `</{name}>` before the end of the view")))
                    }
                };
            }
            let c = self.src.as_bytes()[self.pos];
            match c {
                b'<' if self.src[self.pos + 1..self.end].starts_with('/') => {
                    let start = self.pos;
                    let name_start = self.pos + 2;
                    let (name, name_end) = self.tag_name(name_start)?;
                    let mut p = scan::skip_trivia(self.src, name_end, self.end)?;
                    if !self.src[p..self.end].starts_with('>') {
                        return Err(Diagnostic::new(
                            "expected `>` to finish the closing tag",
                            p..p + 1,
                        ));
                    }
                    p += 1;
                    return match close {
                        Close::Tag(expected, _) if *expected == name => {
                            self.pos = p;
                            Ok(nodes)
                        }
                        Close::Tag(expected, open) => {
                            Err(Diagnostic::new("mismatched closing tag", start..p)
                                .label(format!("expected `</{expected}>`, found `</{name}>`"))
                                .note(open.clone(), format!("`<{expected}>` opened here")))
                        }
                        _ => Err(Diagnostic::new(
                            format!("unexpected closing tag `</{name}>`"),
                            start..p,
                        )
                        .label("there is no open tag to close here")),
                    };
                }
                b'<' => nodes.push(self.element()?),
                b'"' => nodes.push(Node::Text(self.text()?)),
                b'{' => {
                    let open = self.pos;
                    let close_at = scan::find_close(self.src, open, self.end)?;
                    let expr = open + 1..close_at;
                    validate_expr(self.src, expr.clone())?;
                    self.pos = close_at + 1;
                    nodes.push(Node::Expr(expr));
                }
                b'}' => {
                    if let Close::Brace(_) = close {
                        self.pos += 1;
                        return Ok(nodes);
                    }
                    return Err(Diagnostic::new("unexpected `}`", self.pos..self.pos + 1));
                }
                _ => {
                    let (kind, end) = scan::token(self.src, self.pos)?;
                    let word = &self.src[self.pos..end];
                    if kind == Kind::Ident && word == "if" {
                        nodes.push(Node::If(self.if_node()?));
                    } else if kind == Kind::Ident && word == "for" {
                        nodes.push(Node::For(self.for_node()?));
                    } else if kind == Kind::Ident
                        && matches!(word, "match" | "while" | "loop" | "let")
                    {
                        return Err(Diagnostic::new(format!("`{word}` is not supported in a view"), self.pos..end)
                            .help("use `if`, `for ... key ...`, or compute the value in the setup code and write `{value}`"));
                    } else {
                        let text_end = self
                            .rest()
                            .find(['<', '{', '}', '"', '\n'])
                            .map(|n| self.pos + n)
                            .unwrap_or(self.end);
                        return Err(Diagnostic::new(
                            "text in a view must be a quoted string",
                            self.pos..text_end.max(end),
                        )
                        .label("not an element, string, `{expression}`, `if` or `for`")
                        .help("write it as \"text\""));
                    }
                }
            }
        }
    }

    fn tag_name(&self, at: usize) -> Result<(String, usize)> {
        let mut j = at;
        for (o, ch) in self.src[at..self.end].char_indices() {
            if ch.is_alphanumeric() || ch == '_' || ch == '-' {
                j = at + o + ch.len_utf8();
            } else {
                break;
            }
        }
        if j == at {
            return Err(Diagnostic::new(
                "expected a tag name after `<`",
                at.saturating_sub(1)..at + 1,
            )
            .label("tag name expected"));
        }
        Ok((self.src[at..j].to_string(), j))
    }

    fn element(&mut self) -> Result<Node> {
        let start = self.pos;
        let (name, name_end) = self.tag_name(start + 1)?;
        let name_span = start + 1..name_end;
        let is_component = name.starts_with(|c: char| c.is_uppercase());
        self.pos = name_end;
        let mut attrs = Vec::new();
        let self_closing;
        loop {
            self.trivia()?;
            let rest = self.rest();
            if rest.starts_with("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if rest.starts_with('>') {
                self.pos += 1;
                self_closing = false;
                break;
            }
            if rest.is_empty() {
                return Err(Diagnostic::new(format!("unclosed tag `<{name}`"), start..name_end)
                    .label("the tag is never finished with `>` or `/>`"));
            }
            attrs.push(self.attr(&name)?);
        }
        let open_tag = start..self.pos;
        let void = !is_component && VOID_TAGS.contains(&name.as_str());
        let mut children = Vec::new();
        if !self_closing && !void {
            children = self.children(&Close::Tag(name.clone(), start..name_end))?;
        } else if void && !self_closing {
            // `<input ...>`: a closing tag would be an error, caught by `children` of the parent.
        }
        if name == "slot" && !is_component {
            let mut slot_name = None;
            for a in &attrs {
                if a.name != "name" {
                    return Err(Diagnostic::new(
                        format!("`<slot>` has no attribute `{}`", a.name),
                        a.name_span.clone(),
                    )
                    .help("the only attribute is `name=\"...\"`"));
                }
                match &a.value {
                    AttrValue::Text(t) if t.is_static() => {
                        slot_name = Some(self.src[t.span.start + 1..t.span.end - 1].to_string())
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            "the name of a slot is a plain string",
                            a.name_span.clone(),
                        )
                        .help("`<slot name=\"title\" />`"));
                    }
                }
                if !slot_name.as_deref().is_some_and(is_ident) {
                    return Err(Diagnostic::new(
                        "the name of a slot must be a Rust identifier (it is a prop of the component)",
                        a.name_span.clone(),
                    ));
                }
            }
            return Ok(Node::Slot(start..self.pos, slot_name));
        }
        Ok(Node::Element(Element {
            name,
            name_span,
            is_component,
            attrs,
            children,
            span: start..open_tag.end.max(self.pos),
        }))
    }

    fn attr(&mut self, tag: &str) -> Result<Attr> {
        let start = self.pos;
        let mut j = start;
        for (o, ch) in self.src[start..self.end].char_indices() {
            if ch.is_alphanumeric() || matches!(ch, '_' | '-' | ':') {
                j = start + o + ch.len_utf8();
            } else {
                break;
            }
        }
        if j == start {
            let ch = self.rest().chars().next().unwrap();
            return Err(Diagnostic::new(
                format!("unexpected `{ch}` in tag `<{tag}>`"),
                start..start + ch.len_utf8(),
            )
            .label("expected an attribute name, `>` or `/>`"));
        }
        let name = self.src[start..j].to_string();
        let name_span = start..j;
        self.pos = j;
        self.trivia()?;
        if !self.rest().starts_with('=') {
            return Ok(Attr { name, name_span, value: AttrValue::Flag });
        }
        self.pos += 1;
        self.trivia()?;
        let value = match self.rest().as_bytes().first() {
            Some(b'"') => AttrValue::Text(self.text()?),
            Some(b'{') => {
                let open = self.pos;
                let close = scan::find_close(self.src, open, self.end)?;
                let expr = open + 1..close;
                validate_expr(self.src, expr.clone())?;
                self.pos = close + 1;
                AttrValue::Expr(expr)
            }
            _ => {
                return Err(Diagnostic::new(
                    format!("value of `{name}` must be a \"string\" or a {{expression}}"),
                    self.pos..(self.pos + 1).min(self.end),
                )
                .label("quote it, or wrap it in braces"));
            }
        };
        Ok(Attr { name, name_span, value })
    }

    fn text(&mut self) -> Result<Text> {
        let start = self.pos;
        let (_, end) = scan::token(&self.src[..self.end], start)?;
        let inner = start + 1..end - 1;
        let mut parts = Vec::new();
        let mut lit_start = inner.start;
        let mut i = inner.start;
        let b = self.src.as_bytes();
        while i < inner.end {
            match b[i] {
                b'\\' => i += 2,
                b'{' if b.get(i + 1) == Some(&b'{') => i += 2,
                b'}' if b.get(i + 1) == Some(&b'}') => i += 2,
                b'}' => {
                    return Err(Diagnostic::new("unmatched `}` in string", i..i + 1)
                        .help("write `}}` for a literal brace"));
                }
                b'{' => {
                    if i > lit_start {
                        parts.push(Part::Lit(lit_start..i));
                    }
                    let close = scan::find_close(self.src, i, inner.end)
                        .map_err(|d| d.help("write `{{` for a literal brace"))?;
                    let (expr, spec) = split_spec(self.src, i + 1, close)?;
                    validate_expr(self.src, expr.clone())?;
                    parts.push(Part::Interp { expr, spec });
                    i = close + 1;
                    lit_start = i;
                }
                _ => i += 1,
            }
        }
        if inner.end > lit_start {
            parts.push(Part::Lit(lit_start..inner.end));
        }
        self.pos = end;
        Ok(Text { span: start..end, parts })
    }

    fn header(&mut self, kw_end: usize) -> Result<(Span, usize)> {
        // The block's `{` is the first one outside any parentheses or brackets.
        let mut i = kw_end;
        let mut depth = 0usize;
        while i < self.end {
            let (kind, end) = scan::token(&self.src[..self.end], i)?;
            match kind {
                Kind::Open(b'{') if depth == 0 => return Ok((kw_end..i, i)),
                Kind::Open(_) => depth += 1,
                Kind::Close(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            i = end;
        }
        Err(Diagnostic::new("expected `{` to open the block", kw_end..self.end.min(kw_end + 1)))
    }

    fn if_node(&mut self) -> Result<If> {
        let start = self.pos;
        let (cond, open) = self.header(start + 2)?;
        let cond_text = &self.src[cond.clone()];
        let trimmed = cond_text.trim();
        if trimmed.is_empty() {
            return Err(Diagnostic::new("`if` needs a condition", start..start + 2)
                .label("condition expected"));
        }
        let lead = cond_text.len() - cond_text.trim_start().len();
        let cond = cond.start + lead..cond.start + lead + trimmed.len();
        let mut binding = None;
        if let Some(rest) = trimmed.strip_prefix("let ") {
            let pat_start = cond.start + "let ".len() + (rest.len() - rest.trim_start().len());
            let Some(eq) = let_equals(&self.src[pat_start..cond.end]) else {
                return Err(Diagnostic::new("`if let` needs `= value`", cond.clone())
                    .label("expected `PATTERN = value`"));
            };
            let pat = pat_start..pat_start + self.src[pat_start..pat_start + eq].trim_end().len();
            let value_start = pat_start + eq + 1;
            let value_start = value_start
                + (self.src[value_start..cond.end].len()
                    - self.src[value_start..cond.end].trim_start().len());
            binding = Some((pat, value_start..cond.end));
        }
        let wrapped = format!("if {trimmed} {{}}");
        if let Err(e) = syn::parse_str::<syn::Expr>(&wrapped) {
            return Err(map_wrapped(&e, &wrapped, 3, cond.start, trimmed));
        }
        self.pos = open + 1;
        let then = self.children(&Close::Brace(open..open + 1))?;
        let mut otherwise = None;
        let save = self.pos;
        self.trivia()?;
        let (kind, end) = if self.pos < self.end {
            scan::token(&self.src[..self.end], self.pos)?
        } else {
            (Kind::Space, self.pos)
        };
        if kind == Kind::Ident && &self.src[self.pos..end] == "else" {
            self.pos = end;
            self.trivia()?;
            if self.rest().starts_with('{') {
                let open = self.pos;
                self.pos += 1;
                otherwise = Some(self.children(&Close::Brace(open..open + 1))?);
            } else if self.rest().starts_with("if") {
                otherwise = Some(vec![Node::If(self.if_node()?)]);
            } else {
                return Err(Diagnostic::new(
                    "expected `{` or `if` after `else`",
                    self.pos..(self.pos + 1).min(self.end),
                ));
            }
        } else {
            self.pos = save;
        }
        Ok(If { span: start..self.pos, cond, binding, then, otherwise })
    }

    fn for_node(&mut self) -> Result<For> {
        let start = self.pos;
        let (header, open) = self.header(start + 3)?;
        let in_at = scan::find_top_level(self.src, header.start, header.end, |k, t, _| {
            k == Kind::Ident && t == "in"
        })?;
        let Some(in_at) = in_at else {
            return Err(Diagnostic::new("`for` needs `in`", start..start + 3)
                .help("`for item in items.get() key item.id { ... }`"));
        };
        let key_at = scan::find_top_level(self.src, in_at + 2, header.end, |k, t, i| {
            k == Kind::Ident && t == "key" && !self.src[..i].trim_end().ends_with(['.', ':'])
        })?;
        let Some(key_at) = key_at else {
            return Err(Diagnostic::new("`for` in a view needs a `key`", start..header.end)
                .label("add `key <expression>` before the `{`")
                .help("keys identify rows so they are reused and moved, not rebuilt: `for todo in todos.get() key todo.id {`"));
        };
        let trim = |a: usize, b: usize| {
            let t = &self.src[a..b];
            let lead = t.len() - t.trim_start().len();
            a + lead..a + lead + t.trim().len()
        };
        let pat = trim(header.start, in_at);
        let iter = trim(in_at + 2, key_at);
        let key = trim(key_at + 3, header.end);
        if pat.is_empty() {
            return Err(Diagnostic::new("`for` needs a pattern", start..in_at + 2));
        }
        if iter.is_empty() {
            return Err(Diagnostic::new("`for` needs something to loop over", in_at..in_at + 2));
        }
        if key.is_empty() {
            return Err(Diagnostic::new("`key` needs an expression", key_at..key_at + 3)
                .help("`key item.id`"));
        }
        let (p, it) = (&self.src[pat.clone()], &self.src[iter.clone()]);
        let wrapped = format!("for {p} in {it} {{}}");
        if let Err(e) = syn::parse_str::<syn::Expr>(&wrapped) {
            // Point into the pattern or the iterator expression.
            let idx = LineIndex::new(&wrapped);
            let at = idx.offset(&wrapped, e.span().start().line - 1, e.span().start().column);
            let (base, rel) = if at < 4 + p.len() {
                (pat.start, at.saturating_sub(4))
            } else {
                (iter.start, at - (4 + p.len() + 4))
            };
            return Err(Diagnostic::new(e.to_string(), base + rel..base + rel + 1));
        }
        validate_expr(self.src, key.clone())?;
        self.pos = open + 1;
        let body = self.children(&Close::Brace(open..open + 1))?;
        Ok(For { span: start..self.pos, pat, iter, key, body })
    }
}

fn split_spec(src: &str, from: usize, to: usize) -> Result<(Span, Option<Span>)> {
    let colon = scan::find_top_level(src, from, to, |k, t, i| {
        k == Kind::Punct
            && t == ":"
            && src.as_bytes().get(i + 1) != Some(&b':')
            && src.as_bytes()[i - 1] != b':'
    })?;
    if let Some(c) = colon {
        let spec = &src[c + 1..to];
        if spec.chars().all(|ch| ch.is_ascii_alphanumeric() || "<>^=+-#.?$_ ".contains(ch))
            && !spec.contains(' ')
        {
            return Ok((from..c, Some(c + 1..to)));
        }
    }
    Ok((from..to, None))
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn let_equals(text: &str) -> Option<usize> {
    let mut i = 0;
    let mut depth = 0usize;
    while i < text.len() {
        let (kind, end) = scan::token(text, i).ok()?;
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Punct if depth == 0 && &text[i..end] == "=" => {
                let before = text[..i].chars().next_back();
                let after = text[end..].chars().next();
                if !matches!(before, Some('.' | '!' | '<' | '>' | '='))
                    && !matches!(after, Some('=' | '>'))
                {
                    return Some(i);
                }
            }
            _ => {}
        }
        i = end;
    }
    None
}
