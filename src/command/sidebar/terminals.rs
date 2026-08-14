//! Plain shell panes as sidebar rows.
//!
//! Agents reach the sidebar through state files written by their status hooks.
//! A terminal has no hooks and no state, so it is synthesized from the live
//! pane list on every tick and pushed into the same `Vec<AgentPane>` the agents
//! live in. That is the whole trick: sorting, filtering, templating and jumping
//! then work on terminals for free, because nothing downstream knows the
//! difference beyond the icon and the label.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::multiplexer::{AgentPane, LivePaneInfo, TerminalLabel};

/// Panes running workmux itself - the sidebar, a `workmux add` - are UI, not
/// terminals anyone wants a row for.
const SELF_COMMAND: &str = "workmux";

/// Foreground commands that mean "sitting at a prompt" rather than "busy".
/// Same set `sanitize_pane_title` treats as a noise title, for the same reason.
const SHELLS: &[&str] = &["zsh", "bash", "sh", "fish"];

/// Long-running TUIs, and the glyph each gets in place of the `>_` marker.
///
/// Membership carries meaning beyond the icon: these programs *sit there*
/// rather than grinding towards an exit, so a spinner would be a lie. They
/// render idle instead. Anything not listed keeps the spinner while it runs,
/// which is what you want for a build or a test run.
///
/// Every codepoint here was checked against the installed Nerd Font before
/// being added - a wrong one is an invisible tofu box, not a compile error.
/// Batch-ish commands (python, node, cargo) are deliberately absent: they are
/// far more often a job in progress than a session you are sitting in.
pub const TUI_ICONS: &[(&str, &str)] = &[
    ("nvim", "\u{e62b}"),   //  matches this user's waybar window-rewrite
    ("vim", "\u{e62b}"),
    ("vi", "\u{e62b}"),
    ("emacs", "\u{e632}"),  //
    ("htop", "\u{f0ae}"),   //  likewise from waybar, for the *top family
    ("btop", "\u{f0ae}"),
    ("btm", "\u{f0ae}"),
    ("top", "\u{f0ae}"),
    ("atop", "\u{f0ae}"),
    ("nvtop", "\u{f0ae}"),
    ("glances", "\u{f0ae}"),
    ("lazygit", "\u{e702}"), //
    ("gitui", "\u{e702}"),
    ("tig", "\u{e702}"),
    ("less", "\u{f02d}"),   //
    ("man", "\u{f02d}"),
    ("ranger", "\u{f07b}"), //
    ("yazi", "\u{f07b}"),
    ("lf", "\u{f07b}"),
    ("nnn", "\u{f07b}"),
    ("mc", "\u{f07b}"),
    ("ncdu", "\u{f0a0}"),   //
    ("neomutt", "\u{f0e0}"), //
    ("mutt", "\u{f0e0}"),
    ("aerc", "\u{f0e0}"),
    ("weechat", "\u{f086}"), //
    ("irssi", "\u{f086}"),
    ("k9s", "\u{f308}"),    //
    ("lazydocker", "\u{f308}"),
];

/// Icon for a foreground command: user overrides first, then the built-in
/// table. An override set to an empty string removes the program.
pub fn tui_icon(command: &str, overrides: &BTreeMap<String, String>) -> Option<String> {
    if let Some(icon) = overrides.get(command) {
        return (!icon.is_empty()).then(|| icon.clone());
    }
    TUI_ICONS
        .iter()
        .find(|(name, _)| *name == command)
        .map(|(_, icon)| (*icon).to_string())
}

/// How long a resolved branch is trusted before re-reading `.git/HEAD`.
const BRANCH_TTL: Duration = Duration::from_secs(10);

