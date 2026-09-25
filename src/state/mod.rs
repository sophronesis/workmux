//! Filesystem-based state storage for workmux agents.
//!
//! This module provides persistent state storage that works across all
//! terminal multiplexer backends (tmux, WezTerm, Zellij).

pub mod run;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tracing::warn;

use crate::agent_identity::classify_agent_kind;
use crate::multiplexer::{AgentStatus, Multiplexer};

pub use store::StateStore;
pub use types::{AgentState, LastDoneCycleState, PaneKey, RuntimeState};

pub(crate) fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content).context("Failed to write temp file")?;
    fs::rename(&tmp, path).context("Failed to rename temp file")?;
    Ok(())
}

/// Persist an agent state update to the StateStore.
///
/// Merges with existing state so partial updates don't wipe other fields:
/// - If `status` is Some, updates the agent's status. If None, preserves existing.
/// - If `title_override` is Some, uses it. If None, preserves existing stored title,
///   falling back to the live pane title.
///
/// Logs warnings on failure without propagating errors (best-effort persistence).
pub fn persist_agent_update(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
) -> Option<TaskCompletion> {
    let pane_key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };

    let live_info = match mux.get_live_pane_info(pane_id) {
        Ok(Some(info)) => info,
        Ok(None) => {
            warn!(%pane_id, "pane not found, skipping state persist");
            return None;
        }
        Err(e) => {
            warn!(error = %e, "failed to get live pane info, skipping state persist");
            return None;
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Load existing state to merge with. A file whose pane_pid differs from
    // the live pane belongs to a dead agent whose pane id got recycled -
    // reconcile deletes it on the next daemon tick, but this hook may run
    // first, and inheriting its status_ts/title would clock the new task
    // from days ago.
    let existing = StateStore::new()
        .ok()
        .and_then(|store| store.get_agent(&pane_key).ok().flatten())
        .filter(|e| live_info.pid.is_none_or(|pid| e.pane_pid == pid));

    // Resolve status: explicit update wins, otherwise preserve existing
    let final_status = status.or(existing.as_ref().and_then(|e| e.status));

    // Preserve existing status_ts if status hasn't changed (avoids resetting timer)
    let status_ts = if final_status == existing.as_ref().and_then(|e| e.status) {
        existing.as_ref().and_then(|e| e.status_ts).unwrap_or(now)
    } else {
        now
    };

    // Task clock: carried across Working <-> Waiting, closed on Done. Only an
    // explicit status change can complete a task - title-only updates pass
    // `status: None` and must not be mistaken for a transition.
    let (working_since, completed_secs) = task_timing(
        existing.as_ref().and_then(|e| e.status),
        existing.as_ref().and_then(|e| e.working_since),
        existing.as_ref().and_then(|e| e.status_ts),
        status,
        now,
    );

    // Capture existing agent_kind before `existing` is consumed below.
    let existing_agent_kind = existing.as_ref().and_then(|e| e.agent_kind.clone());

    // Snapshot the live title for classification before the resolved
    // `pane_title` consumes `live_info.title`.
    let live_title_for_classify = live_info.title.clone();

    // Resolve title: explicit override wins, then existing stored title, then live
    let pane_title = title_override
        .or(existing.and_then(|e| e.pane_title))
        .or(live_info.title);

    // Get server boot ID for crash detection (best-effort)
    let boot_id = mux.server_boot_id().unwrap_or(None);

    // Classify the agent kind once and lock it in. The classifier sees the
    // *live* title (not the merged `pane_title` above, which prefers the
    // stored value): a stale stored title would otherwise re-confirm the
    // previous identity even after the foreground command has changed.
    // Pane reuse (Claude exits, another agent launches in the same pane) is
    // handled by reconcile in `state::store`, which deletes the stored
    // entry on `command` change before this path runs again.
    let agent_kind = merge_agent_kind(
        classify_agent_kind(
            live_info.current_command.as_deref(),
            live_title_for_classify.as_deref(),
        ),
        existing_agent_kind,
    );

    let state = AgentState {
        pane_key,
        workdir: live_info.working_dir,
        status: final_status,
        status_ts: Some(status_ts),
        pane_title,
        pane_pid: live_info.pid.unwrap_or(0),
        command: live_info.current_command.unwrap_or_default(),
        updated_ts: now,
        window_name: live_info.window,
        session_name: live_info.session,
        boot_id,
        agent_kind,
        // workmux only ever writes state for agents; terminal rows are
        // synthesized from live panes, or mirrored in from another machine
        terminal: None,
        working_since,
    };

    if let Ok(store) = StateStore::new()
        && let Err(e) = store.upsert_agent(&state)
    {
        warn!(error = %e, "failed to persist agent state");
    }

    // Names are read live at Done time, so a window/session renamed mid-task
    // and a title the agent rewrote (Claude Code `/rename`) show up in the
    // notification - same live-over-stored preference the sidebar applies.
    completed_secs.map(|duration_secs| TaskCompletion {
        duration_secs,
        session_name: state.session_name.clone(),
        window_name: state.window_name.clone(),
        pane_title: live_title_for_classify.or(state.pane_title.clone()),
        workdir: state.workdir.clone(),
    })
}

/// A task the agent just finished: reported by `persist_agent_update` exactly
/// once, on the status change *into* Done from Working or Waiting. A repeated
/// `done` (Stop hook firing twice, a stray PostToolUse) finds the stored
/// status already Done and produces nothing - this is what keeps the
/// completion notification to a single shot per task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskCompletion {
    /// Wall-clock seconds from the first Working of this task to Done.
    pub duration_secs: u64,
    pub session_name: Option<String>,
    pub window_name: Option<String>,
    /// Agent-set pane title, live at Done time (Claude Code writes its
    /// session summary here).
    pub pane_title: Option<String>,
    pub workdir: PathBuf,
}

