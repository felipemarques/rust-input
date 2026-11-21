//! The resources of the GTK frontend: the icons and the style in one
//! bundle, and the windows (`.ui` files) in one bundle per language, with
//! the translations of `rustinput-i18n` put in. The frontend registers the
//! bundle of the windows in its language only.

use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use rustinput_i18n::{LANGUAGES, extract};

/// the windows, in `resources/`
const UI_FILES: &[&str] = &[
    "window.ui",
    "authorization_window.ui",
    "fingerprint_window.ui",
    "client_row.ui",
    "key_row.ui",
];

fn main() {
    glib_build_tools::compile_resources(
        &["resources"],
        "resources/resources.gresource.xml",
        "rustinput.gresource",
    );

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let mut table = String::from(
        "/// the windows in each language, by its code\n\
         static UI_RESOURCES: &[(&str, &[u8])] = &[\n",
    );
    for language in LANGUAGES {
        let dir = out.join(format!("ui-{}", language.code));
        fs::create_dir_all(&dir).expect("folder of the windows");
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gresources>\n  \
             <gresource prefix=\"/io/github/felipemarques/RustInput\">\n",
        );
        for file in UI_FILES {
            let path = Path::new("resources").join(file);
            println!("cargo:rerun-if-changed={}", path.display());
            let source = fs::read_to_string(&path).expect("a .ui file");
            let translated = if language.file.is_empty() {
                source
            } else {
                extract::translate_ui(&source, |context, id| {
                    rustinput_i18n::translate(*language, context, id).map(str::to_owned)
                })
            };
            let dest = dir.join(file);
            // unchanged files keep their time, so nothing builds again
            if fs::read_to_string(&dest).ok().as_deref() != Some(translated.as_str()) {
                fs::write(&dest, translated).expect("a translated .ui file");
            }
            writeln!(
                xml,
                "    <file compressed=\"true\" preprocess=\"xml-stripblanks\">{file}</file>"
            )
            .unwrap();
        }
        xml.push_str("  </gresource>\n</gresources>\n");
        let xml_path = dir.join("ui.gresource.xml");
        if fs::read_to_string(&xml_path).ok().as_deref() != Some(xml.as_str()) {
            fs::write(&xml_path, xml).expect("ui.gresource.xml");
        }
        let target = format!("ui-{}.gresource", language.code);
        glib_build_tools::compile_resources(
            &[&dir],
            xml_path.to_str().expect("a path in UTF-8"),
            &target,
        );
        writeln!(
            table,
            "    ({:?}, include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{target}\"))),",
            language.code
        )
        .unwrap();
    }
    table.push_str("];\n");
    fs::write(out.join("ui_resources.rs"), table).expect("ui_resources.rs");
}
