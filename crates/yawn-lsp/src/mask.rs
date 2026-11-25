use yawn_rsx::scan::{self, Kind};

const MASK: u8 = 0;
const KEEP: u8 = 1;
const QUOTE: u8 = 2;

#[derive(Default)]
struct Blocks {
    views: Vec<(usize, usize)>,
    styles: Vec<(usize, usize)>,
}

fn blocks(src: &str) -> Blocks {
    let mut out = Blocks::default();
    let mut i = 0;
    let mut depth = 0usize;
    while i < src.len() {
        let Ok((kind, end)) = scan::token(src, i) else {
            break;
        };
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident if depth == 0 && &src[i..end] == "component" => {
                let (mut j, mut paren, mut open) = (end, 0i32, None);
                while j < src.len() {
                    let Ok((k, e)) = scan::token(src, j) else {
                        break;
                    };
                    match k {
                        Kind::Open(b'(') => paren += 1,
                        Kind::Close(b')') => paren -= 1,
                        Kind::Open(b'{') if paren == 0 => {
                            open = Some(j);
                            break;
                        }
                        _ => {}
                    }
                    j = e;
                }
                let Some(open) = open else { break };
                let close = scan::find_close(src, open, src.len()).unwrap_or(src.len());
                inside_component(src, open + 1, close, &mut out);
                i = (close + 1).min(src.len());
                depth = 0;
                continue;
            }
            _ => {}
        }
        i = end;
    }
    out
}

fn inside_component(src: &str, from: usize, to: usize, out: &mut Blocks) {
    let mut i = from;
    let mut depth = 0usize;
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
                {
                    let close = scan::find_close(src, after, to).unwrap_or(to);
                    if &src[i..end] == "view" { &mut out.views } else { &mut out.styles }
                        .push((after + 1, close));
                    i = (close + 1).min(to);
                    continue;
                }
            }
            _ => {}
        }
        i = end;
    }
}

/// The markup of the views, as HTML
pub fn html(src: &str) -> String {
    let mut state = vec![MASK; src.len()];
    for (from, to) in blocks(src).views {
        view(src, from, to, &mut state);
    }
    render(src, &state)
}

/// The `style { }` blocks, as CSS
pub fn css(src: &str) -> String {
    let mut state = vec![MASK; src.len()];
    for (from, to) in blocks(src).styles {
        state[from..to].fill(KEEP);
    }
    render(src, &state)
}

fn render(src: &str, state: &[u8]) -> String {
    let mut out = String::with_capacity(src.len());
    for (i, c) in src.char_indices() {
        match state[i] {
            KEEP => out.push(c),
            QUOTE => out.push('"'),
            _ if c == '\n' || c == '\r' => out.push(c),
            _ => out.extend(std::iter::repeat_n(' ', c.len_utf16())),
        }
    }
    out
}

fn keep(state: &mut [u8], from: usize, to: usize) {
    let to = to.min(state.len());
    state[from..to].fill(KEEP);
}

