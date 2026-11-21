//! Finding the texts to translate: the calls of [`crate::tr`],
//! [`crate::trc`] and [`crate::trf`] with a string literal in the Rust code,
//! and the `translatable="yes"` elements of the GTK `.ui` files. Also puts
//! translations into a `.ui` file, for the build of the GTK frontend.

use std::path::{Path, PathBuf};

/// a text to translate and where it is
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub context: Option<String>,
    pub id: String,
    /// the English text for other numbers than one, for a text with a
    /// number
    pub plural: Option<String>,
    /// `file:line`, relative to the workspace
    pub location: String,
}

/// The folders, relative to the workspace, whose files have texts to
/// translate.
pub const SOURCES: &[&str] = &["src", "rustinput-gtk/src", "rustinput-gtk/resources"];

/// the Rust and `.ui` files of [`SOURCES`] in `workspace`, sorted
pub fn files(workspace: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs" || e == "ui") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    for dir in SOURCES {
        walk(&workspace.join(dir), &mut out);
    }
    out.sort();
    out
}

/// All the texts to translate in `workspace`, in the order of the files and
/// of the lines, each once (at its first place).
pub fn messages(workspace: &Path) -> Vec<Message> {
    let mut all: Vec<Message> = Vec::new();
    for path in files(workspace) {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let relative = path
            .strip_prefix(workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let found = if path.extension().is_some_and(|e| e == "ui") {
            from_ui(&source)
                .into_iter()
                .map(|(line, context, id)| (line, context, id, None))
                .collect()
        } else {
            from_rust(&source)
        };
        for (line, context, id, plural) in found {
            if !all.iter().any(|m| m.context == context && m.id == id) {
                all.push(Message {
                    context,
                    id,
                    plural,
                    location: format!("{relative}:{line}"),
                });
            }
        }
    }
    all
}

fn line_of(source: &str, at: usize) -> usize {
    source[..at].matches('\n').count() + 1
}

/// the texts of `tr("…")`, `trf("…", …)`, `mark("…")`, `trc("context", "…")`
/// and `trn("one", "other", …)` in Rust code, with their lines: line, context,
/// text, text for other numbers
pub fn from_rust(source: &str) -> Vec<(usize, Option<String>, String, Option<String>)> {
    let mut found = Vec::new();
    for name in ["tr", "trf", "trc", "trn", "mark"] {
        let call = format!("{name}(");
        let mut from = 0;
        while let Some(i) = source[from..].find(&call) {
            let at = from + i;
            from = at + call.len();
            // a call of the function itself, not of `attr(` or `.tr(`
            let before = source[..at].chars().next_back();
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            // not its definition
            if source[..at].ends_with("fn ") {
                continue;
            }
            let Some((first, end)) = literal(source, from) else {
                continue;
            };
            if name == "trc" || name == "trn" {
                let rest = source[end..].trim_start();
                let Some(rest) = rest.strip_prefix(',') else {
                    continue;
                };
                let start = source.len() - rest.len();
                let Some((second, _)) = literal(source, start) else {
                    continue;
                };
                if name == "trc" {
                    found.push((line_of(source, at), Some(first), second, None));
                } else {
                    found.push((line_of(source, at), None, first, Some(second)));
                }
            } else {
                found.push((line_of(source, at), None, first, None));
            }
        }
    }
    found.sort_by_key(|(line, ..)| *line);
    found
}

/// the Rust string literal starting at `from` (after blanks), unescaped,
/// and where it ends
fn literal(source: &str, from: usize) -> Option<(String, usize)> {
    let rest = &source[from..];
    let trimmed = rest.trim_start();
    let start = from + (rest.len() - trimmed.len());
    let mut chars = source[start..].char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let mut text = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((text, start + i + 1)),
            '\\' => match chars.next()?.1 {
                'n' => text.push('\n'),
                't' => text.push('\t'),
                'r' => text.push('\r'),
                '0' => text.push('\0'),
                '"' => text.push('"'),
                '\'' => text.push('\''),
                '\\' => text.push('\\'),
                'u' => {
                    // \u{…}
                    let mut hex = String::new();
                    for (_, c) in chars.by_ref() {
                        match c {
                            '{' => {}
                            '}' => break,
                            c => hex.push(c),
                        }
                    }
                    text.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                // a line continued (also with Windows line ends): the
                // blanks of the next line go
                '\n' | '\r' => {
                    for (_, c) in chars.clone() {
                        if !c.is_whitespace() {
                            break;
                        }
                        chars.next();
                    }
                }
                _ => return None,
            },
            c => text.push(c),
        }
    }
    None
}

/// a translatable element of a `.ui` file: where its text is and its
/// context
struct Translatable {
    /// the text, as written in the file (XML escaped)
    start: usize,
    end: usize,
    context: Option<String>,
}

