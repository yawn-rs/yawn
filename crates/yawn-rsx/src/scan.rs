use crate::diag::Diagnostic;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Space,
    Comment,
    Str,
    Char,
    Lifetime,
    Ident,
    Number,
    Open(u8),
    Close(u8),
    Punct,
}

/// Reads the token starting at `i` (which must be `< s.len()`), returns its kind and end
pub fn token(s: &str, i: usize) -> Result<(Kind, usize), Diagnostic> {
    let b = s.as_bytes();
    let c = b[i];
    match c {
        b' ' | b'\t' | b'\n' | b'\r' => {
            let mut j = i;
            while j < b.len() && matches!(b[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            Ok((Kind::Space, j))
        }
        b'/' if b.get(i + 1) == Some(&b'/') => {
            let j = s[i..].find('\n').map(|n| i + n).unwrap_or(s.len());
            Ok((Kind::Comment, j))
        }
        b'/' if b.get(i + 1) == Some(&b'*') => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                if b[j] == b'/' && b.get(j + 1) == Some(&b'*') {
                    depth += 1;
                    j += 2;
                } else if b[j] == b'*' && b.get(j + 1) == Some(&b'/') {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        return Ok((Kind::Comment, j));
                    }
                } else {
                    j += 1;
                }
            }
            Err(Diagnostic::new("unterminated block comment", i..i + 2)
                .label("this comment is never closed"))
        }
        b'"' => string_end(s, i, i).map(|e| (Kind::Str, e)),
        b'\'' => Ok(char_or_lifetime(s, i)),
        b'(' | b'[' | b'{' => Ok((Kind::Open(c), i + 1)),
        b')' | b']' | b'}' => Ok((Kind::Close(c), i + 1)),
        b'0'..=b'9' => {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            Ok((Kind::Number, j))
        }
        _ if is_ident_start(s, i) => {
            // Prefixed literals: r"..", r#".."#, b"..", br"..", c"..".
            if let Some(end) = prefixed_string(s, i)? {
                return Ok((Kind::Str, end));
            }
            if c == b'b' && b.get(i + 1) == Some(&b'\'') {
                let (_, e) = char_or_lifetime(s, i + 1);
                return Ok((Kind::Char, e));
            }
            let mut j = i;
            for (o, ch) in s[i..].char_indices() {
                if ch.is_alphanumeric() || ch == '_' {
                    j = i + o + ch.len_utf8();
                } else {
                    break;
                }
            }
            Ok((Kind::Ident, j))
        }
        _ => {
            let ch = s[i..].chars().next().unwrap();
            Ok((Kind::Punct, i + ch.len_utf8()))
        }
    }
}

fn is_ident_start(s: &str, i: usize) -> bool {
    s[i..].chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
}

fn string_end(s: &str, start: usize, lit_start: usize) -> Result<usize, Diagnostic> {
    let b = s.as_bytes();
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Ok(j + 1),
            _ => j += 1,
        }
    }
    Err(Diagnostic::new("unterminated string literal", lit_start..lit_start + 1)
        .label("this string is never closed"))
}

fn prefixed_string(s: &str, i: usize) -> Result<Option<usize>, Diagnostic> {
    let b = s.as_bytes();
    let mut j = i;
    if b[j] == b'b' || b[j] == b'c' {
        j += 1;
    }
    let raw = b.get(j) == Some(&b'r');
    if raw {
        j += 1;
    }
    if j == i {
        return Ok(None);
    }
    let mut hashes = 0;
    if raw {
        while b.get(j) == Some(&b'#') {
            hashes += 1;
            j += 1;
        }
    }
    if b.get(j) != Some(&b'"') {
        return Ok(None);
    }
    if !raw {
        return string_end(s, j, i).map(Some);
    }
    let closer = format!("\"{}", "#".repeat(hashes));
    match s[j + 1..].find(&closer) {
        Some(n) => Ok(Some(j + 1 + n + closer.len())),
        None => Err(Diagnostic::new("unterminated raw string literal", i..j + 1)),
    }
}

