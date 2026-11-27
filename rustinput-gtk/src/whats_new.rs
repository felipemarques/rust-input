//! What is new after an update (0.48.0): the window shows the parts of
//! CHANGELOG.md of the versions after the one it showed last, once.

use std::cmp::Ordering;

use adw::prelude::*;
use rustinput_i18n::{tr, trf};
use rustinput_ipc::FrontendRequest;

use crate::window::Window;

/// what changed in each version, newest first
const CHANGELOG: &str = include_str!("../../CHANGELOG.md");

/// a change in the language of the windows, English when not translated
fn translated(change: &str) -> String {
    rustinput_i18n::translate(
        rustinput_i18n::current(),
        Some(rustinput_i18n::extract::CHANGELOG_CONTEXT),
        change,
    )
    .unwrap_or(change)
    .to_owned()
}

/// `text` as Pango markup: what is between backticks in monospace
fn markup(text: &str) -> String {
    text.split('`')
        .enumerate()
        .map(|(i, part)| {
            let escaped = gtk::glib::markup_escape_text(part).to_string();
            match i % 2 {
                1 => format!("<tt>{escaped}</tt>"),
                _ => escaped,
            }
        })
        .collect()
}

/// "0.44.0" as numbers
fn numbers(version: &str) -> Vec<u64> {
    version
        .split(['.', '-', '+'])
        .map_while(|part| part.parse().ok())
        .collect()
}

/// how two versions compare, by their numbers
pub(crate) fn compare(a: &str, b: &str) -> Ordering {
    numbers(a).cmp(&numbers(b))
}

/// The changes of the versions after `seen` up to `current` (only
/// `current`'s without `seen`), newest first, each version with its
/// changes in English; `None` when there is none.
pub(crate) fn since(
    changelog: &str,
    seen: Option<&str>,
    current: &str,
) -> Option<Vec<(String, Vec<String>)>> {
    let mut parts = Vec::new();
    for section in changelog.split("\n## ").skip(1) {
        let (version, body) = section.split_once('\n').unwrap_or((section, ""));
        let version = version.trim();
        let newer = match seen {
            Some(seen) => compare(version, seen) == Ordering::Greater,
            None => compare(version, current) == Ordering::Equal,
        };
        if newer && compare(version, current) != Ordering::Greater {
            let changes = rustinput_i18n::extract::from_changelog(body)
                .into_iter()
                .map(|(_, change)| change)
                .collect();
            parts.push((version.to_owned(), changes));
        }
    }
    (!parts.is_empty()).then_some(parts)
}

/// Shows what is new since `seen`, if anything, and tells the service it was.
pub(crate) fn show(main: &Window, seen: Option<String>) {
    let current = crate::APP_VERSION.get().copied().unwrap_or_default();
    if seen.as_deref() == Some(current) {
        return;
    }
    main.request(FrontendRequest::WhatsNewSeen(current.to_owned()));
    let Some(text) = since(CHANGELOG, seen.as_deref(), current) else {
        return;
    };
    // the versions in bold, each change translated
    let mut lines = Vec::new();
    for (version, changes) in text {
        lines.push(format!("<b>{}</b>", markup(&version)));
        for change in changes {
            lines.push(format!("• {}", markup(&translated(&change))));
        }
        lines.push(String::new());
    }
    let markup = lines.join("\n").trim_end().to_owned();
    let label = gtk::Label::builder()
        .label(markup)
        .use_markup(true)
        .wrap(true)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(18)
        .margin_end(18)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .child(&label)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(260)
        .max_content_height(480)
        .propagate_natural_height(true)
        .vexpand(true)
        .build();
    let ok = gtk::Button::builder()
        .label(tr("OK"))
        .halign(gtk::Align::End)
        .margin_end(18)
        .margin_bottom(18)
        .css_classes(["suggested-action"])
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let title = trf(
        "What is new in RustInput {version}",
        &[("version", &current)],
    );
    content.append(
        &adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new(&title, ""))
            .build(),
    );
    content.append(&scrolled);
    content.append(&ok);
    let window = adw::Window::builder()
        .title(&title)
        .default_width(520)
        .transient_for(main)
        .content(&content)
        .build();
    let weak = window.downgrade();
    ok.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    window.present();
    ok.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "# News\n\nintro\n\n## 0.48.0\n\n- d\n\n## 0.47.0\n\n- c\n\n## 0.45.0\n\n- b\n\n## 0.44.0\n\n- a\n";

    #[test]
    fn versions_compare_by_their_numbers() {
        assert_eq!(compare("0.44.0", "0.45.0"), Ordering::Less);
        assert_eq!(compare("0.100.0", "0.99.1"), Ordering::Greater);
        assert_eq!(compare("1.2.3", "1.2.3"), Ordering::Equal);
    }

    #[test]
    fn the_news_since_the_version_seen() {
        let news = since(LOG, Some("0.44.0"), "0.47.0").unwrap();
        let versions: Vec<_> = news.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(versions, ["0.47.0", "0.45.0"]);
        assert_eq!(news[0].1, ["c"]);
        assert_eq!(since(LOG, Some("0.48.0"), "0.48.0"), None);
        // without a version seen: only the current one
        assert_eq!(
            since(LOG, None, "0.47.0").unwrap(),
            [("0.47.0".to_owned(), vec!["c".to_owned()])]
        );
    }

    #[test]
    fn code_in_monospace() {
        assert_eq!(
            markup("run `a <b>` & go"),
            "run <tt>a &lt;b&gt;</tt> &amp; go"
        );
    }
}
