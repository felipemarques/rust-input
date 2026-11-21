//! Reading the translations: gettext PO files, as far as RustInput uses
//! them. Shared by the build script, which compiles the translations into
//! the program, and the tests.

/// one message of a PO file
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// `msgctxt`: tells apart the same English text with different meanings
    pub context: Option<String>,
    /// `msgid`: the English text
    pub id: String,
    /// `msgid_plural`: the English text for other numbers than one, for a
    /// text with a number
    pub plural: Option<String>,
    /// `msgstr`: the translation, empty when not translated yet; for a text
    /// with a number its forms (`msgstr[0]`, `msgstr[1]`, ...) joined by
    /// U+0000
    pub text: String,
    /// marked `#, fuzzy`: a guess that still needs a look, not used
    pub fuzzy: bool,
}

impl Entry {
    /// how the message is looked up: the context and the English text
    /// joined by U+0004, as gettext does
    pub fn key(&self) -> String {
        key(self.context.as_deref(), &self.id)
    }
}

/// the key of a message with `context` and the English text `id`
pub fn key(context: Option<&str>, id: &str) -> String {
    match context {
        Some(context) => format!("{context}\u{4}{id}"),
        None => id.to_owned(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Context,
    Id,
    Plural,
    Text,
    Form(usize),
}

/// the forms of a text with a number, as [`Entry::text`] keeps them
pub fn forms(text: &str) -> Vec<&str> {
    text.split('\0').collect()
}

/// The messages of the PO file `source`, without the header (the message
/// with the empty English text) and without obsolete ones (`#~`).
pub fn parse(source: &str) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut entry = Entry::default();
    let mut forms: Vec<String> = Vec::new();
    let mut field: Option<Field> = None;
    let mut started = false;

    let finish = |entry: &mut Entry,
                  forms: &mut Vec<String>,
                  started: &mut bool,
                  entries: &mut Vec<Entry>| {
        if !forms.is_empty() {
            entry.text = forms.join("\0");
            forms.clear();
        }
        if *started && !entry.id.is_empty() {
            entries.push(std::mem::take(entry));
        }
        *entry = Entry::default();
        *started = false;
    };

    for (number, line) in source.lines().enumerate() {
        let line = line.trim();
        let at = |e: String| format!("line {}: {e}", number + 1);
        if line.is_empty() {
            finish(&mut entry, &mut forms, &mut started, &mut entries);
            field = None;
            continue;
        }
        if line.starts_with("#~") {
            continue;
        }
        if let Some(flags) = line.strip_prefix("#,") {
            if started && matches!(field, Some(Field::Text | Field::Form(_))) {
                finish(&mut entry, &mut forms, &mut started, &mut entries);
            }
            if flags.split(',').any(|f| f.trim() == "fuzzy") {
                entry.fuzzy = true;
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = match line.find(char::is_whitespace) {
            Some(i) if !line.starts_with('"') => (&line[..i], line[i..].trim_start()),
            _ => ("", line),
        };
        let value = unquote(rest).map_err(at)?;
        match keyword {
            "msgctxt" | "msgid" => {
                // a new message right after the translation of the last one
                if started && matches!(field, Some(Field::Text | Field::Form(_))) {
                    finish(&mut entry, &mut forms, &mut started, &mut entries);
                }
                started = true;
                if keyword == "msgctxt" {
                    entry.context = Some(value);
                    field = Some(Field::Context);
                } else {
                    entry.id = value;
                    field = Some(Field::Id);
                }
            }
            "msgstr" => {
                entry.text = value;
                field = Some(Field::Text);
            }
            "msgid_plural" => {
                entry.plural = Some(value);
                field = Some(Field::Plural);
            }
            "" => match field {
                Some(Field::Context) => entry
                    .context
                    .get_or_insert_with(String::new)
                    .push_str(&value),
                Some(Field::Id) => entry.id.push_str(&value),
                Some(Field::Plural) => entry
                    .plural
                    .get_or_insert_with(String::new)
                    .push_str(&value),
                Some(Field::Text) => entry.text.push_str(&value),
                Some(Field::Form(i)) => forms[i].push_str(&value),
                None => return Err(at("a string outside of a message".into())),
            },
            other if other.starts_with("msgstr[") => {
                let index: usize = other["msgstr[".len()..]
                    .trim_end_matches(']')
                    .parse()
                    .map_err(|_| at(format!("not a form: {other}")))?;
                if index != forms.len() {
                    return Err(at(format!("{other} out of order")));
                }
                forms.push(value);
                field = Some(Field::Form(index));
            }
            other => return Err(at(format!("unknown keyword {other}"))),
        }
    }
    finish(&mut entry, &mut forms, &mut started, &mut entries);
    Ok(entries)
}

/// the text of a quoted PO string, with its escapes resolved
fn unquote(quoted: &str) -> Result<String, String> {
    let inner = quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("not a quoted string: {quoted}"))?;
    let mut text = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some('"') => text.push('"'),
            Some('\\') => text.push('\\'),
            other => return Err(format!("unknown escape \\{}", other.unwrap_or(' '))),
        }
    }
    Ok(text)
}

/// `text` as a quoted PO string
pub fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// the names in braces of `text`, `{name}` for a value put in by the program,
/// sorted
pub fn placeholders(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    names.push(name);
                }
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    names.sort_unstable();
    names
}