fn char_or_lifetime(s: &str, i: usize) -> (Kind, usize) {
    let b = s.as_bytes();
    if b.get(i + 1) == Some(&b'\\') {
        let mut j = i + 2;
        while j < b.len() && b[j] != b'\'' {
            j += 1;
        }
        return (Kind::Char, (j + 1).min(b.len()));
    }
    let Some(c) = s[i + 1..].chars().next() else {
        return (Kind::Punct, i + 1);
    };
    let after = i + 1 + c.len_utf8();
    if b.get(after) == Some(&b'\'') {
        return (Kind::Char, after + 1);
    }
    let mut j = i + 1;
    for (o, ch) in s[i + 1..].char_indices() {
        if ch.is_alphanumeric() || ch == '_' {
            j = i + 1 + o + ch.len_utf8();
        } else {
            break;
        }
    }
    (Kind::Lifetime, j.max(i + 1))
}

/// Given the position of an opening delimiter, returns the position of its matching closer
pub fn find_close(s: &str, open: usize, limit: usize) -> Result<usize, Diagnostic> {
    let s = &s[..limit];
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut i = open;
    while i < s.len() {
        let (kind, end) = token(s, i)?;
        match kind {
            Kind::Open(c) => stack.push((c, i)),
            Kind::Close(c) => {
                let Some((o, at)) = stack.pop() else {
                    return Err(Diagnostic::new(
                        format!("unexpected closing `{}`", c as char),
                        i..i + 1,
                    ));
                };
                if closer_of(o) != c {
                    return Err(Diagnostic::new(
                        format!(
                            "mismatched delimiter: expected `{}`, found `{}`",
                            closer_of(o) as char,
                            c as char
                        ),
                        i..i + 1,
                    )
                    .note(at..at + 1, format!("`{}` opened here", o as char)));
                }
                if stack.is_empty() {
                    return Ok(i);
                }
            }
            _ => {}
        }
        i = end;
    }
    let (o, at) = stack.last().copied().unwrap_or((s.as_bytes()[open], open));
    Err(Diagnostic::new(format!("unclosed `{}`", o as char), at..at + 1)
        .label(format!("this `{}` is never closed", o as char)))
}

/// The `>` that closes the `<` at `open`. The `>` of `->` doesn't count, and neither does anything
/// in brackets (`[u8; N >> 1]`)
pub fn find_angle_close(s: &str, open: usize, limit: usize) -> Result<usize, Diagnostic> {
    let mut depth = 0usize;
    let mut i = open;
    while i < limit {
        let (kind, end) = token(&s[..limit], i)?;
        match kind {
            Kind::Open(_) => {
                i = find_close(s, i, limit)? + 1;
                continue;
            }
            Kind::Punct if &s[i..end] == "<" => depth += 1,
            Kind::Punct if &s[i..end] == ">" && !s[..i].ends_with('-') => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => {}
        }
        i = end;
    }
    Err(Diagnostic::new("unclosed `<`", open..open + 1).label("this `<` is never closed"))
}

pub fn closer_of(open: u8) -> u8 {
    match open {
        b'(' => b')',
        b'[' => b']',
        _ => b'}',
    }
}

/// Skips whitespace and comments from `i`
pub fn skip_trivia(s: &str, mut i: usize, limit: usize) -> Result<usize, Diagnostic> {
    while i < limit {
        match token(&s[..limit], i)? {
            (Kind::Space | Kind::Comment, e) => i = e,
            _ => break,
        }
    }
    Ok(i)
}

/// Whether `src` uses `ident` as a name; `a.item` (a field access) doesn't mention `item`
pub fn mentions(src: &str, ident: &str) -> bool {
    let mut i = 0;
    let mut prev_dot = false;
    while i < src.len() {
        let Ok((kind, end)) = token(src, i) else {
            return false;
        };
        match kind {
            Kind::Ident if &src[i..end] == ident && !prev_dot => return true,
            Kind::Space | Kind::Comment => {}
            Kind::Punct => prev_dot = &src[i..end] == ".",
            _ => prev_dot = false,
        }
        i = end;
    }
    false
}

/// Finds the first position at nesting depth 0 where `pred(kind, text, start)` holds
pub fn find_top_level(
    s: &str,
    from: usize,
    to: usize,
    mut pred: impl FnMut(Kind, &str, usize) -> bool,
) -> Result<Option<usize>, Diagnostic> {
    let s = &s[..to];
    let mut depth = 0usize;
    let mut i = from;
    while i < s.len() {
        let (kind, end) = token(s, i)?;
        match kind {
            Kind::Open(_) => depth += 1,
            Kind::Close(_) => depth = depth.saturating_sub(1),
            _ => {
                if depth == 0 && pred(kind, &s[i..end], i) {
                    return Ok(Some(i));
                }
            }
        }
        i = end;
    }
    Ok(None)
}
