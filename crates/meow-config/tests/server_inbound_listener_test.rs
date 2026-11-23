//! C-01 / C-02 for the meow-vgate fork delta: parse the deployment's
//! `listeners:` entries without touching the config file.
//!
//! All UUIDs below are synthetic. No device credential or subscription data
//! is used or stored here.

#![cfg(feature = "server-inbound")]

use meow_config::{load_config_from_str, ListenerSpec};

const VLESS_YAML: &str = r#"
listeners:
  - name: vless-in
    type: vless
    listen: 0.0.0.0
    port: 19001
    users:
      - uuid: 00000000-0000-4000-8000-000000000001
        flow: xtls-rprx-vision
    ws-path: /free
    allow-insecure: true
  - name: vmess-in
    type: vmess
    listen: 0.0.0.0
    port: 19002
    users:
      - uuid: 00000000-0000-4000-8000-000000000002
        alterId: 0
    mkcp-config:
      enable: true
      mtu: 1350
      tti: 20
      uplink-capacity: 12
      downlink-capacity: 10
      congestion: bbr
      read-buffer: 2
      write-buffer: 2
"#;

/// C-01: vless/vmess listeners parse into the new specs with every
/// deployment field preserved.
#[tokio::test]
async fn c01_parses_vless_and_vmess_listeners() {
    let config = load_config_from_str(VLESS_YAML)
        .await
        .expect("deployment-shaped listeners must parse");

    let vless = config
        .listeners
        .named
        .iter()
        .find(|nl| nl.name == "vless-in")
        .expect("vless listener present");
    assert_eq!(vless.port, 19001);
    assert_eq!(vless.spec.type_name(), "vless");
    let ListenerSpec::Vless(vless_spec) = &vless.spec else {
        panic!("expected Vless listener spec");
    };
    assert_eq!(vless_spec.users.len(), 1);
    assert_eq!(
        vless_spec.users[0].uuid,
        "00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(
        vless_spec.users[0].flow.as_deref(),
        Some("xtls-rprx-vision")
    );
    assert_eq!(vless_spec.ws_path.as_deref(), Some("/free"));
    assert!(vless_spec.allow_insecure);

    let vmess = config
        .listeners
        .named
        .iter()
        .find(|nl| nl.name == "vmess-in")
        .expect("vmess listener present");
    assert_eq!(vmess.port, 19002);
    assert_eq!(vmess.spec.type_name(), "vmess");
    let ListenerSpec::Vmess(vmess_spec) = &vmess.spec else {
        panic!("expected Vmess listener spec");
    };
    assert_eq!(vmess_spec.users.len(), 1);
    assert_eq!(
        vmess_spec.users[0].uuid,
        "00000000-0000-4000-8000-000000000002"
    );
    assert_eq!(vmess_spec.users[0].alter_id, Some(0));

    let mkcp = vmess_spec
        .mkcp_config
        .as_ref()
        .expect("mkcp-config preserved");
    assert_eq!(mkcp.enable, Some(true));
    assert_eq!(mkcp.mtu, Some(1350));
    assert_eq!(mkcp.tti, Some(20));
    assert_eq!(mkcp.uplink_capacity, Some(12));
    assert_eq!(mkcp.downlink_capacity, Some(10));
    assert_eq!(mkcp.congestion.as_deref(), Some("bbr"));
    assert_eq!(mkcp.read_buffer, Some(2));
    assert_eq!(mkcp.write_buffer, Some(2));
}

/// C-01 extension: upstream defaults are not invented in this hook.
#[tokio::test]
async fn c01_optional_fields_default_without_inventing_values() {
    let yaml = r#"
listeners:
  - name: minimal-vless
    type: vless
    listen: 127.0.0.1
    port: 0
    users:
      - uuid: 00000000-0000-4000-8000-000000000003
"#;
    let config = load_config_from_str(yaml)
        .await
        .expect("minimal config parses");
    let ListenerSpec::Vless(spec) = &config.listeners.named[0].spec else {
        panic!("expected Vless listener spec");
    };
    assert_eq!(spec.ws_path, None);
    assert!(!spec.allow_insecure);
    assert_eq!(spec.users[0].flow, None);
    assert_eq!(spec.users[0].alter_id, None);
}

/// C-02: unknown listener types still hard-error (ADR-0002 Class A) and the
/// error names both the offending type and the supported set.
#[tokio::test]
async fn c02_unknown_listener_type_is_a_hard_error() {
    let yaml = r#"
listeners:
  - name: bogus
    type: trojan
    listen: 127.0.0.1
    port: 0
"#;
    // `Config` intentionally has no Debug impl, so `expect_err` cannot be
    // used here (its Ok type must be Debug). Match the Err explicitly.
    let error = match load_config_from_str(yaml).await {
        Ok(_) => panic!("unknown listener type must be rejected"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(
        message.contains("unknown listener type 'trojan'"),
        "{message}"
    );
    assert!(message.contains("vless"), "{message}");
    assert!(message.contains("vmess"), "{message}");
}

/// C-02 extension: a server inbound with no users is rejected rather than
/// starting an unauthenticated listener.
#[tokio::test]
async fn c02_empty_user_table_is_rejected() {
    let yaml = r#"
listeners:
  - name: empty-vless
    type: vless
    listen: 127.0.0.1
    port: 0
"#;
    let error = match load_config_from_str(yaml).await {
        Ok(_) => panic!("missing users must be rejected"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("listeners[empty-vless].users"));
}
