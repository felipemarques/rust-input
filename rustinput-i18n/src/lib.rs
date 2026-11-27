//! The languages of RustInput.
//!
//! The texts are written in English in the code: `tr("…")` in Rust and
//! `translatable="yes"` in the GTK `.ui` files. The other languages are
//! gettext PO files in `po/`, one per language, which the build script
//! compiles into the program: a single executable speaks all of them and
//! needs no files beside it.
//!
//! A value the program puts into a text is written as a name in braces,
//! `{name}`, and filled in with [`trf`]; the translations keep the same
//! names, in whatever order the language needs. A text with a number has
//! its forms for the numbers ([`trn`]): two in English, one in Chinese,
//! three in Russian, six in Arabic.
//!
//! `cargo run -p rustinput-i18n --example update-po` brings `po/` up to date
//! with the code: the template `po/rustinput.pot` and, in every language,
//! the new texts to translate. The tests fail while a text has no
//! translation in some language.

use std::{
    fmt::Display,
    sync::atomic::{AtomicUsize, Ordering},
};

pub mod extract;
pub mod po;

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

/// a language of RustInput
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Language {
    /// as in the configuration (`language`): a BCP 47 tag, "pt-BR"
    pub code: &'static str,
    /// its name, in itself
    pub name: &'static str,
    /// its file in `po/`, without `.po`; empty for English
    pub file: &'static str,
    /// written from right to left
    pub rtl: bool,
    /// how it picks the form of a text for a number
    pub plural: Plural,
}

/// How a language picks the form of a text for a number, as the
/// `Plural-Forms` of gettext.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plural {
    /// one form: Chinese, Japanese
    Invariable,
    /// for one, for the others: English, Spanish, Italian
    OneOther,
    /// for zero and one, for the others: Brazilian Portuguese, French
    UpToOneOther,
    /// for 1, 21, 31…; for 2–4, 22–24…; for the others: Russian
    Slavic,
    /// for 0; 1; 2; 3–10; 11–99; the others (100–102…): Arabic
    Arabic,
}

impl Plural {
    /// how many forms
    pub fn forms(self) -> usize {
        match self {
            Plural::Invariable => 1,
            Plural::OneOther | Plural::UpToOneOther => 2,
            Plural::Slavic => 3,
            Plural::Arabic => 6,
        }
    }

    /// the form for `n`
    pub fn index(self, n: u64) -> usize {
        match self {
            Plural::Invariable => 0,
            Plural::OneOther => usize::from(n != 1),
            Plural::UpToOneOther => usize::from(n > 1),
            Plural::Slavic => {
                if n % 10 == 1 && n % 100 != 11 {
                    0
                } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
                    1
                } else {
                    2
                }
            }
            Plural::Arabic => match (n, n % 100) {
                (0, _) => 0,
                (1, _) => 1,
                (2, _) => 2,
                (_, 3..=10) => 3,
                (_, 11..=99) => 4,
                _ => 5,
            },
        }
    }

    /// as the header of a PO file says it
    pub fn header(self) -> &'static str {
        match self {
            Plural::Invariable => "nplurals=1; plural=0;",
            Plural::OneOther => "nplurals=2; plural=(n != 1);",
            Plural::UpToOneOther => "nplurals=2; plural=(n > 1);",
            Plural::Slavic => {
                "nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && \
                 (n%100<10 || n%100>=20) ? 1 : 2);"
            }
            Plural::Arabic => {
                "nplurals=6; plural=(n==0 ? 0 : n==1 ? 1 : n==2 ? 2 : n%100>=3 && \
                 n%100<=10 ? 3 : n%100>=11 ? 4 : 5);"
            }
        }
    }
}

pub const ENGLISH: Language = Language {
    code: "en",
    name: "English",
    file: "",
    rtl: false,
    plural: Plural::OneOther,
};

/// all of them, English first
pub static LANGUAGES: &[Language] = &[
    ENGLISH,
    Language {
        code: "es",
        name: "Español",
        file: "es",
        rtl: false,
        plural: Plural::OneOther,
    },
    Language {
        code: "fr",
        name: "Français",
        file: "fr",
        rtl: false,
        plural: Plural::UpToOneOther,
    },
    Language {
        code: "it",
        name: "Italiano",
        file: "it",
        rtl: false,
        plural: Plural::OneOther,
    },
    Language {
        code: "pt-BR",
        name: "Português (Brasil)",
        file: "pt_BR",
        rtl: false,
        plural: Plural::UpToOneOther,
    },
    Language {
        code: "pt-PT",
        name: "Português (Portugal)",
        file: "pt_PT",
        rtl: false,
        plural: Plural::OneOther,
    },
    Language {
        code: "ru",
        name: "Русский",
        file: "ru",
        rtl: false,
        plural: Plural::Slavic,
    },
    Language {
        code: "ar",
        name: "العربية",
        file: "ar",
        rtl: true,
        plural: Plural::Arabic,
    },
    Language {
        code: "zh-CN",
        name: "中文（简体）",
        file: "zh_CN",
        rtl: false,
        plural: Plural::Invariable,
    },
    Language {
        code: "ja",
        name: "日本語",
        file: "ja",
        rtl: false,
        plural: Plural::Invariable,
    },
];

