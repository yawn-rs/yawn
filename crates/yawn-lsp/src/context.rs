use yawn_rsx::scan::{self, Kind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ctx {
    /// Rust code: setup, `{expressions}`, conditions, plain Rust items
    Rust,
    /// After `<`: a tag name
    Tag { start: usize },
    /// Inside an open tag where an attribute name goes
    Attr { tag: String, start: usize },
    /// Inside the string of a `class="..."` attribute; `start` is where the class being typed
    /// begins (after the last space)
    Class { start: usize },
    /// After `on:`
    Event { start: usize },
    /// Between nodes in a view (could start `if`, `for`, a string, a tag)
    Children { start: usize },
    /// Inside a `style { }` block
    Css { start: usize, prop: Option<String>, in_block: bool },
    /// Strings and everything else: nothing to offer
    None,
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | ':')
}

pub fn context(src: &str, offset: usize) -> Ctx {
    let offset = offset.min(src.len());
    let Some((kind, open)) = enclosing_block(src, offset) else {
        return Ctx::Rust;
    };
    match kind {
        Block::Setup => Ctx::Rust,
        Block::Style => css_context(src, open + 1, offset),
        Block::View => view_context(src, open + 1, offset),
    }
}

enum Block {
    Setup,
    View,
    Style,
}

fn enclosing_block(src: &str, offset: usize) -> Option<(Block, usize)> {
    let mut i = 0;
    let mut depth = 0usize;
    while i < src.len() {
        let Ok((kind, end)) = scan::token(src, i) else {
            return None;
        };
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident
                if depth == 0
                    && &src[i..end] == "component"
                    && src[end..].starts_with(|c: char| c.is_whitespace()) =>
            {
                // component Name(params) {
                let mut j = end;
                let mut paren = 0;
                while j < src.len() {
                    let Ok((k, e)) = scan::token(src, j) else {
                        return None;
                    };
                    match k {
                        Kind::Open(b'(') => paren += 1,
                        Kind::Close(b')') => paren -= 1,
                        Kind::Open(b'{') if paren == 0 => break,
                        _ => {}
                    }
                    j = e;
                }
                if j >= src.len() || offset <= j {
                    return None;
                }
                let body_end = scan::find_close(src, j, src.len()).unwrap_or(src.len());
                if offset > body_end {
                    i = body_end + 1;
                    depth = 0;
                    continue;
                }
                return Some(inside_component(src, j + 1, body_end, offset));
            }
            _ => {}
        }
        i = end;
    }
    None
}

fn inside_component(src: &str, from: usize, to: usize, offset: usize) -> (Block, usize) {
    let mut found = (Block::Setup, from);
    let mut depth = 0usize;
    let mut i = from;
    while i < to {
        let Ok((kind, end)) = scan::token(&src[..to], i) else {
            break;
        };
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident if depth == 0 && matches!(&src[i..end], "view" | "style") => {
                if let Ok(after) = scan::skip_trivia(src, end, to)
                    && src[after..to].starts_with('{')
                    && after < offset
                {
                    let close = scan::find_close(src, after, to).unwrap_or(to);
                    if offset <= close {
                        let b = if &src[i..end] == "view" { Block::View } else { Block::Style };
                        return (b, after);
                    }
                    found = (Block::Setup, from);
                    i = close + 1;
                    continue;
                }
            }
            _ => {}
        }
        i = end;
    }
    found
}

fn css_context(src: &str, from: usize, offset: usize) -> Ctx {
    let text = &src[from..offset];
    let mut depth = 0i32;
    let mut stmt_start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '{' => {
                depth += 1;
                stmt_start = i + 1;
            }
            '}' => {
                depth -= 1;
                stmt_start = i + 1;
            }
            ';' => stmt_start = i + 1,
            _ => {}
        }
    }
    let stmt = &text[stmt_start..];
    let word_start = |s: &str| {
        s.rfind(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
            .map(|i| i + 1)
            .unwrap_or(0)
    };
    if depth > 0 {
        if let Some(colon) = stmt.find(':') {
            let prop = stmt[..colon].trim().to_string();
            let tail = &stmt[colon + 1..];
            return Ctx::Css {
                start: from + stmt_start + colon + 1 + word_start(tail),
                prop: Some(prop),
                in_block: true,
            };
        }
        return Ctx::Css {
            start: from + stmt_start + word_start(stmt),
            prop: None,
            in_block: true,
        };
    }
    Ctx::Css { start: from + stmt_start + word_start(stmt), prop: None, in_block: false }
}

