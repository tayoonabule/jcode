//! Tracking of active session process IDs under `~/.jcode/active_pids`.
//!
//! This is pure filesystem state keyed by session ID, used to discover which
//! sessions are currently running (and to map a PID back to its session). It
//! lives in the storage crate because it only needs [`jcode_dir`] and is a
//! low-level concern shared by session management, dictation, and crash
//! recovery, none of which should pull the full `session` module into scope.

use crate::jcode_dir;
use std::path::PathBuf;

/// Directory holding one file per active session ID (`~/.jcode/active_pids`).
pub fn active_pids_dir() -> Option<PathBuf> {
    jcode_dir().ok().map(|d| d.join("active_pids"))
}

/// Directory holding per-session "currently streaming" markers. A marker file
/// exists only while a session is actively generating a model response. The
/// file content is the owning process PID so stale markers (from crashed
/// processes) can be detected and ignored.
pub fn streaming_pids_dir() -> Option<std::path::PathBuf> {
    jcode_dir().ok().map(|d| d.join("streaming_pids"))
}

/// Directory holding markers for internal sessions (debug/test sessions and
/// spawned children such as swarm workers). These sessions keep their normal
/// active-pid lifecycle markers, but presence UIs like the menu bar indicator
/// only want to show top-level sessions the user opened directly, so internal
/// ones are flagged here and filtered out of user-facing counts (issue #508).
pub fn internal_pids_dir() -> Option<std::path::PathBuf> {
    jcode_dir().ok().map(|d| d.join("internal_pids"))
}

/// Flag (or unflag) `session_id` as an internal session for presence UIs.
pub fn set_session_internal(session_id: &str, internal: bool) {
    let Some(dir) = internal_pids_dir() else {
        return;
    };
    if internal {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(session_id), "");
    } else {
        let _ = std::fs::remove_file(dir.join(session_id));
    }
}

/// Whether `session_id` is flagged as an internal session.
pub fn session_is_internal(session_id: &str) -> bool {
    internal_pids_dir().is_some_and(|dir| dir.join(session_id).exists())
}

/// Record that `session_id` is owned by process `pid`.
pub fn register_active_pid(session_id: &str, pid: u32) {
    if let Some(dir) = active_pids_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(session_id), pid.to_string());
    }
}

/// Remove the active-PID record for `session_id`, if present.
pub fn unregister_active_pid(session_id: &str) {
    if let Some(dir) = active_pids_dir() {
        let _ = std::fs::remove_file(dir.join(session_id));
    }
    // A closed session is never streaming, and its internal flag is moot.
    unmark_streaming(session_id);
    set_session_internal(session_id, false);
}

/// Mark a session as actively streaming a model response.
pub fn mark_streaming(session_id: &str) {
    if let Some(dir) = streaming_pids_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(session_id), std::process::id().to_string());
    }
}

/// Clear the streaming marker for a session (turn finished or interrupted).
pub fn unmark_streaming(session_id: &str) {
    if let Some(dir) = streaming_pids_dir() {
        let _ = std::fs::remove_file(dir.join(session_id));
    }
}

/// RAII guard that marks a session as streaming for its lifetime and clears the
/// marker on drop. This guarantees the marker is cleared on every exit path
/// (normal return, `?` propagation, interrupt, or panic) so the menu bar count
/// never gets stuck showing a phantom streaming session.
pub struct StreamingGuard {
    session_id: String,
}

impl StreamingGuard {
    pub fn new(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        mark_streaming(&session_id);
        Self { session_id }
    }
}

impl Drop for StreamingGuard {
    fn drop(&mut self) {
        unmark_streaming(&self.session_id);
    }
}

/// Find the active session ID currently owned by the given process ID.
pub fn find_active_session_id_by_pid(pid: u32) -> Option<String> {
    let dir = active_pids_dir()?;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let session_id = entry.file_name().to_string_lossy().to_string();
        let stored = std::fs::read_to_string(entry.path()).ok()?;
        if stored.trim().parse::<u32>().ok()? == pid {
            return Some(session_id);
        }
    }
    None
}