impl Language {
    /// The language of a tag as the configuration, the system or the
    /// environment write it: "pt-BR", "pt_BR.UTF-8", "pt" or "PT-br" are
    /// all Brazilian Portuguese; "pt-PT" and the Portuguese of the other
    /// countries (Angola, Mozambique…) the one of Portugal; any Chinese is
    /// simplified Chinese. `None` for a language RustInput does not speak.
    pub fn from_code(tag: &str) -> Option<Language> {
        let tag = tag
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .trim()
            .replace('_', "-");
        if tag.is_empty() || tag.eq_ignore_ascii_case("C") || tag.eq_ignore_ascii_case("POSIX") {
            return None;
        }
        if let Some(exact) = LANGUAGES.iter().find(|l| l.code.eq_ignore_ascii_case(&tag)) {
            return Some(*exact);
        }
        let primary = tag.split('-').next().unwrap_or_default();
        // only Brazil writes Brazilian Portuguese
        if primary.eq_ignore_ascii_case("pt")
            && tag.contains('-')
            && !tag.split('-').any(|part| part.eq_ignore_ascii_case("BR"))
        {
            return LANGUAGES.iter().find(|l| l.code == "pt-PT").copied();
        }
        LANGUAGES
            .iter()
            .find(|l| {
                l.code
                    .split('-')
                    .next()
                    .is_some_and(|p| p.eq_ignore_ascii_case(primary))
            })
            .copied()
    }

    /// the position in [`LANGUAGES`]
    pub fn index(self) -> usize {
        LANGUAGES.iter().position(|l| *l == self).unwrap_or(0)
    }
}

/// The environment variable with the languages of the user, separated by
/// `;`, that the Windows service gives RustInput when it starts it as SYSTEM.
pub const USER_LANGUAGES: &str = "RUSTINPUT_USER_LANGUAGES";

/// The language the system prefers, of those RustInput speaks; English
/// when it prefers none of them.
pub fn system() -> Language {
    preferred()
        .iter()
        .find_map(|tag| Language::from_code(tag))
        .unwrap_or(ENGLISH)
}

/// the languages the user prefers, the most preferred first
fn preferred() -> Vec<String> {
    let mut tags = Vec::new();
    // those of the user at the screen, from the Windows service, which
    // starts RustInput as SYSTEM: Windows would tell those of the system
    if let Ok(list) = std::env::var(USER_LANGUAGES) {
        tags.extend(list.split(';').filter(|t| !t.is_empty()).map(str::to_owned));
    }
    // the list of gettext, which sys-locale does not read
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Ok(list) = std::env::var("LANGUAGE") {
        tags.extend(list.split(':').filter(|t| !t.is_empty()).map(str::to_owned));
    }
    tags.extend(sys_locale::get_locales());
    tags
}

/// The language configured (`language`, "pt-BR"), or the one of the system
/// when there is none or RustInput does not speak it.
pub fn resolve(configured: Option<&str>) -> Language {
    configured
        .and_then(Language::from_code)
        .unwrap_or_else(system)
}

static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// the language of the texts from now on
pub fn set(language: Language) {
    CURRENT.store(language.index(), Ordering::Relaxed);
}

/// the language of the texts
pub fn current() -> Language {
    LANGUAGES[CURRENT.load(Ordering::Relaxed).min(LANGUAGES.len() - 1)]
}

/// the translations into `language`, by key ([`po::key`]), sorted
fn catalog(language: Language) -> &'static [(&'static str, &'static str)] {
    CATALOGS
        .iter()
        .find(|(file, _)| *file == language.file)
        .map(|(_, catalog)| *catalog)
        .unwrap_or(&[])
}

/// The translation of the English text `id` (with `context`, if it has
/// one) into `language`, `None` when there is none.
pub fn translate(language: Language, context: Option<&str>, id: &str) -> Option<&'static str> {
    let catalog = catalog(language);
    let key = po::key(context, id);
    catalog
        .binary_search_by(|(k, _)| (*k).cmp(key.as_str()))
        .ok()
        .map(|i| catalog[i].1)
}

