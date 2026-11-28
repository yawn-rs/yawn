use yawn_rsx::scan::{self, Kind};

pub struct Comp {
    pub name: String,
    /// Byte range of the name in `component Name(...)`
    pub name_at: (usize, usize),
    /// From the `component` keyword (or `pub`) to the closing brace
    pub full: (usize, usize),
    /// The Rust between the opening brace and the first `view` / `style` block
    pub setup: (usize, usize),
    pub params: String,
}

pub fn components(src: &str) -> Vec<Comp> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut depth = 0usize;
    let mut last_pub: Option<usize> = None;
    while i < src.len() {
        let Ok((kind, end)) = scan::token(src, i) else {
            break;
        };
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            Kind::Ident if depth == 0 && &src[i..end] == "pub" => last_pub = Some(i),
            Kind::Ident if depth == 0 && &src[i..end] == "component" => {
                let start = last_pub.filter(|p| src[*p + 3..i].trim().is_empty()).unwrap_or(i);
                let Ok(n0) = scan::skip_trivia(src, end, src.len()) else {
                    break;
                };
                let n1 = src[n0..]
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .map_or(src.len(), |n| n0 + n);
                if n1 == n0 {
                    i = end;
                    continue;
                }
                let (mut j, mut open) = (n1, None);
                // the props are the parentheses right after the generics: `<F: Fn(u8)>` and
                // `where F: Fn(u8)` have their own
                if let Ok(g) = scan::skip_trivia(src, n1, src.len())
                    && src[g..].starts_with('<')
                    && let Ok(close) = scan::find_angle_close(src, g, src.len())
                {
                    j = close + 1;
                }
                let mut params = (j, j);
                if let Ok(p) = scan::skip_trivia(src, j, src.len())
                    && src[p..].starts_with('(')
                    && let Ok(close) = scan::find_close(src, p, src.len())
                {
                    params = (p + 1, close);
                    j = close + 1;
                }
                while j < src.len() {
                    let Ok((k, e)) = scan::token(src, j) else {
                        break;
                    };
                    match k {
                        Kind::Open(b'{') => {
                            open = Some(j);
                            break;
                        }
                        Kind::Open(_) => {
                            j = scan::find_close(src, j, src.len()).map_or(src.len(), |c| c + 1);
                            continue;
                        }
                        _ => {}
                    }
                    j = e;
                }
                let Some(open) = open else { break };
                let close = scan::find_close(src, open, src.len()).unwrap_or(src.len());
                let setup_end = first_block(src, open + 1, close);
                out.push(Comp {
                    name: src[n0..n1].to_string(),
                    name_at: (n0, n1),
                    full: (start, (close + 1).min(src.len())),
                    setup: (open + 1, setup_end),
                    params: src[params.0..params.1.max(params.0)].trim().to_string(),
                });
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

fn first_block(src: &str, from: usize, to: usize) -> usize {
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
                    return i;
                }
            }
            _ => {}
        }
        i = end;
    }
    to
}

/// Byte ranges of every identifier `name` in `src`
pub fn occurrences(src: &str, name: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let Ok((kind, end)) = scan::token(src, i) else {
            break;
        };
        if kind == Kind::Ident && &src[i..end] == name {
            out.push((i, end));
        }
        i = end;
    }
    out
}

/// The identifier containing `offset`, if any
pub fn word_at(src: &str, offset: usize) -> Option<(usize, usize)> {
    let is = |c: char| c.is_alphanumeric() || c == '_';
    let offset = offset.min(src.len());
    let start = src[..offset]
        .rfind(|c: char| !is(c))
        .map_or(0, |n| n + src[n..].chars().next().map_or(1, char::len_utf8));
    let end = src[offset..].find(|c: char| !is(c)).map_or(src.len(), |n| offset + n);
    (end > start).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_components_with_setup_and_params() {
        let src = "use a::B;\npub component Card(title: String, n: i32) {\n    let x = 1;\n    view { <b>{x}</b> }\n}\ncomponent Other() { view { <i/> } }\n";
        let c = components(src);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "Card");
        assert_eq!(c[0].params, "title: String, n: i32");
        assert!(src[c[0].full.0..].starts_with("pub component"));
        assert_eq!(src[c[0].setup.0..c[0].setup.1].trim(), "let x = 1;");
        assert_eq!(c[1].name, "Other");
        assert!(src[c[1].setup.0..c[1].setup.1].trim().is_empty());
    }

    #[test]
    fn params_of_a_generic_component() {
        let src = "component List<F: Fn(u8) -> u8>(items: Vec<u8>, f: F) where F: Fn(u8) { let a = 1; view { <b/> } }";
        let c = components(src);
        assert_eq!(c[0].params, "items: Vec<u8>, f: F");
        assert_eq!(src[c[0].setup.0..c[0].setup.1].trim(), "let a = 1;");
    }

    #[test]
    fn occurrences_skip_strings_and_comments() {
        let src = "use x::Card; // Card\ncomponent A() { view { <Card /> \"Card\" } }";
        assert_eq!(occurrences(src, "Card").len(), 2);
    }

    #[test]
    fn word_at_edges() {
        assert_eq!(word_at("<Card x />", 3), Some((1, 5)));
        assert_eq!(word_at("<Card x />", 5), Some((1, 5)));
        assert_eq!(word_at("a  b", 2), None);
    }
}
