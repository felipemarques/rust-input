//! Brings `po/` up to date with the code: writes the template
//! `po/rustinput.pot` with every text to translate, and keeps in each
//! language the translations of the texts still used, adding the new ones
//! untranslated (`msgstr ""`) for the translator.
//!
//! `cargo run -p rustinput-i18n --example update-po`

use std::{collections::HashMap, fmt::Write as _, fs, path::Path};

use rustinput_i18n::{LANGUAGES, Language, extract, po};

fn header(language: Option<&Language>) -> String {
    let mut out = String::new();
    match language {
        Some(language) => writeln!(out, "# RustInput in {}.", language.name).unwrap(),
        None => out.push_str("# The texts of RustInput to translate.\n"),
    }
    out.push_str(
        "# This file is distributed under the same license as RustInput (GPL-3.0-or-later).\n\
         # Made by `cargo run -p rustinput-i18n --example update-po`: translate the msgstr\n\
         # lines, keep every {name} of the English text, and do not change the msgid lines.\n",
    );
    out.push_str("msgid \"\"\nmsgstr \"\"\n\"Project-Id-Version: RustInput\\n\"\n");
    if let Some(language) = language {
        writeln!(out, "\"Language: {}\\n\"", language.file).unwrap();
        writeln!(out, "\"Plural-Forms: {}\\n\"", language.plural.header()).unwrap();
    }
    out.push_str(
        "\"MIME-Version: 1.0\\n\"\n\
         \"Content-Type: text/plain; charset=UTF-8\\n\"\n\
         \"Content-Transfer-Encoding: 8bit\\n\"\n",
    );
    out
}

fn main() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = crate_dir.parent().expect("the workspace");
    let po_dir = crate_dir.join("po");
    let messages = extract::messages(workspace);

    let mut pot = header(None);
    for m in &messages {
        writeln!(pot, "\n#: {}", m.location).unwrap();
        if let Some(context) = &m.context {
            writeln!(pot, "msgctxt {}", po::quote(context)).unwrap();
        }
        writeln!(pot, "msgid {}", po::quote(&m.id)).unwrap();
        match &m.plural {
            Some(plural) => writeln!(
                pot,
                "msgid_plural {}\nmsgstr[0] \"\"\nmsgstr[1] \"\"",
                po::quote(plural)
            )
            .unwrap(),
            None => pot.push_str("msgstr \"\"\n"),
        }
    }
    fs::write(po_dir.join("rustinput.pot"), pot).expect("rustinput.pot written");
    println!("po/rustinput.pot: {} texts", messages.len());

    for language in &LANGUAGES[1..] {
        let path = po_dir.join(format!("{}.po", language.file));
        let old: HashMap<String, po::Entry> = fs::read_to_string(&path)
            .ok()
            .map(|s| po::parse(&s).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.key(), e))
            .collect();
        let mut out = header(Some(language));
        let mut missing = 0;
        for m in &messages {
            let key = po::key(m.context.as_deref(), &m.id);
            let old = old.get(&key);
            let text = old.map(|e| e.text.as_str()).unwrap_or_default();
            let forms = language.plural.forms();
            let complete = match &m.plural {
                Some(_) => po::forms(text).iter().filter(|f| !f.is_empty()).count() == forms,
                None => !text.is_empty(),
            };
            if !complete || old.is_some_and(|e| e.fuzzy) {
                missing += 1;
            }
            writeln!(out, "\n#: {}", m.location).unwrap();
            if old.is_some_and(|e| e.fuzzy) {
                out.push_str("#, fuzzy\n");
            }
            if let Some(context) = &m.context {
                writeln!(out, "msgctxt {}", po::quote(context)).unwrap();
            }
            writeln!(out, "msgid {}", po::quote(&m.id)).unwrap();
            match &m.plural {
                Some(plural) => {
                    writeln!(out, "msgid_plural {}", po::quote(plural)).unwrap();
                    let old_forms = if text.is_empty() {
                        Vec::new()
                    } else {
                        po::forms(text)
                    };
                    for i in 0..forms {
                        let form = old_forms.get(i).copied().unwrap_or_default();
                        writeln!(out, "msgstr[{i}] {}", po::quote(form)).unwrap();
                    }
                }
                None => writeln!(out, "msgstr {}", po::quote(text)).unwrap(),
            }
        }
        fs::write(&path, out).expect("PO file written");
        println!("po/{}.po: {missing} to translate", language.file);
    }
}
