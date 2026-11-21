//! The translations are complete and match the code.

use std::{collections::BTreeSet, fs, path::Path};

use rustinput_i18n::{LANGUAGES, extract, po};

const UPDATE: &str = "run `cargo run -p rustinput-i18n --example update-po`";

fn po_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/po"))
}

fn read(name: &str) -> Vec<po::Entry> {
    let path = po_dir().join(name);
    let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    po::parse(&source).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_template_has_the_texts_of_the_code() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let code: BTreeSet<String> = extract::messages(workspace)
        .iter()
        .map(|m| po::key(m.context.as_deref(), &m.id))
        .collect();
    let template: BTreeSet<String> = read("rustinput.pot").iter().map(po::Entry::key).collect();
    let new: Vec<_> = code.difference(&template).collect();
    let gone: Vec<_> = template.difference(&code).collect();
    assert!(
        new.is_empty(),
        "texts not in po/rustinput.pot yet, {UPDATE}: {new:#?}"
    );
    assert!(gone.is_empty(), "texts no longer used, {UPDATE}: {gone:#?}");
}

#[test]
fn every_language_translates_every_text() {
    let template: BTreeSet<String> = read("rustinput.pot").iter().map(po::Entry::key).collect();
    let mut problems = Vec::new();
    for language in &LANGUAGES[1..] {
        let file = format!("{}.po", language.file);
        let entries = read(&file);
        let keys: BTreeSet<String> = entries.iter().map(po::Entry::key).collect();
        for missing in template.difference(&keys) {
            problems.push(format!("{file}: missing {missing:?} ({UPDATE})"));
        }
        for extra in keys.difference(&template) {
            problems.push(format!("{file}: no longer used {extra:?} ({UPDATE})"));
        }
        for e in &entries {
            if e.fuzzy {
                problems.push(format!("{file}: marked fuzzy: {:?}", e.id));
                continue;
            }
            match &e.plural {
                Some(plural) => {
                    let forms = po::forms(&e.text);
                    if forms.len() != language.plural.forms()
                        || forms.iter().any(|f| f.trim().is_empty())
                    {
                        problems.push(format!(
                            "{file}: needs {} forms: {:?}",
                            language.plural.forms(),
                            e.id
                        ));
                    }
                    // a form may leave the number out ("one file"), but no
                    // other names
                    let names = po::placeholders(plural);
                    for form in forms {
                        if po::placeholders(form).iter().any(|n| !names.contains(n)) {
                            problems.push(format!("{file}: unknown name in braces: {form:?}"));
                        }
                    }
                }
                None if e.text.trim().is_empty() => {
                    problems.push(format!("{file}: not translated: {:?}", e.id));
                }
                None if po::placeholders(&e.id) != po::placeholders(&e.text) => {
                    problems.push(format!(
                        "{file}: the names in braces differ: {:?} -> {:?}",
                        e.id, e.text
                    ));
                }
                None => {}
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