/// Advance the task clock for one status update.
///
/// Returns the `working_since` to store and, when this update closes a task,
/// the task's duration. `prev_status_ts` is only a fallback start for state
/// files written before `working_since` existed.
fn task_timing(
    prev_status: Option<AgentStatus>,
    prev_since: Option<u64>,
    prev_status_ts: Option<u64>,
    update: Option<AgentStatus>,
    now: u64,
) -> (Option<u64>, Option<u64>) {
    let in_task = matches!(
        prev_status,
        Some(AgentStatus::Working) | Some(AgentStatus::Waiting)
    );
    // Start of the task in flight, if any: recorded start, else the timestamp
    // of the status that opened it (pre-field state files), else unknown.
    let started = prev_since.or(if in_task { prev_status_ts } else { None });

    match update {
        // title-only refresh: nothing moved
        None => (prev_since, None),
        Some(AgentStatus::Working) | Some(AgentStatus::Waiting) => {
            (Some(started.unwrap_or(now)), None)
        }
        Some(AgentStatus::Done) => {
            let completed = in_task.then(|| now.saturating_sub(started.unwrap_or(now)));
            (None, completed)
        }
    }
}

/// Merge a freshly classified agent kind with the previously cached one.
///
/// Locks in the first definitive answer: once `existing` is `Some(_)`, that
/// value is preserved. This guards against title drift (a non-agent process
/// printing a substring like "Vibe" or "◇" into the pane title and stealing
/// the cached identity). Pane reuse is handled separately by reconcile,
/// which removes the stored entry when `pane_current_command` changes.
fn merge_agent_kind(new: Option<String>, existing: Option<String>) -> Option<String> {
    existing.or(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentStatus::{Done, Waiting, Working};

    #[test]
    fn task_starts_on_first_working() {
        assert_eq!(
            task_timing(None, None, None, Some(Working), 100),
            (Some(100), None)
        );
        assert_eq!(
            task_timing(Some(Done), None, Some(50), Some(Working), 100),
            (Some(100), None)
        );
    }

    #[test]
    fn task_clock_survives_working_and_waiting_flips() {
        assert_eq!(
            task_timing(Some(Working), Some(100), Some(100), Some(Working), 130),
            (Some(100), None)
        );
        assert_eq!(
            task_timing(Some(Working), Some(100), Some(100), Some(Waiting), 160),
            (Some(100), None)
        );
        assert_eq!(
            task_timing(Some(Waiting), Some(100), Some(160), Some(Working), 190),
            (Some(100), None)
        );
    }

    #[test]
    fn done_closes_task_with_full_duration() {
        assert_eq!(
            task_timing(Some(Working), Some(100), Some(190), Some(Done), 400),
            (None, Some(300))
        );
        // waiting -> done (permission denied, agent stopped) is still a completion
        assert_eq!(
            task_timing(Some(Waiting), Some(100), Some(190), Some(Done), 400),
            (None, Some(300))
        );
    }

    #[test]
    fn repeated_done_is_not_a_completion() {
        assert_eq!(
            task_timing(Some(Done), None, Some(400), Some(Done), 410),
            (None, None)
        );
        assert_eq!(task_timing(None, None, None, Some(Done), 410), (None, None));
    }

    #[test]
    fn title_only_update_keeps_clock_untouched() {
        assert_eq!(
            task_timing(Some(Working), Some(100), Some(100), None, 150),
            (Some(100), None)
        );
        assert_eq!(
            task_timing(Some(Done), None, Some(400), None, 450),
            (None, None)
        );
    }

    #[test]
    fn legacy_state_without_working_since_falls_back_to_status_ts() {
        assert_eq!(
            task_timing(Some(Working), None, Some(100), Some(Done), 400),
            (None, Some(300))
        );
        assert_eq!(
            task_timing(Some(Working), None, Some(100), Some(Waiting), 200),
            (Some(100), None)
        );
    }

    #[test]
    fn merge_keeps_existing_when_new_is_none() {
        let merged = merge_agent_kind(None, Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_preserves_existing_against_drift() {
        // Existing was correctly classified; a later tick whose title drifted
        // into another agent's fingerprint must not overwrite it.
        let merged = merge_agent_kind(Some("vibe".into()), Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_returns_none_when_both_none() {
        assert_eq!(merge_agent_kind(None, None), None);
    }

    #[test]
    fn merge_classifies_when_existing_is_none() {
        let merged = merge_agent_kind(Some("claude".into()), None);
        assert_eq!(merged, Some("claude".into()));
    }
}
