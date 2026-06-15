// Lane C2: quarantine git clone with symlink-escape defense.
//
// Unix-gated: the test commits a symlink whose target escapes the repo, which
// requires real symlink semantics. The quarantine logic itself is
// platform-agnostic; macOS + Linux CI exercise it.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use wcore_cli::plugin::quarantine::quarantine_clone;
use wcore_pluginsrc::SourceKind;

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn clone_skips_escaping_symlink_and_returns_sha() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    // Build a tiny git repo with a normal file and an escaping symlink.
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@wayland.test"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("keep.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", repo.join("escape")).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    let url = format!("file://{}", repo.display());
    let dest = tmp.path().join("quarantine");
    let cloned = quarantine_clone(
        &SourceKind::Url {
            url,
            git_ref: None,
            sha: None,
        },
        &dest,
    )
    .expect("quarantine clone");

    // The normal file survives the normalize-copy.
    assert!(
        cloned.path.join("keep.txt").is_file(),
        "keep.txt should be copied"
    );
    // The escaping symlink is NOT present in the output.
    assert!(
        !cloned.path.join("escape").exists(),
        "escaping symlink must be skipped"
    );
    // `.git` is dropped.
    assert!(
        !cloned.path.join(".git").exists(),
        ".git must not be copied"
    );
    // The resolved sha is a real, non-empty commit hash.
    assert!(!cloned.resolved_sha.is_empty(), "sha must be resolved");
    assert!(
        cloned.resolved_sha.chars().all(|c| c.is_ascii_hexdigit()),
        "sha must be hex: {}",
        cloned.resolved_sha
    );
}

#[test]
fn relative_path_source_is_not_cloneable() {
    let tmp = tempfile::tempdir().unwrap();
    let err =
        quarantine_clone(&SourceKind::RelativePath("x".into()), &tmp.path().join("q")).unwrap_err();
    assert!(
        matches!(err, wcore_cli::plugin::error::PluginCliError::Quarantine(_)),
        "relative-path is resolved in-repo, not cloned: {err:?}"
    );
}