fn translatables(source: &str) -> Vec<Translatable> {
    // what is commented out is not shown
    let mut comments = Vec::new();
    let mut from = 0;
    while let Some(i) = source[from..].find("<!--") {
        let start = from + i;
        let end = source[start..]
            .find("-->")
            .map_or(source.len(), |e| start + e + 3);
        comments.push(start..end);
        from = end;
    }
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = source[from..].find("translatable=\"yes\"") {
        let at = from + i;
        from = at + 1;
        if comments.iter().any(|c| c.contains(&at)) {
            continue;
        }
        let Some(open) = source[..at].rfind('<') else {
            continue;
        };
        let Some(close) = source[at..].find('>').map(|c| at + c) else {
            continue;
        };
        let tag = &source[open + 1..close];
        let name: String = tag
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        if tag.ends_with('/') {
            continue;
        }
        let context = tag.find("context=\"").map(|c| {
            let value = &tag[c + 9..];
            unescape(&value[..value.find('"').unwrap_or(value.len())])
        });
        let end_tag = format!("</{name}>");
        let Some(end) = source[close..].find(&end_tag).map(|e| close + e) else {
            continue;
        };
        found.push(Translatable {
            start: close + 1,
            end,
            context,
        });
    }
    found
}

/// the texts of the `translatable="yes"` elements of a `.ui` file, with
/// their lines
pub fn from_ui(source: &str) -> Vec<(usize, Option<String>, String)> {
    translatables(source)
        .into_iter()
        .map(|t| {
            (
                line_of(source, t.start),
                t.context,
                unescape(&source[t.start..t.end]),
            )
        })
        .filter(|(_, _, id)| !id.trim().is_empty())
        .collect()
}

/// The `.ui` file `source` with the text of each translatable element
/// replaced by what `translate` gives for it (its context and its English
/// text); the elements it gives nothing for stay in English.
pub fn translate_ui(
    source: &str,
    translate: impl Fn(Option<&str>, &str) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    for t in translatables(source) {
        let id = unescape(&source[t.start..t.end]);
        if let Some(text) = translate(t.context.as_deref(), &id) {
            out.push_str(&source[last..t.start]);
            out.push_str(&escape(&text));
            last = t.end;
        }
    }
    out.push_str(&source[last..]);
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..semi];
        let c = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                .ok()
                .and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_in_rust() {
        let source = r#"
            let a = tr("Apply");
            let b = trf(
                "Arrangement sent to {name}",
                &[("name", &name)],
            );
            let c = trc("side", "Left");
            let d = attr("not this");
            let e = tr(variable);
            let f = tr("A \"quoted\" word\nand a line");
            let g = trn("{count} file", "{count} files", n, &[]);
            pub fn tr(text: &'static str) -> &'static str { text }
        "#;
        let found: Vec<_> = from_rust(source)
            .into_iter()
            .map(|(_, c, id, plural)| (c, id, plural))
            .collect();
        assert_eq!(
            found,
            vec![
                (None, "Apply".to_owned(), None),
                (None, "Arrangement sent to {name}".to_owned(), None),
                (Some("side".to_owned()), "Left".to_owned(), None),
                (None, "A \"quoted\" word\nand a line".to_owned(), None),
                (
                    None,
                    "{count} file".to_owned(),
                    Some("{count} files".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn lines_continued_with_windows_line_ends() {
        let source = "let a = tr(\"continued \\\r\n        on the next line\");";
        let found: Vec<String> = from_rust(source)
            .into_iter()
            .map(|(_, _, id, _)| id)
            .collect();
        assert_eq!(found, ["continued on the next line"]);
    }

    #[test]
    fn elements_of_ui_files() {
        let source = r#"<interface>
  <property name="title" translatable="yes">Copy &amp; paste</property>
  <property name="label">RustInput</property>
  <item translatable="yes" context="theme">Light</item>
  <attribute name="label" translatable="yes">_Quit</attribute>
  <!-- <property name="title" translatable="yes">hostname</property> -->
</interface>"#;
        let found: Vec<_> = from_ui(source)
            .into_iter()
            .map(|(_, c, id)| (c, id))
            .collect();
        assert_eq!(
            found,
            vec![
                (None, "Copy & paste".to_owned()),
                (Some("theme".to_owned()), "Light".to_owned()),
                (None, "_Quit".to_owned()),
            ]
        );
        let translated = translate_ui(source, |context, id| match (context, id) {
            (None, "Copy & paste") => Some("Copiar <e> colar".to_owned()),
            (Some("theme"), "Light") => Some("Claro".to_owned()),
            _ => None,
        });
        assert!(translated.contains(">Copiar &lt;e&gt; colar</property>"));
        assert!(translated.contains("context=\"theme\">Claro</item>"));
        assert!(translated.contains(">_Quit</attribute>"));
        assert!(translated.contains(">RustInput</property>"));
    }
}