/// List active session IDs currently tracked in `~/.jcode/active_pids`.
pub fn active_session_ids() -> Vec<String> {
    let Some(dir) = active_pids_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect()
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_running(pid: u32) -> bool {
    // Best-effort fallback for platforms where this low-level storage crate does
    // not have a process API. The active PID file is still useful, and stale
    // entries are cleaned up by higher-level session lifecycle code.
    pid != 0
}

/// Live snapshot of how many jcode sessions are running, and how many of those
/// are actively streaming a model response right now. Used by the menu bar
/// indicator (`jcode menubar`) and any other presence UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCounts {
    /// Number of live sessions (registered PID is still running).
    pub total: usize,
    /// Number of live sessions currently streaming a model response.
    pub streaming: usize,
}

/// Live presence info for one running session, derived from the active-pid
/// registry and the streaming markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPresence {
    /// Session ID, e.g. `session_fox_1234567890_deadbeef`.
    pub session_id: String,
    /// PID of the process that owns the session.
    pub pid: u32,
    /// Whether the session is actively streaming a model response right now.
    pub streaming: bool,
    /// When the current streaming turn started (streaming marker mtime), if
    /// the session is streaming. Lets presence UIs show "working for 2m".
    pub streaming_since: Option<std::time::SystemTime>,
    /// Whether this is an internal session (debug/test or a spawned child
    /// such as a swarm worker) that user-facing presence UIs should hide.
    pub internal: bool,
}

/// Snapshot per-session presence by scanning the active-pid registry and
/// streaming markers, skipping any entries whose owning process is no longer
/// alive. This is a cheap O(n) scan over a handful of tiny files; used by the
/// menu bar indicator and other presence UI.
pub fn session_presence() -> Vec<SessionPresence> {
    let Some(active_dir) = active_pids_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&active_dir) else {
        return Vec::new();
    };

    let streaming_dir = streaming_pids_dir();
    let internal_dir = internal_pids_dir();
    let mut sessions = Vec::new();

    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let session_id = entry.file_name().to_string_lossy().to_string();
        let Some(pid) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
        else {
            continue;
        };
        if !process_is_running(pid) {
            continue;
        }

        let marker_path = streaming_dir.as_ref().map(|dir| dir.join(&session_id));
        let streaming = marker_path.as_ref().is_some_and(|marker| {
            std::fs::read_to_string(marker)
                .ok()
                .and_then(|raw| raw.trim().parse::<u32>().ok())
                .is_some_and(process_is_running)
        });
        let streaming_since = if streaming {
            marker_path
                .as_ref()
                .and_then(|marker| std::fs::metadata(marker).ok())
                .and_then(|meta| meta.modified().ok())
        } else {
            None
        };

        sessions.push(SessionPresence {
            internal: internal_dir
                .as_ref()
                .is_some_and(|dir| dir.join(&session_id).exists()),
            session_id,
            pid,
            streaming,
            streaming_since,
        });
    }

    sessions
}

