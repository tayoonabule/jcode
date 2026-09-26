---
name: jcode-update
description: Finish a jcode repository task by synchronizing with the original upstream repository, rebasing and integrating the completed branch into main, installing the resulting local build, reloading the shared daemon, and safely pruning obsolete local build artifacts.
allowed-tools: bash, read, write, agentgrep, todo, selfdev
---

# Jcode Update

Use this skill when the user asks to finish, ship, sync, install, or update the local jcode repository after a task. This is a repository-local release hygiene workflow for `/Users/light/Documents/GitHub/jcode` and compatible clones.

## Safety contract

- Start with `git status --short`, `git branch --show-current`, and `git worktree list`.
- Upstream is authoritative. Preserve a backup ref, then make the local fork fit
  the fetched `upstream/master`; do not stop and ask the user about ordinary
  conflicts.
- Never merge or copy commits from another agent's branch without explicit user authorization. If another worktree or agent is active, stop and report it.
- Do not merge a feature branch into `main` if the working tree is dirty, tests fail, the branch has unresolved conflicts, or the user did not authorize integration for this run.
- Treat `upstream` as the original jcode repository (`https://github.com/1jehuang/jcode.git`) and `origin` as the user's fork. Verify remotes before syncing.
- Do not push unless the user explicitly asks. Local integration and local installation are separate from publishing.
- Keep the fork history compact when requested: preserve the old tip in a backup
  ref, then create one local commit on top of `upstream/master` containing the
  surviving fork changes. Never force-push automatically.
- When publishing to a fork whose `origin/main` diverged after a rebase, do not
  stop at the failed fast-forward or force-push. Audit the origin-only commits,
  port any surviving changes, and incorporate origin's ancestry with a normal
  merge before a regular push. Preserve the tested integrated tree when the
  origin content has already been accounted for.
- Before any destructive build cleanup, print the candidates and preserve every path referenced by `current`, `stable`, `shared-server`, `canary`, launcher symlinks, or a live process.

## Standard workflow

1. **Capture state and identify the integration target**
   ```bash
   git status --short
   git branch --show-current
   git worktree list
   git remote -v
   ```
   Require a clean tree before rebasing or merging. If there are local changes, commit them when they belong to the task, or stop for user direction when they are unrelated.

2. **Fetch the original upstream**
   ```bash
   git fetch upstream --prune
   git fetch origin --prune
   ```
   Use `upstream/master` as the original project's base unless the repository's current integration policy explicitly names another ref. Do not assume `origin/main` and `upstream/master` are interchangeable.

3. **Integrate the completed task branch**
   On the completed task branch:
   ```bash
   git merge-base --is-ancestor upstream/master HEAD
   ```
   If that succeeds, upstream is already included and no replay is needed. If it
   fails, preserve the current tip and rebase. When a commit conflicts, upstream
   wins for the conflicting file or hunk, while non-conflicting local changes are
   retained. Continue automatically:
   ```bash
   backup="backup-before-update-$(date +%Y%m%d-%H%M%S)"
   git branch "$backup" HEAD
   git rebase upstream/master
   # For each conflict during rebase, keep upstream, stage, and continue:
   git checkout --ours -- <conflicted-paths>
   git add <conflicted-paths>
   GIT_EDITOR=true git rebase --continue
   ```
   Run the narrowest relevant tests and the repository's required checks. If a
   local feature no longer applies cleanly, leave the upstream version in place
   and record that the feature needs to be rethought, rather than blocking the
   update.

   For an explicitly requested history cleanup, first measure the local range:
   ```bash
   git rev-list --count upstream/master..main
   git rev-list --merges --count upstream/master..main
   ```
   Preserve the current tip, then build one squashed local commit on top of
   `upstream/master`, with upstream winning any conflicting file. Do not push
   automatically.

4. **Integrate into local `main`**
   After validation:
   ```bash
   git switch main
   git pull --ff-only origin main   # only if the user wants fork synchronization
   git merge --ff-only <completed-branch>
   ```
   Prefer fast-forward integration. If fast-forward is impossible, explain the divergence and ask before creating a merge commit. Never force-update `main`.

   When the user also requested a push to their fork, check whether
   `origin/main` is an ancestor of local `main`. If not, inspect every
   origin-only commit against the rebased tree and port any missing fork
   behavior with focused tests. A normal merge may auto-apply stale versions
   of many upstream files. When that risk is present and every origin-only
   change is either ported, already present, or intentionally superseded, record
   its ancestry with `git merge --no-ff -s ours origin/main`, preserving the
   audited tree. Verify the tree ID is unchanged by that ancestry merge and
   both upstream and origin are ancestors. Otherwise use a normal merge and
   resolve conflicts, then revalidate. Do not substitute `-s ours` for the
   content audit or use it on an unrelated branch. Re-fetch before publishing,
   then use only a regular `git push origin main`. If origin moved, repeat the
   audit instead of forcing a push.

5. **Install the integrated local build**
   For a fast local install from source:
   ```bash
   expected=$(git rev-parse --short HEAD)
   JCODE_RELEASE_PROFILE=release scripts/install_release.sh --fast
   ```
   This updates `~/.jcode/builds/versions`, `current`, `stable`, and the launcher. For normal
   self-development activation, use the supported `selfdev build` followed by
   `selfdev reload` instead of a release build.

6. **Reload and verify the shared daemon**
   ```bash
   jcode server promote "$expected" --json
   jcode server reload --force --json
   jcode --version
   readlink -f ~/.jcode/builds/current/jcode
   readlink -f ~/.jcode/builds/shared-server/jcode
   ```
   Do not treat the install script's reload message as proof of activation. The
   shared-server channel may be pinned to an older self-dev binary, and a reload
   can be a successful no-op when no listener is found. Verify that `current`,
   `stable`, `shared-server`, the launcher, and the live daemon executable all
   resolve to `expected`. On macOS, if the daemon was launched with a different
   runtime directory, discover its actual socket from the live process and pass
   it explicitly with `--socket`. For source builds, `selfdev status` must report
   the same hash for `Current` and `Shared server`, with reload state `SocketReady`.

7. **Safely clean old builds**
   First inspect:
   ```bash
   ls -la ~/.jcode/builds/versions
   readlink -f ~/.jcode/builds/current/jcode
   readlink -f ~/.jcode/builds/stable/jcode 2>/dev/null || true
   readlink -f ~/.jcode/builds/shared-server/jcode 2>/dev/null || true
   readlink -f ~/.jcode/builds/canary/jcode 2>/dev/null || true
   ```
   Only remove version directories that are not referenced by any channel or live process, and retain at least the two newest successfully installed versions as rollback protection. Never use broad `rm -rf ~/.jcode/builds`; delete only explicitly listed obsolete version directories. If cleanup would remove a referenced or ambiguous path, leave it and report it.

## Completion report

Report:

- upstream ref fetched and rebase result
- branch integrated into `main` and exact commit
- checks run and results
- installed binary path and version
- shared daemon reload result
- build cleanup candidates removed or intentionally retained
- any push or conflict work left for the user

If any step is blocked, stop at that step and preserve the repository and installed rollback path.
