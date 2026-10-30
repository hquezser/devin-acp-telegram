//! Devin's Markdown → Telegram message entities, for a small safe subset:
//! fenced code blocks, `inline code`, **bold** and [links](url). Entities instead of
//! `parse_mode` mean nothing needs escaping, and unmatched markers simply
//! stay literal.

use serde_json::{Value, json};

/// Plain text plus its `entities`. Also shortens the home directory to `~`.
pub fn render(text: &str) -> (String, Vec<Value>) {
    render_in(text, std::env::var("HOME").ok().as_deref())
}

fn render_in(text: &str, home: Option<&str>) -> (String, Vec<Value>) {
    let text = match home {
        Some(h) if h.len() > 1 => text.replace(&format!("{}/", h.trim_end_matches('/')), "~/"),
        _ => text.to_string(),
    };
    let mut out = String::new();
    let mut ents = Vec::new();
    let mut rest = text.as_str();
    while let Some(c) = rest.chars().next() {
        if let Some(r) = rest.strip_prefix("```")
            && let Some(end) = r.find("```")
        {
            let body = &r[..end];
            let (lang, code) = match body.split_once('\n') {
                Some((l, c)) if !l.trim().contains(' ') => (l.trim(), c),
                _ => ("", body),
            };
            let mut e = json!({"type": "pre"});
            if !lang.is_empty() {
                e["language"] = json!(lang);
            }
            push(&mut out, &mut ents, code.trim_end_matches('\n'), e);
            rest = &r[end + 3..];
        } else if let Some((inner, after)) = span(rest, "`") {
            push(&mut out, &mut ents, inner, json!({"type": "code"}));
            rest = after;
        } else if let Some((inner, after)) = span(rest, "**") {
            push(&mut out, &mut ents, inner, json!({"type": "bold"}));
            rest = after;
        } else if let Some((label, url, after)) = link(rest) {
            // Web links stay clickable; local ones (`file://`, paths) mean
            // nothing on a phone: keep their label only.
            let e = if url.starts_with("https://") || url.starts_with("http://") {
                json!({"type": "text_link", "url": url})
            } else {
                json!({"type": "code"})
            };
            push(&mut out, &mut ents, label.trim_matches('`'), e);
            rest = after;
        } else {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    (out, ents)
}

/// `rest` starts with `mark`, closed by `mark` later on the same line.
fn span<'a>(rest: &'a str, mark: &str) -> Option<(&'a str, &'a str)> {
    let r = rest.strip_prefix(mark)?;
    let end = r.find(mark)?;
    let inner = &r[..end];
    (!inner.is_empty() && !inner.contains('\n') && !inner.starts_with(' '))
        .then(|| (inner, &r[end + mark.len()..]))
}

/// `[label](url)` at the start of `rest`, on one line.
fn link(rest: &str) -> Option<(&str, &str, &str)> {
    let r = rest.strip_prefix('[')?;
    let close = r.find("](")?;
    let label = &r[..close];
    let r = &r[close + 2..];
    let end = r.find(')')?;
    let url = &r[..end];
    (!label.is_empty() && !label.contains(['\n', '[']) && !url.contains([' ', '\n']))
        .then(|| (label, url, &r[end + 1..]))
}

/// Appends `s` to `out` with an entity covering it (offsets in UTF-16).
fn push(out: &mut String, ents: &mut Vec<Value>, s: &str, mut e: Value) {
    let len = utf16(s);
    if len > 0 {
        e["offset"] = json!(utf16(out));
        e["length"] = json!(len);
        ents.push(e);
    }
    out.push_str(s);
}

fn utf16(s: &str) -> usize {
    s.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(t: &str) -> (String, Vec<Value>) {
        render_in(t, Some("/Users/me"))
    }

    #[test]
    fn inline_code_and_bold() {
        let (t, e) = r("Fixed `cart.py`, **both** pass");
        assert_eq!(t, "Fixed cart.py, both pass");
        assert_eq!(e[0], json!({"type": "code", "offset": 6, "length": 7}));
        assert_eq!(e[1], json!({"type": "bold", "offset": 15, "length": 4}));
    }

    #[test]
    fn offsets_count_utf16_units() {
        let (t, e) = r("🔐 Allow? `git push`");
        assert_eq!(t, "🔐 Allow? git push");
        assert_eq!(e[0]["offset"], 10);
    }

    #[test]
    fn fenced_block_with_language() {
        let (t, e) = r("Run:\n```bash\ncargo test\n```\ndone");
        assert_eq!(t, "Run:\ncargo test\ndone");
        assert_eq!(
            e[0],
            json!({"type": "pre", "language": "bash", "offset": 5, "length": 10})
        );
    }

    #[test]
    fn unmatched_markers_stay_literal() {
        assert_eq!(r("a ` b").0, "a ` b");
        assert_eq!(r("2 ** 3 and x").0, "2 ** 3 and x");
        assert_eq!(r("```\nopen").0, "```\nopen");
        assert!(r("a ` b").1.is_empty());
    }

    #[test]
    fn links() {
        let (t, e) = r("In [cart.py:7](file:///Users/me/shop/cart.py) see [docs](https://x.dev/a)");
        assert_eq!(t, "In cart.py:7 see docs");
        assert_eq!(e[0], json!({"type": "code", "offset": 3, "length": 9}));
        assert_eq!(
            e[1],
            json!({"type": "text_link", "url": "https://x.dev/a", "offset": 17, "length": 4})
        );
        assert_eq!(r("[x] done (yes)").0, "[x] done (yes)");
    }

    #[test]
    fn home_is_shortened() {
        assert_eq!(r("cd /Users/me/code/app").0, "cd ~/code/app");
        assert_eq!(r("/Users/meow/x").0, "/Users/meow/x");
    }
}