/// Remove active-PID markers that can never correspond to a live session.
///
/// Two kinds of debris accumulate:
///
/// - markers whose recorded process is gone (a crashed or killed session), and
/// - markers for sessions that were registered but never persisted, which
///   happens when a short-lived child such as a swarm worker dies before its
///   first save. Those are only reaped once they are older than `min_age`, so a
///   session that is mid-startup is never pruned out from under itself.
///
/// `has_session_record` decides whether a session exists on disk; the storage
/// crate deliberately does not know how sessions are stored. Returns how many
/// markers were removed.
pub fn prune_orphan_active_pids(
    min_age: std::time::Duration,
    has_session_record: impl Fn(&str) -> bool,
) -> usize {
    let Some(dir) = active_pids_dir() else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };

    let now = std::time::SystemTime::now();
    let mut removed = 0usize;

    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let session_id = entry.file_name().to_string_lossy().to_string();
        let pid = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok());

        let orphan = match pid {
            // The owning process is gone, so this marker is debris whatever
            // else is true.
            Some(pid) if !process_is_running(pid) => true,
            // The process is alive (often the long-lived shared server), so
            // liveness proves nothing. Treat it as debris only when no session
            // was ever recorded and the marker has had time to settle.
            Some(_) | None => {
                let settled = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age >= min_age);
                settled && !has_session_record(&session_id)
            }
        };

        if orphan {
            let _ = std::fs::remove_file(&path);
            unmark_streaming(&session_id);
            set_session_internal(&session_id, false);
            removed += 1;
        }
    }

    // Streaming markers record the writing process, which for server-hosted
    // sessions is the long-lived server, so a marker left behind by a worker
    // that never cleared it keeps claiming a live PID. Drop any whose session
    // is no longer tracked at all.
    if let Some(streaming_dir) = streaming_pids_dir()
        && let Ok(streaming_entries) = std::fs::read_dir(&streaming_dir)
    {
        for entry in streaming_entries.filter_map(|entry| entry.ok()) {
            let session_id = entry.file_name().to_string_lossy().to_string();
            if !dir.join(&session_id).exists() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    removed
}

/// Compute the current session counts from [`session_presence`].
pub fn session_counts() -> SessionCounts {
    let sessions = session_presence();
    SessionCounts {
        total: sessions.len(),
        streaming: sessions.iter().filter(|s| s.streaming).count(),
    }
}

/// [`session_presence`] filtered to sessions the user opened directly,
/// excluding internal debug/test sessions and spawned children such as swarm
/// workers (issue #508). Used by the menu bar indicator.
pub fn user_session_presence() -> Vec<SessionPresence> {
    session_presence()
        .into_iter()
        .filter(|s| !s.internal)
        .collect()
}

/// Session counts over [`user_session_presence`] only.
pub fn user_session_counts() -> SessionCounts {
    let sessions = user_session_presence();
    SessionCounts {
        total: sessions.len(),
        streaming: sessions.iter().filter(|s| s.streaming).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize tests that mutate `JCODE_HOME`.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn session_counts_counts_live_and_streaming_only() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        let live = std::process::id();
        // Pick a PID that is almost certainly dead.
        let dead = 999_999u32;

        // live + streaming
        register_active_pid("session_alpha", live);
        mark_streaming("session_alpha");
        // live + not streaming
        register_active_pid("session_beta", live);
        // dead session (should be ignored entirely)
        register_active_pid("session_gamma", dead);
        // live session whose streaming marker points at a dead pid (ignored for streaming)
        register_active_pid("session_delta", live);
        if let Some(dir) = streaming_pids_dir() {
            let _ = std::fs::write(dir.join("session_delta"), dead.to_string());
        }

        let counts = session_counts();
        assert_eq!(counts.total, 3, "three live sessions expected");
        assert_eq!(
            counts.streaming, 1,
            "only one live streaming session expected"
        );

        // Per-session presence reports the same view, keyed by session.
        let sessions = session_presence();
        assert_eq!(sessions.len(), 3);
        let by_id = |id: &str| {
            sessions
                .iter()
                .find(|s| s.session_id == id)
                .unwrap_or_else(|| panic!("{id} should be present"))
        };
        assert!(by_id("session_alpha").streaming);
        assert!(!by_id("session_beta").streaming);
        assert!(!by_id("session_delta").streaming);
        assert_eq!(by_id("session_alpha").pid, live);
        assert!(!sessions.iter().any(|s| s.session_id == "session_gamma"));

        // Clearing the streaming marker drops the streaming count.
        unmark_streaming("session_alpha");
        assert_eq!(session_counts().streaming, 0);

        // Unregistering also clears any leftover streaming marker.
        register_active_pid("session_epsilon", live);
        mark_streaming("session_epsilon");
        assert_eq!(session_counts().streaming, 1);
        unregister_active_pid("session_epsilon");
        assert_eq!(session_counts().streaming, 0);

        jcode_core::env::remove_var("JCODE_HOME");
    }

    #[test]
    fn streaming_guard_marks_and_clears_on_drop() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        register_active_pid("session_guard", std::process::id());
        assert_eq!(session_counts().streaming, 0);
        {
            let _streaming = StreamingGuard::new("session_guard");
            assert_eq!(session_counts().streaming, 1);
        }
        assert_eq!(session_counts().streaming, 0);

        jcode_core::env::remove_var("JCODE_HOME");
    }

    /// Issue #508: internal (debug/child) sessions stay in the raw registry
    /// but are excluded from user-facing presence and counts.
    #[test]
    fn user_session_counts_exclude_internal_sessions() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        let live = std::process::id();
        register_active_pid("session_user", live);
        register_active_pid("session_worker", live);
        set_session_internal("session_worker", true);
        mark_streaming("session_worker");

        assert!(session_is_internal("session_worker"));
        assert!(!session_is_internal("session_user"));

        // Raw view keeps everything (lifecycle/crash detection needs it).
        assert_eq!(session_counts().total, 2);
        assert_eq!(session_counts().streaming, 1);

        // User-facing view hides the internal worker.
        let user_counts = user_session_counts();
        assert_eq!(user_counts.total, 1);
        assert_eq!(user_counts.streaming, 0);
        let user_sessions = user_session_presence();
        assert_eq!(user_sessions.len(), 1);
        assert_eq!(user_sessions[0].session_id, "session_user");

        // Unflagging restores visibility; unregistering clears the flag file.
        set_session_internal("session_worker", false);
        assert_eq!(user_session_counts().total, 2);
        set_session_internal("session_worker", true);
        unregister_active_pid("session_worker");
        assert!(!session_is_internal("session_worker"));

        jcode_core::env::remove_var("JCODE_HOME");
    }

    /// The failure behind the menu bar filling with unnamed entries: a child
    /// session registers an active PID, dies before its first save, and its
    /// marker names the long-lived shared server, so a liveness check alone
    /// never retires it.
    #[test]
    fn prunes_markers_for_sessions_that_were_never_persisted() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        let live = std::process::id();
        register_active_pid("session_ghost", live);
        register_active_pid("session_real", live);
        mark_streaming("session_ghost");
        set_session_internal("session_ghost", true);

        // Nothing is pruned while the markers are still within their grace
        // period, so a session that is mid-startup is never pulled out.
        let removed = prune_orphan_active_pids(std::time::Duration::from_secs(300), |id| {
            id == "session_real"
        });
        assert_eq!(removed, 0, "new markers must survive their grace period");
        assert_eq!(active_session_ids().len(), 2);

        // Once settled, the marker with no persisted session is debris.
        let removed =
            prune_orphan_active_pids(std::time::Duration::ZERO, |id| id == "session_real");
        assert_eq!(removed, 1, "only the never-persisted session is pruned");

        let remaining = active_session_ids();
        assert_eq!(remaining, vec!["session_real".to_string()]);
        // Its side markers go too, so no orphaned streaming/internal state.
        assert!(!session_is_internal("session_ghost"));
        assert!(
            streaming_pids_dir()
                .map(|dir| !dir.join("session_ghost").exists())
                .unwrap_or(true)
        );
    }

    #[test]
    fn prunes_markers_whose_process_is_gone_regardless_of_age() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        register_active_pid("session_crashed", 999_999);
        register_active_pid("session_live", std::process::id());

        // A dead process is debris even inside the grace period, and even if a
        // session record exists: the process can never come back.
        let removed = prune_orphan_active_pids(std::time::Duration::from_secs(300), |_| true);
        assert_eq!(removed, 1);
        assert_eq!(active_session_ids(), vec!["session_live".to_string()]);
    }

    /// A streaming marker left by a worker whose session is gone keeps claiming
    /// the long-lived server PID, so the menu bar would show phantom "streaming"
    /// work forever.
    #[test]
    fn prunes_streaming_markers_whose_session_is_no_longer_tracked() {
        let _guard = lock_env();
        let temp = tempfile::tempdir().expect("tempdir");
        jcode_core::env::set_var("JCODE_HOME", temp.path());

        register_active_pid("session_live", std::process::id());
        mark_streaming("session_live");
        // Marker with no active-pid entry at all: its session is long gone.
        mark_streaming("session_vanished");

        prune_orphan_active_pids(std::time::Duration::ZERO, |id| id == "session_live");

        let dir = streaming_pids_dir().expect("streaming dir");
        assert!(
            dir.join("session_live").exists(),
            "a tracked streaming session must keep its marker"
        );
        assert!(
            !dir.join("session_vanished").exists(),
            "an untracked streaming marker must be pruned"
        );
    }
}
