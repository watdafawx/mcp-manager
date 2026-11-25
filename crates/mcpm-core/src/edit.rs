//! Format-preserving MCP entry edits.
use crate::harness::{Format, HarnessFile};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

// Mask trivia rather than deleting it so source byte offsets remain valid.
fn mask(text: &str) -> Result<Vec<u8>> {
    let mut b = text.as_bytes().to_vec();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            i = string_end(&b, i)?;
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                b[i] = b' ';
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            b[i] = b' ';
            b[i + 1] = b' ';
            i += 2;
            while i + 1 < b.len() && !b[i..].starts_with(b"*/") {
                if b[i] != b'\n' && b[i] != b'\r' {
                    b[i] = b' ';
                }
                i += 1;
            }
            ensure!(i + 1 < b.len(), "unterminated comment");
            b[i] = b' ';
            b[i + 1] = b' ';
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(b)
}
fn string_end(b: &[u8], start: usize) -> Result<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    bail!("unterminated string")
}
fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}
fn value_end(b: &[u8], start: usize) -> Result<usize> {
    if b[start] == b'"' {
        return string_end(b, start);
    }
    if b[start] == b'{' || b[start] == b'[' {
        let mut depth = 1;
        let mut i = start + 1;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    i = string_end(b, i)?;
                    continue;
                }
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(i + 1);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        bail!("unclosed container");
    }
    let mut i = start;
    while i < b.len() && !b[i].is_ascii_whitespace() && !b",}]".contains(&b[i]) {
        i += 1;
    }
    Ok(i)
}
#[derive(Clone)]
struct Member {
    key: String,
    start: usize,
    value: usize,
    end: usize,
    comma: Option<usize>,
}
fn members(b: &[u8], start: usize) -> Result<(Vec<Member>, usize)> {
    ensure!(b.get(start) == Some(&b'{'), "expected object");
    let mut i = ws(b, start + 1);
    let mut out = vec![];
    while b.get(i) != Some(&b'}') {
        ensure!(b.get(i) == Some(&b'"'), "expected key");
        let key_end = string_end(b, i)?;
        let key = serde_json::from_slice(&b[i..key_end])?;
        let colon = ws(b, key_end);
        ensure!(b.get(colon) == Some(&b':'), "expected colon");
        let value = ws(b, colon + 1);
        ensure!(value < b.len(), "missing value");
        let end = value_end(b, value)?;
        let next = ws(b, end);
        let comma = (b.get(next) == Some(&b',')).then_some(next);
        out.push(Member {
            key,
            start: i,
            value,
            end,
            comma,
        });
        i = ws(b, comma.map_or(end, |n| n + 1));
    }
    Ok((out, i))
}
/// Parses JSONC or TOML into a generic document, rejecting malformed input.
pub fn read(text: &str, format: Format) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    if format == Format::Toml {
        return Ok(toml_edit::de::from_str(text)?);
    }
    let mut b = mask(text)?;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            i = string_end(&b, i)?;
            continue;
        }
        if b[i] == b',' {
            let next = ws(&b, i + 1);
            if b.get(next).is_some_and(|c| b"}]".contains(c)) {
                b[i] = b' ';
            }
        }
        i += 1;
    }
    Ok(serde_json::from_slice(&b)?)
}
/// Reads a harness MCP container, rejecting non-object containers.
pub fn entries(text: &str, file: &HarnessFile) -> Result<BTreeMap<String, Value>> {
    let doc = read(text, file.format)?;
    match doc.get(&file.container) {
        None => Ok(BTreeMap::new()),
        Some(v) => Ok(serde_json::from_value(v.clone())?),
    }
}

/// Removes or upserts one MCP entry while preserving unrelated source text.
pub fn set(text: &str, file: &HarnessFile, name: &str, value: Option<&Value>) -> Result<String> {
    read(text, file.format)?;
    if file.format == Format::Toml {
        let mut doc = text.parse::<toml_edit::DocumentMut>()?;
        if doc.get(&file.container).is_none() {
            if value.is_none() {
                return Ok(text.into());
            }
            doc[&file.container] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let table = doc[&file.container]
            .as_table_like_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid MCP table"))?;
        if let Some(v) = value {
            let serialized = toml_edit::ser::to_string(&BTreeMap::from([(name, v)]))?;
            let mut entry = serialized.parse::<toml_edit::DocumentMut>()?;
            table.insert(name, entry.as_table_mut().remove(name).unwrap());
        } else {
            table.remove(name);
        }
        return Ok(doc.to_string());
    }
    let source = if text.trim().is_empty() { "{}" } else { text };
    let b = mask(source)?;
    let root_start = ws(&b, 0);
    let (root, root_end) = members(&b, root_start)?;
    let mut out = source.to_string();
    let Some(container) = root.iter().find(|m| m.key == file.container) else {
        if value.is_none() {
            return Ok(source.into());
        }
        let v = json!({ name: value });
        insert_member(&mut out, &root, root_start, root_end, &file.container, &v)?;
        read(&out, file.format)?;
        return Ok(out);
    };
    let (items, end) = members(&b, container.value)?;
    if let Some((index, item)) = items.iter().enumerate().find(|(_, m)| m.key == name) {
        if let Some(v) = value {
            let indent = line_indent(source, item.start).unwrap_or_default();
            out.replace_range(item.value..item.end, &pretty(v, &indent)?);
        } else {
            let mut start = item.start;
            let mut stop = item.comma.map_or(item.end, |c| c + 1);
            // Drop the whole line when the entry sat on lines of its own.
            if line_indent(source, item.start).is_some() {
                let rest = &source[stop..];
                let eol = rest.find('\n');
                if eol.is_some_and(|n| rest[..n].trim().is_empty()) {
                    start = source[..item.start].rfind('\n').map_or(start, |n| n + 1);
                    stop += eol.unwrap_or(0) + 1;
                }
            }
            out.replace_range(start..stop, "");
            if item.comma.is_none()
                && index > 0
                && let Some(comma) = items[index - 1].comma
            {
                out.replace_range(comma..comma + 1, "");
            }
        }
    } else if let Some(v) = value {
        insert_member(&mut out, &items, container.value, end, name, v)?;
    }
    read(&out, file.format)?;
    Ok(out)
}

/// Whitespace before `pos` on its line, or `None` if other text precedes it there.
fn line_indent(text: &str, pos: usize) -> Option<String> {
    let line_start = text[..pos].rfind('\n').map_or(0, |n| n + 1);
    let lead = &text[line_start..pos];
    (lead.trim().is_empty() && line_start > 0).then(|| lead.to_string())
}

/// Pretty JSON whose continuation lines start at `indent`.
fn pretty(v: &Value, indent: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)?.replace('\n', &format!("\n{indent}")))
}