/// "Last interaction" per terminal: the moment its foreground command last
/// changed.
///
/// Deliberately *not* focus. Merely looking at a pane must not float it to the
/// top of the sidebar - that churns the order every time you glance somewhere.
/// Starting `nvim`, or dropping back to the prompt when it exits, is a real
/// event; sitting at a prompt is not.
///
/// tmux offers nothing to lean on here either. `#{window_activity}` is the
/// closest thing and it is useless: the sidebar repaints every couple of
/// seconds, which keeps *every* window permanently active - measured on a
/// three-window session, all of them report the same second, forever.
///
/// The first pass after a daemon start seeds commands without stamping them,
/// so a restart leaves terminals unstamped and out of the way rather than
/// slamming the whole set to the top. Panes appearing after that are genuinely
/// new, and do get stamped.
#[derive(Debug, Default)]
pub struct ActivityTracker {
    /// pane id -> (foreground command, when it last changed)
    seen: HashMap<String, (String, Option<u64>)>,
    primed: bool,
}

impl ActivityTracker {
    /// Rebuild from the live pane set, carrying stamps for unchanged commands.
    pub fn observe(&mut self, live: &HashMap<String, LivePaneInfo>, now_ts: u64) {
        let mut next = HashMap::with_capacity(live.len());
        for (pane_id, info) in live {
            let command = info.current_command.clone().unwrap_or_default();
            let ts = match self.seen.get(pane_id) {
                Some((previous, ts)) if *previous == command => *ts,
                Some(_) => Some(now_ts),             // ran something else
                None if self.primed => Some(now_ts), // pane just opened
                None => None,                        // first pass: age unknown
            };
            next.insert(pane_id.clone(), (command, ts));
        }
        self.seen = next; // rebuilt from live, so dead panes drop out
        self.primed = true;
    }

    pub fn ts(&self, pane_id: &str) -> Option<u64> {
        self.seen.get(pane_id).and_then(|(_, ts)| *ts)
    }
}

/// Branch per working directory, cached because it is looked up for every
/// terminal on every tick.
///
/// Reads `.git/HEAD` rather than shelling out: this sits on the render path,
/// and a `git` fork per pane per tick is not worth one segment of a label.
#[derive(Debug, Default)]
pub struct BranchCache {
    entries: HashMap<PathBuf, (Instant, Option<String>)>,
}

impl BranchCache {
    pub fn get(&mut self, dir: &Path) -> Option<String> {
        let now = Instant::now();
        if let Some((at, branch)) = self.entries.get(dir)
            && now.duration_since(*at) < BRANCH_TTL
        {
            return branch.clone();
        }
        let branch = read_branch(dir);
        self.entries
            .insert(dir.to_path_buf(), (now, branch.clone()));
        branch
    }

    pub fn retain_live(&mut self, live: &HashSet<PathBuf>) {
        self.entries.retain(|dir, _| live.contains(dir));
    }
}

/// Walk up from `dir` for a `.git`, then read the branch out of its HEAD.
///
/// `.git` is a directory in a normal clone and a file pointing at the real
/// gitdir inside a worktree - and worktrees are workmux's whole business, so
/// both paths matter. A detached HEAD holds a raw sha instead of a ref and
/// yields `None`, which drops the segment from the label.
fn read_branch(dir: &Path) -> Option<String> {
    let mut cursor = Some(dir);
    while let Some(current) = cursor {
        let dot_git = current.join(".git");
        let head = if dot_git.is_dir() {
            dot_git.join("HEAD")
        } else if dot_git.is_file() {
            let pointer = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = PathBuf::from(pointer.strip_prefix("gitdir:")?.trim());
            if gitdir.is_absolute() {
                gitdir.join("HEAD")
            } else {
                current.join(gitdir).join("HEAD")
            }
        } else {
            cursor = current.parent();
            continue;
        };
        let head = std::fs::read_to_string(head).ok()?;
        return head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_string);
    }
    None
}