/// `text` itself: marks it to be translated, for code that looks up its
/// translations with [`translate`] (in every language, say)
pub const fn mark(text: &'static str) -> &'static str {
    text
}

/// `text` in the current language
pub fn tr(text: &'static str) -> &'static str {
    translate(current(), None, text).unwrap_or(text)
}

/// `text` in the current language, in the sense `context` gives it (the
/// same English word for different things)
pub fn trc(context: &'static str, text: &'static str) -> &'static str {
    translate(current(), Some(context), text).unwrap_or(text)
}

/// `text` in the current language, with its `{name}`s filled in from
/// `values`
pub fn trf(text: &'static str, values: &[(&str, &dyn Display)]) -> String {
    fill(tr(text), values)
}

/// `one` (for one) or `other` (for other numbers) in the current
/// language, in the form for `n`, with `{count}` filled in with `n` and the
/// other `{name}`s from `values`
pub fn trn(
    one: &'static str,
    other: &'static str,
    n: u64,
    values: &[(&str, &dyn Display)],
) -> String {
    let language = current();
    let text = match translate(language, None, one) {
        Some(text) => {
            let forms = po::forms(text);
            forms[language.plural.index(n).min(forms.len() - 1)]
        }
        None if n == 1 => one,
        None => other,
    };
    let mut all: Vec<(&str, &dyn Display)> = vec![("count", &n)];
    all.extend_from_slice(values);
    fill(text, &all)
}

/// `text` with each `{name}` replaced by its value in `values`; names with
/// no value stay as they are
pub fn fill(text: &str, values: &[(&str, &dyn Display)]) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            values
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| (close, v.to_string()))
        });
        match value {
            Some((close, value)) => {
                out.push_str(&value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_of_the_configuration_and_of_the_system() {
        let code = |tag: &str| Language::from_code(tag).map(|l| l.code);
        assert_eq!(code("pt-BR"), Some("pt-BR"));
        assert_eq!(code("pt_BR.UTF-8"), Some("pt-BR"));
        assert_eq!(code("PT-br"), Some("pt-BR"));
        assert_eq!(code("pt_PT"), Some("pt-PT"));
        assert_eq!(code("pt_PT.UTF-8"), Some("pt-PT"));
        assert_eq!(code("pt-AO"), Some("pt-PT"));
        assert_eq!(code("pt"), Some("pt-BR"));
        assert_eq!(code("pt-Latn-BR"), Some("pt-BR"));
        assert_eq!(code("zh-Hans-CN"), Some("zh-CN"));
        assert_eq!(code("zh_TW"), Some("zh-CN"));
        assert_eq!(code("en-US"), Some("en"));
        assert_eq!(code("ar_EG.UTF-8"), Some("ar"));
        assert_eq!(code("ja"), Some("ja"));
        assert_eq!(code("de-DE"), None);
        assert_eq!(code("C"), None);
        assert_eq!(code("C.UTF-8"), None);
        assert_eq!(code(""), None);
    }

    #[test]
    fn the_configured_one_unless_unknown() {
        assert_eq!(resolve(Some("ru")).code, "ru");
        assert_eq!(resolve(Some("klingon")), system());
        assert_eq!(resolve(None), system());
    }

    #[test]
    fn values_go_where_their_names_are() {
        let name = "xeon";
        assert_eq!(
            fill("Arrangement sent to {name}", &[("name", &name)]),
            "Arrangement sent to xeon"
        );
        assert_eq!(fill("{b} {a} {a}", &[("a", &1), ("b", &"x")]), "x 1 1");
        assert_eq!(fill("{unknown} {", &[]), "{unknown} {");
    }

    #[test]
    fn forms_for_numbers() {
        let russian: Vec<usize> = [1, 2, 5, 11, 12, 21, 22, 25, 111]
            .iter()
            .map(|n| Plural::Slavic.index(*n))
            .collect();
        assert_eq!(russian, [0, 1, 2, 2, 2, 0, 1, 2, 2]);
        let arabic: Vec<usize> = [0, 1, 2, 3, 10, 11, 99, 100, 102, 103]
            .iter()
            .map(|n| Plural::Arabic.index(*n))
            .collect();
        assert_eq!(arabic, [0, 1, 2, 3, 3, 4, 4, 5, 5, 3]);
        assert_eq!(Plural::UpToOneOther.index(0), 0);
        assert_eq!(Plural::OneOther.index(0), 1);
        for language in LANGUAGES {
            assert!(language.plural.index(7) < language.plural.forms());
        }
        set(ENGLISH);
        assert_eq!(trn("{count} file", "{count} files", 1, &[]), "1 file");
        assert_eq!(
            trn(
                "{count} files from {name}",
                "{count} files from {name}",
                3,
                &[("name", &"pc")]
            ),
            "3 files from pc"
        );
    }

    #[test]
    fn english_is_the_text_itself() {
        set(ENGLISH);
        assert_eq!(tr("Apply"), "Apply");
        assert_eq!(translate(ENGLISH, None, "Apply"), None);
    }

    #[test]
    fn every_language_has_its_file() {
        for language in &LANGUAGES[1..] {
            assert!(
                CATALOGS.iter().any(|(file, _)| *file == language.file),
                "po/{}.po is missing",
                language.file
            );
        }
    }
}
