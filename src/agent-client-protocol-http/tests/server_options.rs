#![cfg(feature = "server")]

use agent_client_protocol_http::{CorsOptions, ServerOptions};

#[test]
fn defaults_are_unchanged() {
    let options = ServerOptions::default();

    assert_eq!(options.path, "/acp");
    assert_eq!(options.cors, CorsOptions::disabled());
    assert!(options.health_endpoint);
}

#[test]
fn fluent_setters_replace_values_and_accept_owned_paths() {
    let cors = CorsOptions::allow_origins(["https://example.com"]).unwrap();
    let options = ServerOptions::default()
        .with_path("/initial")
        .with_path(String::from("/agent"))
        .with_cors(CorsOptions::allow_any_origin())
        .with_cors(cors.clone())
        .with_health_endpoint(true)
        .with_health_endpoint(false);

    assert_eq!(options.path, "/agent");
    assert_eq!(options.cors, cors);
    assert!(!options.health_endpoint);
}

#[test]
fn each_setter_preserves_other_defaults() {
    let path = ServerOptions::default().with_path("/agent");
    assert_eq!(path.cors, CorsOptions::disabled());
    assert!(path.health_endpoint);

    let cors = ServerOptions::default().with_cors(CorsOptions::allow_any_origin());
    assert_eq!(cors.path, "/acp");
    assert!(cors.health_endpoint);

    let health = ServerOptions::default().with_health_endpoint(false);
    assert_eq!(health.path, "/acp");
    assert_eq!(health.cors, CorsOptions::disabled());
}

#[test]
fn public_fields_remain_mutable() {
    let mut options = ServerOptions::default();
    options.path = "/agent".into();
    options.cors = CorsOptions::allow_any_origin();
    options.health_endpoint = false;

    assert_eq!(options.path, "/agent");
    assert_eq!(options.cors, CorsOptions::allow_any_origin());
    assert!(!options.health_endpoint);
}