fn view_context(src: &str, from: usize, offset: usize) -> Ctx {
    let mut i = from;
    while i < offset {
        let Ok((kind, end)) =
            scan::token(&src[..offset.max(i)], i).or_else(|_| scan::token(src, i))
        else {
            return Ctx::None;
        };
        match kind {
            Kind::Punct if &src[i..end] == "<" => {
                if src[end..].starts_with('/') {
                    // closing tag: skip to `>`
                    match src[end..].find('>') {
                        Some(n) if end + n < offset => i = end + n + 1,
                        _ => return Ctx::None,
                    }
                    continue;
                }
                let name_end = src[end..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                    .map(|n| end + n)
                    .unwrap_or(src.len());
                if offset <= name_end {
                    return Ctx::Tag { start: end };
                }
                match tag_body(src, &src[end..name_end], name_end, offset) {
                    Step::Done(c) => return c,
                    Step::Continue(next) => i = next,
                }
                continue;
            }
            Kind::Str => {
                if end > offset || !src[i + 1..end].ends_with('"') {
                    return if in_interpolation(src, i, offset) { Ctx::Rust } else { Ctx::None };
                }
            }
            Kind::Open(b'{') => {
                let close = scan::find_close(src, i, src.len()).unwrap_or(src.len());
                if offset <= close {
                    return Ctx::Rust;
                }
                i = close + 1;
                continue;
            }
            // `else {` opens a block of nodes like `if` does; left alone its `{` would be read as an expression
            Kind::Ident if &src[i..end] == "else" => {
                if let Ok(after) = scan::skip_trivia(src, end, src.len())
                    && src[after..].starts_with('{')
                {
                    i = after + 1;
                    continue;
                }
            }
            Kind::Ident if matches!(&src[i..end], "if" | "for") => {
                // header up to the block's `{`
                let mut j = end;
                let mut depth = 0;
                loop {
                    if j >= src.len() {
                        return Ctx::Rust;
                    }
                    let Ok((k, e)) = scan::token(src, j) else {
                        return Ctx::Rust;
                    };
                    match k {
                        Kind::Open(b'{') if depth == 0 => break,
                        Kind::Open(_) => depth += 1,
                        Kind::Close(_) => depth -= 1,
                        _ => {}
                    }
                    j = e;
                    if offset <= j {
                        return Ctx::Rust;
                    }
                }
                if offset <= j {
                    return Ctx::Rust;
                }
                i = j + 1;
                continue;
            }
            _ => {}
        }
        i = end;
    }
    // Between nodes: the prefix being typed is the identifier right before the cursor.
    let start =
        src[..offset].rfind(|c: char| !c.is_alphanumeric()).map(|n| n + 1).unwrap_or(0).max(from);
    Ctx::Children { start }
}

fn in_interpolation(src: &str, start: usize, offset: usize) -> bool {
    let b = src.as_bytes();
    let mut open = false;
    let mut i = start + 1;
    while i < offset.min(b.len()) {
        match b[i] {
            b'\\' => i += 1,
            b'{' if !open && b.get(i + 1) == Some(&b'{') => i += 1,
            b'{' if !open => open = true,
            b'}' if open => open = false,
            _ => {}
        }
        i += 1;
    }
    open
}

enum Step {
    Done(Ctx),
    Continue(usize),
}

fn tag_body(src: &str, tag: &str, mut i: usize, offset: usize) -> Step {
    loop {
        let Ok(j) = scan::skip_trivia(src, i, src.len()) else {
            return Step::Done(Ctx::None);
        };
        if j >= offset {
            return Step::Done(Ctx::Attr { tag: tag.to_string(), start: offset });
        }
        let rest = &src[j..];
        if rest.starts_with("/>") {
            return Step::Continue(j + 2);
        }
        if rest.starts_with('>') {
            return Step::Continue(j + 1);
        }
        let name_end = rest.find(|c: char| !is_name_char(c)).map(|n| j + n).unwrap_or(src.len());
        if name_end == j {
            // stray character
            i = j + rest.chars().next().map_or(1, char::len_utf8);
            continue;
        }
        if offset <= name_end {
            let name = &src[j..offset];
            return Step::Done(match name.strip_prefix("on:") {
                Some(_) => Ctx::Event { start: j + 3 },
                None => Ctx::Attr { tag: tag.to_string(), start: j },
            });
        }
        let after = scan::skip_trivia(src, name_end, src.len()).unwrap_or(name_end);
        if !src[after..].starts_with('=') {
            i = name_end;
            continue;
        }
        let v = scan::skip_trivia(src, after + 1, src.len()).unwrap_or(after + 1);
        if v >= offset {
            return Step::Done(Ctx::None);
        }
        match src[v..].chars().next() {
            Some('"') => match scan::token(src, v) {
                Ok((_, e)) if e <= offset && src[v + 1..e].ends_with('"') => i = e,
                _ => {
                    if in_interpolation(src, v, offset) {
                        return Step::Done(Ctx::Rust);
                    }
                    if &src[j..name_end] == "class" {
                        let start = src[v + 1..offset]
                            .rfind(char::is_whitespace)
                            .map_or(v + 1, |n| v + 1 + n + 1);
                        return Step::Done(Ctx::Class { start });
                    }
                    return Step::Done(Ctx::None);
                }
            },
            Some('{') => {
                let close = scan::find_close(src, v, src.len()).unwrap_or(src.len());
                if offset <= close {
                    return Step::Done(Ctx::Rust);
                }
                i = close + 1;
            }
            _ => i = v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Ctx {
        let off = text.find('|').unwrap();
        context(&text.replace('|', ""), off)
    }

    #[test]
    fn rust_in_setup_expressions_and_top_level() {
        assert_eq!(at("use std::|\ncomponent A() {}"), Ctx::Rust);
        assert_eq!(at("component A() { let x = foo.| ; view { <b/> } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { <p>{foo.|}</p> } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { <p class={a.|} /> } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { if a.| { <b/> } } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { for x in a.| key x { <b/> } } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { <p>\"v {a.|}\"</p> } }"), Ctx::Rust, "interpolation");
    }

    #[test]
    fn markup_positions() {
        assert!(matches!(at("component A() { view { <di| } }"), Ctx::Tag { .. }));
        assert!(matches!(at("component A() { view { <div cl| } }"), Ctx::Attr { .. }));
        assert!(matches!(at("component A() { view { <div class=\"a\" | } }"), Ctx::Attr { .. }));
        assert!(matches!(at("component A() { view { <button on:cl| } }"), Ctx::Event { .. }));
        assert!(matches!(at("component A() { view { <div></div> | } }"), Ctx::Children { .. }));
        assert_eq!(at("component A() { view { <div id=\"x|\" /> } }"), Ctx::None);
        assert_eq!(
            at("component A() { view { <div class=\"flex p-|\" /> } }"),
            Ctx::Class { start: 40 }
        );
        assert!(matches!(at("component A() { view { <div class=\"|\" /> } }"), Ctx::Class { .. }));
        assert_eq!(at("component A() { view { <div class=\"a {x.|}\" /> } }"), Ctx::Rust);
        assert_eq!(at("component A() { view { <div class=\"a\" id=\"x|\" /> } }"), Ctx::None);
        match at("component A() { view { <Card ti| /> } }") {
            Ctx::Attr { tag, .. } => assert_eq!(tag, "Card"),
            c => panic!("{c:?}"),
        }
    }

    #[test]
    fn css_positions() {
        assert!(matches!(
            at("component A() { view { <b/> } style { .x { col| } } }"),
            Ctx::Css { prop: None, in_block: true, .. }
        ));
        match at("component A() { view { <b/> } style { .x { display: fl| } } }") {
            Ctx::Css { prop, .. } => assert_eq!(prop.as_deref(), Some("display")),
            c => panic!("{c:?}"),
        }
        assert!(matches!(
            at("component A() { style { .x| } view { <b/> } }"),
            Ctx::Css { in_block: false, .. }
        ));
    }

    #[test]
    fn a_tag_right_after_a_closing_tag() {
        let hero = "component HeroTitle() {\n    let plain = true;\n    view {\n        if plain {\n            {t(\"a\")}\n        } else {\n            {before.get()}\n            <span class=\"x\" data-text={word.get()} style=\"--pop-main: #7c68e6;\">{word.get()}</span><Test|/>\n            {after.get()}\n        }\n    }\n}\n";
        assert!(matches!(at(hero), Ctx::Tag { .. }), "{:?}", at(hero));
        assert!(matches!(at("component A() { view { <b>\"x\"</b><Te| } }"), Ctx::Tag { .. }));
        assert!(matches!(at("component A() { view { <b>{x}</b><Te| } }"), Ctx::Tag { .. }));
    }
}
