//! The configuration examples in README.md must load without warnings, so
//! the documentation cannot drift from what hdir actually accepts.

use hdir::config::load_settings;
use hdir::servers;

/// The fenced block right after the `<!-- example: NAME -->` marker.
fn example(readme: &str, name: &str) -> String {
    let marker = format!("<!-- example: {name} -->");
    let rest = readme
        .split_once(&marker)
        .unwrap_or_else(|| panic!("README has no {marker}"))
        .1;
    let body = rest.split_once("```toml\n").expect("a toml block").1;
    body.split_once("```").expect("closed block").0.to_string()
}

#[test]
fn readme_examples_load_without_warnings() {
    // a Windows checkout may have CRLF line ends
    let readme = include_str!("../README.md").replace("\r\n", "\n");
    let readme = readme.as_str();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        example(readme, "config.toml"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("themes")).unwrap();
    std::fs::write(
        dir.path().join("themes/my-blue.toml"),
        example(readme, "themes/my-blue.toml"),
    )
    .unwrap();
    let s = load_settings(Some(dir.path()));
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    assert_eq!(s.classic.name, "my-blue", "the user theme is used");
    assert!(!s.config.assoc.is_empty() && !s.config.servers.is_empty());
    let (list, warnings) = servers::merge(&[], &s.config.servers);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(!list.is_empty());
}