fn view(src: &str, from: usize, to: usize, state: &mut [u8]) {
    let mut i = from;
    while i < to {
        let Ok((kind, end)) = scan::token(&src[..to], i) else {
            return;
        };
        match kind {
            Kind::Punct if &src[i..end] == "<" => {
                i = tag(src, i, end, to, state);
                continue;
            }
            Kind::Str => {
                let inner_end =
                    if end > i + 1 && src[..end].ends_with('"') { end - 1 } else { end };
                let b = src.as_bytes();
                let mut j = i + 1;
                while j < inner_end {
                    match b[j] {
                        b'\\' => j += 2,
                        b'{' => {
                            // `{{` is a literal brace, `{expr}` is Rust
                            if b.get(j + 1) == Some(&b'{') {
                                j += 2;
                            } else {
                                j = scan::find_close(src, j, inner_end)
                                    .map_or(inner_end, |c| c + 1);
                            }
                        }
                        _ => {
                            let n = src[j..].chars().next().map_or(1, char::len_utf8);
                            keep(state, j, j + n);
                            j += n;
                        }
                    }
                }
            }
            Kind::Open(b'{') => {
                i = scan::find_close(src, i, to).map_or(to, |c| c + 1);
                continue;
            }
            Kind::Ident if matches!(&src[i..end], "if" | "for") => {
                // head up to the block's `{`, which is blanked as well
                let mut j = end;
                let mut depth = 0i32;
                while j < to {
                    let Ok((k, e)) = scan::token(&src[..to], j) else {
                        return;
                    };
                    match k {
                        Kind::Open(b'{') if depth == 0 => {
                            j = e;
                            break;
                        }
                        Kind::Open(_) => depth += 1,
                        Kind::Close(_) => depth -= 1,
                        _ => {}
                    }
                    j = e;
                }
                i = j;
                continue;
            }
            Kind::Ident if &src[i..end] == "else" => {
                // `else {` ends in the block brace; `else if` continues with the `if` head
                let after = scan::skip_trivia(src, end, to).unwrap_or(end);
                i = if src[after..to].starts_with('{') { after + 1 } else { end };
                continue;
            }
            _ => {}
        }
        i = end;
    }
}

fn tag(src: &str, lt: usize, after_lt: usize, to: usize, state: &mut [u8]) -> usize {
    if src[after_lt..to].starts_with('/') {
        let close = src[after_lt..to].find('>').map_or(to, |n| after_lt + n + 1);
        keep(state, lt, close);
        return close;
    }
    keep(state, lt, after_lt);
    let mut i = after_lt;
    while i < to {
        let Ok((kind, end)) = scan::token(&src[..to], i) else {
            return to;
        };
        match kind {
            Kind::Open(b'{') => {
                // attribute value `{expr}` -> `""`
                let close = scan::find_close(src, i, to);
                state[i] = QUOTE;
                if let Ok(c) = close {
                    state[c] = QUOTE;
                    i = c + 1;
                    continue;
                }
                return to;
            }
            Kind::Punct if src[i..end].contains('>') => {
                keep(state, i, end);
                return end;
            }
            _ => keep(state, i, end),
        }
        i = end;
    }
    to
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_keeps_markup_and_blanks_rust() {
        let src = "use a::b;\ncomponent C() {\n    let x = 1;\n    view {\n        <p class={x} id=\"i\">\"hi {x}\"</p>\n        if x > 1 {\n            <b />\n        } else {\n            <i />\n        }\n    }\n}\n";
        let h = html(src);
        assert_eq!(h.len(), src.len());
        assert_eq!(h.lines().count(), src.lines().count());
        assert!(h.contains("<p class=\" \" id=\"i\"> hi     </p>"), "{h}");
        assert!(h.contains("<b />") && h.contains("<i />"));
        assert!(
            !h.contains("let") && !h.contains("use") && !h.contains("if") && !h.contains("else"),
            "{h}"
        );
    }

    #[test]
    fn css_keeps_only_style_blocks() {
        let src = "component C() {\n    view { <p /> }\n    style {\n        p { color: red; }\n    }\n}\n";
        let c = css(src);
        assert!(c.contains("p { color: red; }") && !c.contains("view") && !c.contains("<p"));
        assert_eq!(c.len(), src.len());
    }

    #[test]
    fn unfinished_markup_does_not_panic() {
        for src in [
            "component C() { view { <di",
            "component C() { view { <p class={",
            "component C() { view { \"abc",
            "component C() { style { a {",
        ] {
            let _ = (html(src), css(src));
        }
    }

    #[test]
    fn non_ascii_text_keeps_utf16_columns() {
        let src = "component C() { view { <b>\"−é\"</b> } }";
        assert_eq!(
            html(src).chars().map(char::len_utf16).sum::<usize>(),
            src.chars().map(char::len_utf16).sum::<usize>()
        );
    }
}
