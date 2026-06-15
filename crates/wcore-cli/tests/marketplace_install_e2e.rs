// Lane C4: end-to-end marketplace install — plan (pure) then commit.

use std::path::Path;

use wcore_cli::plugin::known::{MarketplaceRef, add_marketplace};
use wcore_cli::plugin::lockfile::read_lock;
use wcore_cli::plugin::marketplace::{commit_install, resolve_and_plan};
use wcore_pluginsrc::CompatibilityGrade;

fn write(p: &Path, body: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A local marketplace dir holding one relative-path Claude Code plugin.
fn build_fixture(dir: &Path) {
    write(
        &dir.join(".claude-plugin/marketplace.json"),
        r#"{
          "name": "local",
          "owner": { "name": "Tester" },
          "plugins": [ { "name": "demo", "source": "./demo" } ]
        }"#,
    );
    write(
        &dir.join("demo/.claude-plugin/plugin.json"),
        r#"{"name":"demo","version":"0.1.0","description":"demo plugin"}"#,
    );
    write(
        &dir.join("demo/skills/hello/SKILL.md"),
        "---\nname: hello\ndescription: greets\n---\nSay hello.",
    );
}

#[test]
fn plan_is_pure_then_commit_writes_store_and_lockfile() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let quarantine = tmp.path().join("quarantine");
    let fixture = tmp.path().join("fixture");
    std::fs::create_dir_all(&store).unwrap();
    build_fixture(&fixture);

    // Register the marketplace by local path.
    add_marketplace(
        &store,
        MarketplaceRef {
            name: "local".into(),
            source: fixture.to_string_lossy().into_owned(),
            official: false,
        },
    )
    .unwrap();

    // Plan: pure. Returns the right adds + grade and writes NOTHING to the store.
    let planned = resolve_and_plan(&store, &quarantine, "local", "demo").unwrap();
    assert_eq!(planned.plan.plugin, "demo");
    assert_eq!(planned.plan.grade, CompatibilityGrade::ContentCompatible);
    assert!(
        planned
            .plan
            .adds
            .iter()
            .any(|a| a.kind == "skill" && a.name == "local/demo:hello"),
        "expected namespaced skill in plan adds, got {:?}",
        planned.plan.adds
    );
    let store_dir = store.join("demo@local");
    assert!(!store_dir.exists(), "planning must not write the store dir");
    assert!(
        read_lock(&store).unwrap().is_empty(),
        "planning writes no lock record"
    );

    // Commit: writes the self-contained native plugin dir + a lock record.
    let dir = commit_install(&store, &planned, "2026-06-15T00:00:00Z".into()).unwrap();
    assert_eq!(dir, store_dir);
    assert!(dir.join("plugin.toml").is_file(), "generated plugin.toml");
    assert!(
        dir.join("skills/hello/SKILL.md").is_file(),
        "skill copied into the store"
    );
    assert!(dir.join("provenance.json").is_file(), "provenance sidecar");

    let lock = read_lock(&store).unwrap();
    assert_eq!(lock.len(), 1);
    assert_eq!(lock[0].plugin, "demo");
    assert_eq!(lock[0].marketplace, "local");
    assert_eq!(lock[0].version, "0.1.0");
    assert_eq!(lock[0].installed_at, "2026-06-15T00:00:00Z");
}
