// Lane C1: marketplace.json parsing + source normalization.

use wcore_cli::plugin::error::PluginCliError;
use wcore_cli::plugin::marketplace::parse_marketplace;
use wcore_pluginsrc::SourceKind;

const CATALOG: &str = r#"{
  "name": "acme",
  "owner": { "name": "Acme", "email": "dev@acme.test" },
  "metadata": { "pluginRoot": "./plugins" },
  "plugins": [
    { "name": "rel", "source": "./rel-plugin" },
    { "name": "gh", "source": { "source": "github", "repo": "acme/gh-plugin", "ref": "main" } },
    { "name": "remote", "source": { "source": "url", "url": "https://git.acme.test/p.git", "sha": "deadbeef" } },
    { "name": "sub", "source": { "source": "git-subdir", "url": "https://git.acme.test/mono.git", "path": "tools/sub" } },
    { "name": "loose", "source": "./loose", "strict": false, "version": "1.2.0" }
  ]
}"#;

#[test]
fn parses_all_source_shapes_and_prepends_plugin_root() {
    let (meta, entries) = parse_marketplace(CATALOG).unwrap();

    assert_eq!(meta.name, "acme");
    assert_eq!(meta.owner_name.as_deref(), Some("Acme"));
    assert_eq!(meta.owner_email.as_deref(), Some("dev@acme.test"));
    assert_eq!(meta.plugin_root.as_deref(), Some("./plugins"));
    assert_eq!(entries.len(), 5);

    // Relative path: pluginRoot prepended, `./` stripped from the source.
    let rel = &entries[0];
    assert_eq!(rel.name, "rel");
    assert!(rel.strict, "strict defaults to true");
    match &rel.kind {
        SourceKind::RelativePath(p) => assert_eq!(p.to_str().unwrap(), "./plugins/rel-plugin"),
        other => panic!("expected RelativePath, got {other:?}"),
    }

    // github: sha/ref carried through.
    match &entries[1].kind {
        SourceKind::Github { repo, git_ref, sha } => {
            assert_eq!(repo, "acme/gh-plugin");
            assert_eq!(git_ref.as_deref(), Some("main"));
            assert!(sha.is_none());
        }
        other => panic!("expected Github, got {other:?}"),
    }

    // url: sha pins.
    match &entries[2].kind {
        SourceKind::Url { url, sha, .. } => {
            assert_eq!(url, "https://git.acme.test/p.git");
            assert_eq!(sha.as_deref(), Some("deadbeef"));
        }
        other => panic!("expected Url, got {other:?}"),
    }

    // git-subdir: path carried.
    match &entries[3].kind {
        SourceKind::GitSubdir { url, path, .. } => {
            assert_eq!(url, "https://git.acme.test/mono.git");
            assert_eq!(path, "tools/sub");
        }
        other => panic!("expected GitSubdir, got {other:?}"),
    }

    // strict=false + declared version honored.
    let loose = &entries[4];
    assert!(!loose.strict);
    assert_eq!(loose.declared_version.as_deref(), Some("1.2.0"));
}

#[test]
fn rejects_dotdot_in_relative_source() {
    let catalog = r#"{
      "name": "evil",
      "owner": { "name": "x" },
      "plugins": [ { "name": "esc", "source": "../../etc/passwd" } ]
    }"#;
    let err = parse_marketplace(catalog).unwrap_err();
    assert!(
        matches!(err, PluginCliError::PathTraversal(_)),
        "expected PathTraversal, got {err:?}"
    );
}

#[test]
fn rejects_absolute_relative_source() {
    // `Path::join` replaces its base on an absolute arg, so an absolute source
    // would escape the marketplace/clone root. Must be rejected at parse.
    let catalog = r#"{
      "name": "evil",
      "owner": { "name": "x" },
      "plugins": [ { "name": "esc", "source": "/etc" } ]
    }"#;
    let err = parse_marketplace(catalog).unwrap_err();
    assert!(
        matches!(err, PluginCliError::PathTraversal(_)),
        "expected PathTraversal for absolute source, got {err:?}"
    );
}

#[test]
fn rejects_absolute_git_subdir_path() {
    let catalog = r#"{
      "name": "evil",
      "owner": { "name": "x" },
      "plugins": [
        { "name": "esc", "source": { "source": "git-subdir", "url": "https://h/r.git", "path": "/etc" } }
      ]
    }"#;
    let err = parse_marketplace(catalog).unwrap_err();
    assert!(
        matches!(err, PluginCliError::PathTraversal(_)),
        "expected PathTraversal for absolute subdir, got {err:?}"
    );
}

#[test]
fn rejects_dotdot_in_git_subdir_path() {
    let catalog = r#"{
      "name": "evil",
      "owner": { "name": "x" },
      "plugins": [
        { "name": "esc", "source": { "source": "git-subdir", "url": "https://h/r.git", "path": "../secrets" } }
      ]
    }"#;
    let err = parse_marketplace(catalog).unwrap_err();
    assert!(
        matches!(err, PluginCliError::PathTraversal(_)),
        "expected PathTraversal, got {err:?}"
    );
}
