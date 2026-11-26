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
    assert_eq!(
        mkcp.congestion,
        Some(meow_config::raw::MkcpCongestion::Label("bbr".to_string()))
    );
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
    // used here (its Ok type must be Debug). Destructure the Err explicitly.
    let Err(error) = load_config_from_str(yaml).await else {
        panic!("unknown listener type must be rejected")
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
    let Err(error) = load_config_from_str(yaml).await else {
        panic!("missing users must be rejected")
    };
    assert!(format!("{error:#}").contains("listeners[empty-vless].users"));
}

/// Device-shape regression: the deployment's real config.yaml writes
/// `users[].username` and `mkcp-config.congestion: false` (bool, mihomo
/// mkcp.go:11). Both differ from the fork's original synthetic fixture.
/// The config must load byte-for-byte unchanged for drop-in replacement.
const DEVICE_SHAPE_YAML: &str = r#"
listeners:
  - name: vless-19001
    type: vless
    listen: 0.0.0.0
    port: 19001
    users:
      - username: u1
        uuid: 00000000-0000-4000-8000-000000000001
        flow: ""
    ws-path: /free
    allow-insecure: true
  - name: vmess-19002
    type: vmess
    listen: 0.0.0.0
    port: 19002
    users:
      - username: u1
        uuid: 00000000-0000-4000-8000-000000000002
        alterId: 0
    mkcp-config:
      enable: true
      mtu: 1250
      tti: 20
      uplink-capacity: 20
      downlink-capacity: 20
      congestion: false
      read-buffer: 4
      write-buffer: 4
"#;

#[tokio::test]
async fn device_shape_config_loads_unchanged() {
    let config = load_config_from_str(DEVICE_SHAPE_YAML)
        .await
        .expect("device-shape config must load with zero edits");
    assert_eq!(config.listeners.named.len(), 2);

    let vmess = config
        .listeners
        .named
        .iter()
        .find_map(|l| match &l.spec {
            ListenerSpec::Vmess(c) => Some(c),
            _ => None,
        })
        .expect("vmess listener");
    let mkcp = vmess.mkcp_config.as_ref().expect("mkcp-config");
    assert_eq!(mkcp.mtu, Some(1250));
    assert_eq!(
        mkcp.congestion,
        Some(meow_config::raw::MkcpCongestion::Flag(false)),
        "mihomo writes congestion as bool false"
    );
}

/// P2 regression (2026-11-26 review): `tti` above 1000 would panic in
/// `KcpConfig::from_inbound`'s `1000/tti` window derivation — reject at
/// parse time (Class A) instead.
#[tokio::test]
async fn mkcp_tti_above_1000_is_a_hard_error() {
    let yaml = r#"
listeners:
  - name: vmess-bad-tti
    type: vmess
    listen: 127.0.0.1
    port: 0
    users:
      - uuid: 00000000-0000-4000-8000-000000000004
    mkcp-config:
      tti: 2000
"#;
    let Err(error) = load_config_from_str(yaml).await else {
        panic!("tti above 1000 must be rejected");
    };
    assert!(
        format!("{error:#}").contains("listeners[vmess-bad-tti].mkcp-config.tti"),
        "{error:#}"
    );
}

/// P2 regression (2026-11-26 review): `mtu` below the 18-byte mKCP segment
/// header leaves no room for payload and underflows `mss()`. Reject at
/// parse time (Class A); `0` keeps meaning "unset" (default 1350).
#[tokio::test]
async fn mkcp_mtu_below_segment_overhead_is_a_hard_error() {
    let yaml = r#"
listeners:
  - name: vmess-bad-mtu
    type: vmess
    listen: 127.0.0.1
    port: 0
    users:
      - uuid: 00000000-0000-4000-8000-000000000004
    mkcp-config:
      mtu: 10
"#;
    let Err(error) = load_config_from_str(yaml).await else {
        panic!("mtu below segment overhead must be rejected");
    };
    assert!(
        format!("{error:#}").contains("listeners[vmess-bad-mtu].mkcp-config.mtu"),
        "{error:#}"
    );
}

/// P2 regression (2026-11-26 review): an unsupported mihomo key must not be
/// silently dropped. `mkcp-config.seed` selects AES-128-GCM auth, which the
/// server-inbound stack does not implement — the config still loads (warned
/// loudly at parse time) so a harmless key never blocks a boots-required
/// listener, mirroring the shadowsocks unsupported-key policy.
#[tokio::test]
async fn mkcp_seed_loads_with_a_warning_instead_of_failing() {
    let yaml = r#"
listeners:
  - name: vmess-seeded
    type: vmess
    listen: 127.0.0.1
    port: 0
    users:
      - uuid: 00000000-0000-4000-8000-000000000005
    mkcp-config:
      seed: "not-supported-secret"
      mtu: 1350
"#;
    let config = load_config_from_str(yaml)
        .await
        .expect("seed is warned about, not fatal");
    let ListenerSpec::Vmess(spec) = &config.listeners.named[0].spec else {
        panic!("expected Vmess listener spec");
    };
    assert_eq!(
        spec.mkcp_config.as_ref().expect("mkcp-config").mtu,
        Some(1350),
        "the usable fields of the same block are preserved"
    );
}
