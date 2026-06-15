// Lane C3: known_marketplaces.json + installed.lock.json round-trips.

use wcore_cli::plugin::error::PluginCliError;
use wcore_cli::plugin::known::{
    MarketplaceRef, add_marketplace, get_marketplace, list_marketplaces, remove_marketplace,
};
use wcore_cli::plugin::lockfile::{InstallRecord, read_lock, record_install, remove_record};

#[test]
fn known_marketplaces_add_list_get_remove_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    add_marketplace(
        root,
        MarketplaceRef {
            name: "acme".into(),
            source: "acme/catalog".into(),
            official: false,
        },
    )
    .unwrap();
    add_marketplace(
        root,
        MarketplaceRef {
            name: "beta".into(),
            source: "https://git.test/beta.git".into(),
            official: false,
        },
    )
    .unwrap();

    let all = list_marketplaces(root).unwrap();
    assert_eq!(all.len(), 2);
    // BTreeMap ordering by name.
    assert_eq!(all[0].name, "acme");
    assert_eq!(all[1].name, "beta");

    let got = get_marketplace(root, "acme").unwrap().unwrap();
    assert_eq!(got.source, "acme/catalog");
    assert!(get_marketplace(root, "missing").unwrap().is_none());

    assert!(remove_marketplace(root, "acme").unwrap());
    assert!(!remove_marketplace(root, "acme").unwrap());
    assert_eq!(list_marketplaces(root).unwrap().len(), 1);
}

#[test]
fn reserved_marketplace_name_rejected_for_third_party() {
    let tmp = tempfile::tempdir().unwrap();
    let err = add_marketplace(
        tmp.path(),
        MarketplaceRef {
            name: "anthropic".into(),
            source: "evil/catalog".into(),
            official: false,
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, PluginCliError::ReservedName(_)),
        "expected ReservedName, got {err:?}"
    );

    // The official bundled entry may claim the reserved name.
    add_marketplace(
        tmp.path(),
        MarketplaceRef {
            name: "anthropic".into(),
            source: "anthropics/claude-code".into(),
            official: true,
        },
    )
    .unwrap();
}

#[test]
fn install_lockfile_upsert_and_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    record_install(
        root,
        InstallRecord {
            plugin: "p".into(),
            marketplace: "acme".into(),
            source: "github:acme/p".into(),
            resolved_sha: Some("abc123".into()),
            version: "1.0.0".into(),
            grade: "ContentCompatible".into(),
            installed_at: "2026-06-15T00:00:00Z".into(),
        },
    )
    .unwrap();

    // Re-install the same plugin@marketplace replaces, not duplicates.
    record_install(
        root,
        InstallRecord {
            plugin: "p".into(),
            marketplace: "acme".into(),
            source: "github:acme/p".into(),
            resolved_sha: Some("def456".into()),
            version: "1.1.0".into(),
            grade: "ContentCompatible".into(),
            installed_at: "2026-06-15T01:00:00Z".into(),
        },
    )
    .unwrap();

    let lock = read_lock(root).unwrap();
    assert_eq!(lock.len(), 1, "upsert, not duplicate");
    assert_eq!(lock[0].resolved_sha.as_deref(), Some("def456"));
    assert_eq!(lock[0].version, "1.1.0");

    assert!(remove_record(root, "p", "acme").unwrap());
    assert!(read_lock(root).unwrap().is_empty());
    assert!(!remove_record(root, "p", "acme").unwrap());
}
