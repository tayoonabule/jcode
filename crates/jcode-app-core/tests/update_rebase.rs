//! Update behavior for source checkouts that carry local commits.
//!
//! A self-improving install accumulates commits that upstream does not have.
//! `git pull --ff-only` cannot fast-forward past them, so without this the
//! install is stranded on its current version forever. These tests use real
//! git repositories rather than mocks, because the behavior under test is
//! entirely git's.

use jcode_app_core::update::run_git_pull_ff_only;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to run: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).expect("write file");
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// An "upstream" bare-ish repo plus a local clone tracking it.
fn upstream_and_clone() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let upstream = temp.path().join("upstream");
    let local = temp.path().join("local");

    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "master"]);
    git(&upstream, &["config", "user.email", "up@example.com"]);
    git(&upstream, &["config", "user.name", "Up"]);
    // Allow pushing to the checked-out branch of this non-bare repo.
    git(&upstream, &["config", "receive.denyCurrentBranch", "ignore"]);
    commit(&upstream, "README.md", "v1\n", "initial");

    git(
        temp.path(),
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            local.to_str().unwrap(),
        ],
    );
    git(&local, &["config", "user.email", "me@example.com"]);
    git(&local, &["config", "user.name", "Me"]);

    (temp, upstream, local)
}

#[test]
fn plain_fast_forward_still_works() {
    let (_temp, upstream, local) = upstream_and_clone();
    commit(&upstream, "README.md", "v2\n", "upstream change");

    run_git_pull_ff_only(&local, true).expect("a clean fast-forward must succeed");
    assert_eq!(
        std::fs::read_to_string(local.join("README.md")).unwrap(),
        "v2\n"
    );
}

#[test]
fn local_commits_are_replayed_on_top_of_upstream() {
    // The self-improvement case: local work that can never be pushed upstream
    // must not block updates.
    let (_temp, upstream, local) = upstream_and_clone();
    commit(&local, "mine.txt", "my feature\n", "local: my feature");
    commit(&upstream, "README.md", "v2\n", "upstream change");

    run_git_pull_ff_only(&local, true)
        .expect("diverged local commits must be rebased, not rejected");

    // Upstream's change arrived...
    assert_eq!(
        std::fs::read_to_string(local.join("README.md")).unwrap(),
        "v2\n"
    );
    // ...and the local commit survived on top of it.
    assert_eq!(
        std::fs::read_to_string(local.join("mine.txt")).unwrap(),
        "my feature\n"
    );
    let subjects = git(&local, &["log", "--format=%s", "-3"]);
    assert!(
        subjects.starts_with("local: my feature"),
        "local commit should sit on top, got:\n{subjects}"
    );
}

#[test]
fn conflicting_local_commits_leave_the_checkout_untouched() {
    // When the same file diverges, a rebase would conflict. The update must
    // abort cleanly rather than leave a half-finished rebase behind.
    let (_temp, upstream, local) = upstream_and_clone();
    commit(&local, "README.md", "mine\n", "local: edit readme");
    commit(&upstream, "README.md", "theirs\n", "upstream: edit readme");

    let error = run_git_pull_ff_only(&local, true)
        .expect_err("conflicting histories cannot be reconciled automatically");
    assert!(
        format!("{error:#}").contains("git pull --rebase"),
        "the error should tell the user how to resolve it: {error:#}"
    );

    // No rebase left in progress, and local work intact.
    assert!(
        !local.join(".git/rebase-merge").exists() && !local.join(".git/rebase-apply").exists(),
        "a failed update must not leave a rebase in progress"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("README.md")).unwrap(),
        "mine\n",
        "local work must be preserved exactly"
    );
}

#[test]
fn a_dirty_worktree_is_never_rebased() {
    // Rebasing with uncommitted changes risks the user's work, so the update
    // is skipped instead.
    let (_temp, upstream, local) = upstream_and_clone();
    commit(&local, "mine.txt", "my feature\n", "local: my feature");
    commit(&upstream, "README.md", "v2\n", "upstream change");
    std::fs::write(local.join("scratch.txt"), "uncommitted\n").unwrap();

    let error = run_git_pull_ff_only(&local, true).expect_err("a dirty tree must not be rebased");
    assert!(
        format!("{error:#}").to_lowercase().contains("diverged"),
        "should report the original divergence: {error:#}"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("scratch.txt")).unwrap(),
        "uncommitted\n",
        "uncommitted work must be untouched"
    );
}

#[test]
fn a_fresh_clone_does_not_lose_local_commits() {
    // A freshly cloned checkout has almost no reflog, which is the input to
    // git's fork-point heuristic for deciding which local commits are already
    // upstream. This pins down that an update from a fresh clone keeps local
    // commits, so the rebase base can never become reflog-dependent.
    let (_temp, upstream, origin_local) = upstream_and_clone();
    commit(&origin_local, "feature.txt", "important\n", "local: important work");
    commit(&upstream, "README.md", "v2\n", "upstream change");

    // Re-clone so the reflog is minimal, exactly like a fresh install.
    let fresh = origin_local.parent().unwrap().join("fresh");
    git(
        origin_local.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin_local.to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    git(&fresh, &["config", "user.email", "me@example.com"]);
    git(&fresh, &["config", "user.name", "Me"]);
    // Point the fresh clone at the real upstream, so it is 1 behind / 1 ahead.
    git(&fresh, &["remote", "set-url", "origin", upstream.to_str().unwrap()]);
    git(&fresh, &["fetch", "-q", "origin"]);

    let ahead_before: usize = git(&fresh, &["rev-list", "--count", "@{upstream}..HEAD"])
        .parse()
        .unwrap();
    assert_eq!(ahead_before, 1, "fresh clone should carry the local commit");

    run_git_pull_ff_only(&fresh, true).expect("update should succeed");

    let ahead_after: usize = git(&fresh, &["rev-list", "--count", "@{upstream}..HEAD"])
        .parse()
        .unwrap();
    assert_eq!(
        ahead_after, ahead_before,
        "the local commit must survive the update, not be silently dropped"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("feature.txt")).unwrap(),
        "important\n",
        "local work must still be on disk"
    );
    assert_eq!(
        std::fs::read_to_string(fresh.join("README.md")).unwrap(),
        "v2\n",
        "upstream change must have arrived"
    );
}
