//! Authentication + TLS tests (Wave 65).

use std::sync::{Arc, RwLock};
use turbogp::server::auth::{PasswordManager, TlsConfig};
use turbogp::server::ServerConfig;

#[test]
fn password_manager_create_and_verify() {
    let mut mgr = PasswordManager::new();
    mgr.create_user("alice", "secret");
    assert!(mgr.exists("alice"), "user alice must exist");
    assert!(!mgr.exists("bob"), "user bob must not exist");
}

#[test]
fn password_manager_drop_user() {
    let mut mgr = PasswordManager::new();
    mgr.create_user("alice", "secret");
    assert!(mgr.exists("alice"));
    assert!(mgr.drop_user("alice"));
    assert!(!mgr.exists("alice"));
}

#[test]
fn server_config_with_auth() {
    let mut mgr = PasswordManager::new();
    mgr.create_user("bob", "password");
    let config = ServerConfig {
        auth_required: true,
        tls: None,
        passwords: Arc::new(RwLock::new(mgr)),
        ..Default::default()
    };
    assert!(config.auth_required);
    assert!(config.tls.is_none());
}

#[test]
fn tls_config_struct_exists() {
    let tls =
        TlsConfig { cert_path: "/path/to/cert.pem".into(), key_path: "/path/to/key.pem".into() };
    assert_eq!(tls.cert_path, "/path/to/cert.pem");
    assert_eq!(tls.key_path, "/path/to/key.pem");
}
