//! Every version tells what is new in it: the window shows CHANGELOG.md after
//! an update (0.48.0).

#[test]
fn the_changelog_tells_about_this_version() {
    // a checkout on Windows may have turned the lines into CRLF
    let changelog = include_str!("../CHANGELOG.md").replace("\r\n", "\n");
    let heading = format!("\n## {}\n", env!("CARGO_PKG_VERSION"));
    assert!(
        changelog.contains(&heading),
        "CHANGELOG.md has no section {heading:?}: write what is new in this version"
    );
}