/// Appends `"key": value` to the object spanning `open..close`, matching sibling indentation.
fn insert_member(
    out: &mut String,
    items: &[Member],
    open: usize,
    close: usize,
    key: &str,
    v: &Value,
) -> Result<()> {
    let key = serde_json::to_string(key)?;
    if let Some(last) = items.last() {
        let own_line = line_indent(out, last.start);
        let sep = if own_line.is_some() { "\n" } else { " " };
        let indent = own_line.unwrap_or_default();
        let comma = if last.comma.is_none() { "," } else { "" };
        let at = last.comma.map_or(last.end, |c| c + 1);
        out.insert_str(
            at,
            &format!("{comma}{sep}{indent}{key}: {}", pretty(v, &indent)?),
        );
    } else {
        let outer = line_indent(out, open).unwrap_or_else(|| {
            let ls = out[..open].rfind('\n').map_or(0, |n| n + 1);
            out[ls..]
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect()
        });
        let indent = format!("{outer}  ");
        let member = format!("\n{indent}{key}: {}\n{outer}", pretty(v, &indent)?);
        if out[open + 1..close].trim().is_empty() {
            out.replace_range(open + 1..close, &member);
        } else {
            out.insert_str(close, &member); // keep comments inside an otherwise empty object
        }
    }
    Ok(())
}
/// Builds the harness-specific manager entry.
pub fn manager_entry(harness: &str, port: u16, exe: &std::path::Path) -> Value {
    let url = format!("http://127.0.0.1:{port}/mcp/{harness}");
    match harness {
        "claude-desktop" => json!({"command":exe,"args":["bridge","--harness",harness]}),
        "opencode" => json!({"type":"remote","url":url,"enabled":true}),
        "antigravity" | "windsurf" => json!({"serverUrl":url}),
        "gemini-cli" => json!({"httpUrl":url}),
        "cursor" | "codex" => json!({"url":url}),
        _ => json!({"type":"http","url":url}),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comments() {
        let d = tempfile::tempdir().unwrap();
        let f = crate::harness::registry(d.path(), d.path())
            .remove(3)
            .files
            .remove(0);
        let s = "{ // keep\n\"mcp\": {\"a\": {\"command\":[\"node\"]},}, \"other\": 2}";
        let next = set(s, &f, "a", None).unwrap();
        assert!(next.contains("// keep"));
        assert!(entries(&next, &f).unwrap().is_empty());
        let next = set(&next, &f, "mcpm", Some(&json!({"url":"http://a"}))).unwrap();
        assert_eq!(entries(&next, &f).unwrap().len(), 1);
    }
    #[test]
    fn keeps_indentation() {
        let d = tempfile::tempdir().unwrap();
        let f = crate::harness::registry(d.path(), d.path())
            .remove(3)
            .files
            .remove(0);
        let s = "{\n  \"mcp\": {\n    \"a\": { \"command\": [\"node\"] },\n    \"b\": { \"command\": [\"x\"] }\n  },\n  \"other\": 2\n}\n";
        let removed = set(s, &f, "a", None).unwrap();
        assert_eq!(
            removed,
            "{\n  \"mcp\": {\n    \"b\": { \"command\": [\"x\"] }\n  },\n  \"other\": 2\n}\n"
        );
        let added = set(&removed, &f, "mcpm", Some(&json!({"url": "http://a"}))).unwrap();
        assert!(
            added.contains("\n    \"mcpm\": {\n      \"url\": \"http://a\"\n    }\n  },"),
            "{added}"
        );
        let gone = set(&set(&added, &f, "b", None).unwrap(), &f, "mcpm", None).unwrap();
        assert_eq!(entries(&gone, &f).unwrap().len(), 0);
        let fresh = set(
            "{\n  \"mcp\": {}\n}\n",
            &f,
            "mcpm",
            Some(&json!({"url": "u"})),
        )
        .unwrap();
        assert_eq!(
            fresh,
            "{\n  \"mcp\": {\n    \"mcpm\": {\n      \"url\": \"u\"\n    }\n  }\n}\n"
        );
    }
}
