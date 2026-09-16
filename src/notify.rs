//! Desktop notifications.
//!
//! One shared sender (macOS notification center, freedesktop D-Bus elsewhere)
//! plus the policy for the "agent finished a task" notification that the
//! status-hook path fires.

use crate::agent_display::sanitize_pane_title;
use crate::config::Config;
use crate::state::TaskCompletion;

/// Tasks shorter than this stay silent unless `notify_done_min_secs` says
/// otherwise: a question answered in ten seconds needs no toast, the user is
/// still looking at the pane.
pub const DEFAULT_TASK_DONE_MIN_SECS: u64 = 60;

/// Notify the desktop that an agent finished a task, if configured and the
/// task ran long enough. The caller only ever holds a `TaskCompletion` for
/// the single status change into Done, so this fires at most once per task.
/// Returns whether a notification was sent.
pub fn task_done(config: &Config, done: &TaskCompletion) -> bool {
    if !should_notify_task_done(config, done.duration_secs) {
        return false;
    }
    let summary = format!(
        "{} done in {}",
        task_label(done),
        format_duration(done.duration_secs)
    );
    // same cleanup the sidebar applies: drop spinner glyphs, shell names and
    // the generic "Claude Code" title
    let body = sanitize_pane_title(done.pane_title.as_deref(), "", "").unwrap_or_default();
    show(&summary, body);
    true
}

/// "session:window", falling back to the workdir name when the multiplexer
/// gave us no window name.
fn task_label(done: &TaskCompletion) -> String {
    let non_empty = |name: &Option<String>| name.clone().filter(|n| !n.is_empty());
    let window = non_empty(&done.window_name).or_else(|| {
        done.workdir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    });
    match (non_empty(&done.session_name), window) {
        (Some(session), Some(window)) => format!("{}:{}", session, window),
        (None, Some(window)) => window,
        (Some(session), None) => session,
        (None, None) => "agent".to_string(),
    }
}

fn should_notify_task_done(config: &Config, duration_secs: u64) -> bool {
    config.notify_done.unwrap_or(true)
        && duration_secs
            >= config
                .notify_done_min_secs
                .unwrap_or(DEFAULT_TASK_DONE_MIN_SECS)
}

/// "45s", "3m 12s", "1h 04m".
pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

/// Show a system notification. Failures are logged, never surfaced: this runs
/// inside agent hooks and merges, where a missing notification daemon must
/// not turn into an error.
pub fn show(summary: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        use mac_notification_sys::{Notification, set_application};
        // Set application to Terminal to use its icon
        if let Err(e) = set_application("com.apple.Terminal") {
            tracing::debug!("Failed to set notification application: {:?}", e);
        }
        if let Err(e) = Notification::default().title(summary).message(body).send() {
            tracing::debug!("Failed to send notification: {:?}", e);
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Err(e) = notify_rust::Notification::new()
            .appname("workmux")
            .summary(summary)
            .body(body)
            .show()
        {
            tracing::debug!("Failed to send notification: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(notify_done: Option<bool>, min_secs: Option<u64>) -> Config {
        Config {
            notify_done,
            notify_done_min_secs: min_secs,
            ..Default::default()
        }
    }

    #[test]
    fn default_policy_is_on_with_one_minute_floor() {
        let cfg = config(None, None);
        assert!(!should_notify_task_done(&cfg, 59));
        assert!(should_notify_task_done(&cfg, 60));
    }

    #[test]
    fn disabled_never_notifies() {
        assert!(!should_notify_task_done(
            &config(Some(false), Some(0)),
            3600
        ));
    }

    #[test]
    fn custom_floor_is_respected() {
        let cfg = config(Some(true), Some(5));
        assert!(!should_notify_task_done(&cfg, 4));
        assert!(should_notify_task_done(&cfg, 5));
        assert!(should_notify_task_done(&config(None, Some(0)), 0));
    }

    fn completion(session: Option<&str>, window: Option<&str>) -> TaskCompletion {
        TaskCompletion {
            duration_secs: 90,
            session_name: session.map(str::to_string),
            window_name: window.map(str::to_string),
            pane_title: None,
            workdir: std::path::PathBuf::from("/repo/wt-feature"),
        }
    }

    #[test]
    fn label_is_session_and_window_with_fallbacks() {
        assert_eq!(
            task_label(&completion(Some("work"), Some("wm-feat"))),
            "work:wm-feat"
        );
        assert_eq!(task_label(&completion(None, Some("wm-feat"))), "wm-feat");
        assert_eq!(
            task_label(&completion(Some("work"), Some(""))),
            "work:wt-feature"
        );
        assert_eq!(task_label(&completion(None, None)), "wt-feature");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(192), "3m 12s");
        assert_eq!(format_duration(3840), "1h 04m");
    }
}
