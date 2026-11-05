use teams_mcp::config::{endpoint_from_port_file, is_teams_page, validate_endpoint};

#[test]
fn only_literal_loopback_browser_websockets_are_accepted() {
    assert!(validate_endpoint("ws://127.0.0.1:9222/devtools/browser/test").is_ok());
    for value in [
        "ws://evil.test:9222/devtools/browser/a",
        "ws://localhost:9222/devtools/browser/a",
        "ws://127.0.0.1:9222/devtools/page/a",
        "ws://name:secret@127.0.0.1:9222/devtools/browser/a",
        "ws://127.0.0.1:9222/devtools/browser/a?secret=foo",
        "ws://0.0.0.0:9222/devtools/browser/a",
    ] {
        assert!(validate_endpoint(value).is_err(), "accepted {value}");
    }
}
#[test]
fn chrome_port_file_resolves_without_http_discovery() {
    assert_eq!(
        endpoint_from_port_file("9222\n/devtools/browser/abc\n").unwrap(),
        "ws://127.0.0.1:9222/devtools/browser/abc"
    );
    assert!(endpoint_from_port_file("0\n/devtools/browser/x").is_err());
    assert!(endpoint_from_port_file("9222\n//remote.test/x").is_err());
}
#[test]
fn teams_targets_require_exact_https_origin() {
    assert!(is_teams_page("https://teams.microsoft.com/v2/"));
    assert!(is_teams_page("https://teams.cloud.microsoft/"));
    for value in [
        "https://teams.microsoft.com.attacker.test",
        "http://teams.microsoft.com",
        "https://teams.microsoft.com:444",
        "https://evil.test/#https://teams.microsoft.com",
        "https://user@teams.microsoft.com",
    ] {
        assert!(!is_teams_page(value));
    }
}

#[test]
fn auth_source_is_explicit_and_unknown_modes_are_rejected() {
    use teams_mcp::config::AuthSource;
    assert!("headless".parse::<AuthSource>().is_ok());
    assert_eq!(AuthSource::default(), AuthSource::Browser);
    assert_eq!(
        "browser".parse::<AuthSource>().unwrap(),
        AuthSource::Browser
    );
    assert_eq!("app".parse::<AuthSource>().unwrap(), AuthSource::App);
    assert!("auto".parse::<AuthSource>().is_err());
}
