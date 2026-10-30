#[test]
fn app_metadata_is_exposed() {
    assert_eq!(hdir::APP_NAME, "hdir");
    assert_eq!(hdir::VERSION, env!("CARGO_PKG_VERSION"));
}