/// Argv of the process currently holding the pane's terminal, read straight
/// out of /proc so this costs no fork on the render path.
///
/// `pane_pid` is the pane's *shell*; its tty's foreground process group is
/// what the user is actually running. Returns `None` wherever /proc is not a
/// Linux procfs, and callers treat that as "cannot tell".
fn foreground_argv(pane_pid: u32) -> Option<Vec<String>> {
    // "<pid> (<comm>) <state> <ppid> <pgrp> <session> <tty_nr> <tpgid> ..."
    // comm can itself contain "), " so split on the last one.
    let stat = std::fs::read_to_string(format!("/proc/{pane_pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(") ")?.1;
    let tpgid: u32 = after_comm.split_whitespace().nth(5)?.parse().ok()?;

    let raw = std::fs::read(format!("/proc/{tpgid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect(),
    )
}

/// Whether `argv` is an ssh/autossh client connected to `host`.
///
/// Same shape of match as `remote_pane_jump`: the argv head is the client and
/// the host appears as an argument of its own. Comparing whole argv elements
/// (rather than splitting a joined command line on spaces) keeps the trailing
/// remote command - `ssh host 'tmux attach'` - from matching by accident.
fn is_ssh_client_for(argv: &[String], host: &str) -> bool {
    let head_is_client = argv
        .first()
        .and_then(|head| head.rsplit('/').next())
        .is_some_and(|head| head == "ssh" || head == "autossh");
    head_is_client && argv.iter().skip(1).any(|arg| arg == host)
}

/// Shorten a directory the way this user's zsh prompt does (the ohmyzsh
/// `shrink-path -f` plugin): `$HOME` collapses to `~`, every component but the
/// last shrinks to its first character, the last stays whole.
///
/// Verified against that plugin's actual output:
/// ```text
/// /home/sph                                          -> ~
/// /home/sph/projects/work                            -> ~/p/work
/// /home/sph/.config/nixos                            -> ~/./nixos
/// /home/sph/projects/work/20260806_eyesatop/shawarma -> ~/p/w/2/shawarma
/// /etc/nixos                                         -> /e/nixos
/// /tmp                                               -> /tmp
/// ```
/// Note `.config` shrinking to a bare `.` rather than `.c` - one character is
/// one character, and matching the prompt matters more than looking clever.
pub fn shrink_dir(dir: &Path, home: Option<&Path>) -> String {
    let under_home = home.and_then(|home| dir.strip_prefix(home).ok());
    let rest = match under_home {
        Some(rest) => rest,
        None => dir.strip_prefix("/").unwrap_or(dir),
    };

    let mut parts: Vec<String> = rest
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.len().saturating_sub(1);
    for part in parts.iter_mut().take(last) {
        *part = part.chars().next().map(String::from).unwrap_or_default();
    }
    let joined = parts.join("/");

    if under_home.is_some() {
        if joined.is_empty() {
            "~".to_string()
        } else {
            format!("~/{joined}")
        }
    } else if dir.is_absolute() {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Directory, branch and foreground command, kept apart for the renderer.
///
/// The command is whatever holds the foreground: `nvim`, `ssh`, or the shell
/// itself (`zsh`) when the pane is sitting at a prompt - which is exactly the
/// distinction the name exists to show.
pub fn label(
    dir: &Path,
    command: &str,
    branch: Option<&str>,
    icons: &BTreeMap<String, String>,
) -> TerminalLabel {
    TerminalLabel {
        dir: shrink_dir(dir, home::home_dir().as_deref()),
        branch: branch.map(str::to_string),
        command: command.to_string(),
        icon: tui_icon(command, icons),
    }
}

/// A row for every live pane that is not already an agent.
pub fn synthesize(
    live_panes: &HashMap<String, LivePaneInfo>,
    agent_pane_ids: &HashSet<String>,
    mirrored_hosts: &HashSet<String>,
    activity: &ActivityTracker,
    branches: &mut BranchCache,
    icons: &BTreeMap<String, String>,
) -> Vec<AgentPane> {
    let mut rows = Vec::new();
    for (pane_id, info) in live_panes {
        if agent_pane_ids.contains(pane_id) {
            continue;
        }
        let Some(command) = info.current_command.as_deref().filter(|c| !c.is_empty()) else {
            continue;
        };
        if command == SELF_COMMAND {
            continue;
        }
        // An ssh pane whose host already has rows on screen is a duplicate
        // view, not a workload: the remote panes it hosts are listed
        // individually, and jumping to one of them routes through this pane
        // anyway. Keep it only while the host is otherwise invisible - the
        // mirror being down, say - so it never becomes unreachable.
        if matches!(command, "ssh" | "autossh")
            && !mirrored_hosts.is_empty()
            && info.pid.and_then(foreground_argv).is_some_and(|argv| {
                mirrored_hosts
                    .iter()
                    .any(|host| is_ssh_client_for(&argv, host))
            })
        {
            continue;
        }
        let branch = branches.get(&info.working_dir);
        let stamp = activity.ts(pane_id);
        let label = label(&info.working_dir, command, branch.as_deref(), icons);
        rows.push(AgentPane {
            session: info.session.clone().unwrap_or_default(),
            window_name: info.window.clone().unwrap_or_default(),
            pane_id: pane_id.clone(),
            window_id: info.window_id.clone().unwrap_or_default(),
            window_index: None,
            path: info.working_dir.clone(),
            // Not the tmux pane title - that defaults to the hostname, which
            // would print the same word under every row. The third tile line
            // is the free-form detail slot, and for a shell the detail worth
            // having is what it is running.
            pane_title: Some(command.to_string()),
            // Terminals borrow the agent status vocabulary rather than
            // inventing icons: a prompt is Done (whatever ran last finished),
            // an unrecognised command is Working (spinner, it is grinding
            // towards an exit), and a known TUI is neither - it just sits
            // there, so it goes idle and keeps its own glyph instead.
            status: if label.icon.is_some() {
                None
            } else if SHELLS.contains(&command) {
                Some(crate::multiplexer::AgentStatus::Done)
            } else {
                Some(crate::multiplexer::AgentStatus::Working)
            },
            status_ts: stamp,
            updated_ts: stamp,
            window_cmd: Some(command.to_string()),
            agent_command: None,
            agent_kind: None,
            terminal: Some(label),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_drops_the_branch_segment_outside_a_repo() {
        let dir = PathBuf::from("/nowhere/scratch");
        let none = BTreeMap::new();
        assert_eq!(label(&dir, "zsh", None, &none).joined(), "/n/scratch/zsh");

        let parts = label(&dir, "nvim", Some("feat/x"), &none);
        assert_eq!(parts.dir, "/n/scratch");
        assert_eq!(parts.branch.as_deref(), Some("feat/x"));
        assert_eq!(parts.command, "nvim");
        assert_eq!(parts.icon.as_deref(), Some("\u{e62b}"));
    }

    fn pane(command: &str) -> LivePaneInfo {
        LivePaneInfo {
            pid: Some(1),
            current_command: Some(command.to_string()),
            working_dir: PathBuf::from("/tmp/proj"),
            title: None,
            session: Some("0".to_string()),
            window: Some("win".to_string()),
            session_id: None,
            window_id: Some("@0".to_string()),
        }
    }

    #[test]
    fn tui_icons_drive_the_idle_status() {
        let none = BTreeMap::new();
        assert_eq!(tui_icon("nvim", &none).as_deref(), Some("\u{e62b}"));
        assert_eq!(tui_icon("htop", &none).as_deref(), Some("\u{f0ae}"));
        // a build is not a TUI: it keeps the spinner
        assert_eq!(tui_icon("cargo", &none), None);

        let overrides = BTreeMap::from([
            ("cargo".to_string(), "\u{e7a8}".to_string()),
            ("nvim".to_string(), String::new()), // empty removes it
        ]);
        assert_eq!(tui_icon("cargo", &overrides).as_deref(), Some("\u{e7a8}"));
        assert_eq!(tui_icon("nvim", &overrides), None);

        // and the status follows from membership, not from a second list
        let dir = PathBuf::from("/tmp/x");
        assert!(label(&dir, "nvim", None, &none).icon.is_some());
        assert!(label(&dir, "cargo", None, &none).icon.is_none());
    }

    #[test]
    fn shrink_dir_matches_the_zsh_prompt() {
        let home = PathBuf::from("/home/sph");
        let shrink = |p: &str| shrink_dir(&PathBuf::from(p), Some(home.as_path()));

        assert_eq!(shrink("/home/sph"), "~");
        assert_eq!(shrink("/home/sph/projects/work"), "~/p/work");
        assert_eq!(shrink("/home/sph/.config/nixos"), "~/./nixos");
        assert_eq!(
            shrink("/home/sph/projects/work/20260806_eyesatop/shawarma"),
            "~/p/w/2/shawarma"
        );
        assert_eq!(shrink("/etc/nixos"), "/e/nixos");
        assert_eq!(shrink("/tmp"), "/tmp");

        // no home to anchor against: still shortened, still absolute
        assert_eq!(shrink_dir(&PathBuf::from("/etc/nixos"), None), "/e/nixos");
    }

    #[test]
    fn ssh_client_match_ignores_the_trailing_remote_command() {
        let argv: Vec<String> = ["autossh", "-M", "0", "-t", "s", "tmux attach || tmux"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(is_ssh_client_for(&argv, "s"));
        assert!(!is_ssh_client_for(&argv, "tmux"));
        assert!(!is_ssh_client_for(&argv, "other"));

        // not a client at all
        let argv: Vec<String> = ["zsh".to_string(), "s".to_string()].to_vec();
        assert!(!is_ssh_client_for(&argv, "s"));
    }

    #[test]
    fn activity_stamps_command_changes_only() {
        let mut tracker = ActivityTracker::default();
        let mut live = HashMap::from([("%1".to_string(), pane("zsh"))]);

        // first pass seeds without stamping: a daemon restart must not float
        // every terminal to the top
        tracker.observe(&live, 100);
        assert_eq!(tracker.ts("%1"), None);

        // same command a tick later - still unstamped, no drift
        tracker.observe(&live, 200);
        assert_eq!(tracker.ts("%1"), None);

        // ran something: stamped, so the timer restarts from here
        live.insert("%1".to_string(), pane("nvim"));
        tracker.observe(&live, 300);
        assert_eq!(tracker.ts("%1"), Some(300));

        // still nvim two ticks on: the stamp must not move
        tracker.observe(&live, 400);
        assert_eq!(tracker.ts("%1"), Some(300));

        // back to the prompt is a change too
        live.insert("%1".to_string(), pane("zsh"));
        tracker.observe(&live, 500);
        assert_eq!(tracker.ts("%1"), Some(500));

        // a pane opened after priming is new, so it is stamped
        live.insert("%2".to_string(), pane("zsh"));
        tracker.observe(&live, 600);
        assert_eq!(tracker.ts("%2"), Some(600));

        // %1 closed: its stamp must not linger for a recycled pane id
        live.remove("%1");
        tracker.observe(&live, 700);
        assert_eq!(tracker.ts("%1"), None);
    }

    #[test]
    fn read_branch_follows_a_worktree_git_file() {
        let tmp = tempfile::tempdir().unwrap();
        let gitdir = tmp.path().join("repo/.git/worktrees/feature");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();

        let worktree = tmp.path().join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();

        assert_eq!(read_branch(&worktree).as_deref(), Some("feature-x"));
    }

    #[test]
    fn synthesize_skips_agents_and_workmux_itself() {
        let mut live = HashMap::new();
        for (id, command) in [("%1", "zsh"), ("%2", "claude"), ("%3", "workmux")] {
            live.insert(id.to_string(), pane(command));
        }
        let agents = HashSet::from(["%2".to_string()]);

        let rows = synthesize(
            &live,
            &agents,
            &HashSet::new(),
            &ActivityTracker::default(),
            &mut BranchCache::default(),
            &BTreeMap::new(),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pane_id, "%1");
        assert_eq!(rows[0].pane_title.as_deref(), Some("zsh"));
        assert_eq!(rows[0].terminal.as_ref().unwrap().command, "zsh");
        // at a prompt, so Done - the sidebar shows the same tick an agent gets
        assert_eq!(rows[0].status, Some(crate::multiplexer::AgentStatus::Done));
    }
}
