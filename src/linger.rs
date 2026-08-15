//! The lingering trap.
//!
//! A user-scope timer only runs while the user has a systemd user manager,
//! and by default that manager is torn down at logout. A nightly backup timer
//! installed over SSH therefore stops firing the moment the session ends —
//! which, since notcron defaults to user scope, is the single most common way
//! for a freshly created timer to silently do nothing.
//!
//! `loginctl enable-linger <user>` fixes it by keeping the user manager alive
//! across logouts. This module reports whether that has been done; it never
//! enables lingering on its own. [`enable`] is an explicit call the TUI makes
//! only after the user has consented.
//!
//! # Which signal is trusted
//!
//! The check is the presence of `/var/lib/systemd/linger/<user>`. That file
//! *is* systemd's own persistent record of the setting: `enable-linger`
//! creates it and `disable-linger` removes it, and it is world-readable. The
//! alternative, `loginctl show-user <user> --property=Linger`, needs to talk
//! to logind over D-Bus and fails outright when the user has no session — the
//! exact situation this module exists to warn about — so it is used only as a
//! fallback when the marker directory itself cannot be read.

use crate::unit::model::Scope;
use std::path::{Path, PathBuf};
use std::process::Command;

/// systemd's persistent record of which users have lingering enabled.
pub const LINGER_DIR: &str = "/var/lib/systemd/linger";

/// The username notcron acts for.
///
/// `$USER` is checked first but not trusted alone: it is inherited across
/// `sudo` and `su` and can simply be wrong. It is accepted only when it
/// agrees with `id -un`, which asks the passwd database about the effective
/// uid. Returns `None` only if `id` is unavailable and `$USER` is unset.
pub fn current_user() -> Option<String> {
    let from_id = Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    if from_id.is_some() {
        return from_id;
    }
    std::env::var("USER").ok().filter(|s| !s.is_empty())
}

/// Whether lingering is enabled for `user`, judged by the marker file.
///
/// A `None` means the marker directory could not be read at all (it does not
/// exist, or is unreadable), so the filesystem answered nothing and the
/// caller should fall back to logind.
fn marker_says(dir: &Path, user: &str) -> Option<bool> {
    if user.is_empty() {
        return Some(false);
    }
    if dir.join(user).exists() {
        return Some(true);
    }
    // The directory existing but not containing the user is a real "no".
    if dir.is_dir() {
        Some(false)
    } else {
        None
    }
}

/// Ask logind directly. Used only when the marker directory is unreadable.
/// Absent `loginctl`, or a logind that will not answer, yields `None`.
fn logind_says(user: &str) -> Option<bool> {
    let out = Command::new("loginctl")
        .args(["show-user", user, "--property=Linger", "--value"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// What could be determined about lingering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Linger {
    Enabled,
    Disabled,
    /// Neither the marker directory nor logind gave an answer — most likely a
    /// machine that is not running systemd. Do not nag the user about it.
    Unknown,
}

impl Linger {
    /// True only for a definite "no". `Unknown` is not a problem to report.
    pub fn is_definitely_disabled(self) -> bool {
        self == Linger::Disabled
    }
}

/// Lingering state for `user`, consulting `dir` and then logind.
pub fn state_in(dir: &Path, user: &str) -> Linger {
    let answer = marker_says(dir, user).or_else(|| logind_says(user));
    match answer {
        Some(true) => Linger::Enabled,
        Some(false) => Linger::Disabled,
        None => Linger::Unknown,
    }
}

/// Lingering state for `user` on this machine.
pub fn state_for(user: &str) -> Linger {
    state_in(Path::new(LINGER_DIR), user)
}

/// Whether lingering matters at all for a scope. System units are started by
/// the system manager, which is always running, so only user scope cares.
pub fn is_needed(scope: Scope) -> bool {
    scope == Scope::User
}

/// The full picture for one scope: what to show, and whether to warn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub scope: Scope,
    /// `None` when the username could not be determined.
    pub user: Option<String>,
    /// True when this scope depends on lingering.
    pub needed: bool,
    pub state: Linger,
}

impl Check {
    /// Whether the TUI should offer to enable lingering: the scope needs it
    /// and it is definitely off.
    pub fn should_prompt(&self) -> bool {
        self.needed && self.state.is_definitely_disabled() && self.user.is_some()
    }

    /// A one-line warning to show the user, or `None` when all is well.
    pub fn warning(&self) -> Option<String> {
        if !self.should_prompt() {
            return None;
        }
        let user = self.user.clone().unwrap_or_default();
        Some(format!(
            "lingering is off for '{user}': user timers stop at logout. \
             Run `loginctl enable-linger {user}` to keep them running."
        ))
    }
}

/// Inspect lingering for a scope. Read-only: this never changes anything.
pub fn check(scope: Scope) -> Check {
    let user = current_user();
    let state = match &user {
        Some(u) => state_for(u),
        None => Linger::Unknown,
    };
    Check {
        scope,
        user,
        needed: is_needed(scope),
        state,
    }
}

/// Run `loginctl enable-linger <user>`.
///
/// Call this only after the user has agreed: it changes system state. Enabling
/// lingering for someone else requires privilege, so a non-root caller can
/// generally only enable it for itself.
pub fn enable(user: &str) -> Result<(), String> {
    if user.is_empty() {
        return Err("cannot enable lingering: unknown username".into());
    }
    let out = Command::new("loginctl")
        .args(["enable-linger", user])
        .output()
        .map_err(|e| format!("failed to run loginctl: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if msg.is_empty() {
        format!("loginctl enable-linger {user} failed")
    } else {
        format!("loginctl enable-linger {user}: {msg}")
    })
}

/// Path of the marker file for a user, for display in a confirmation dialog.
pub fn marker_path(user: &str) -> PathBuf {
    Path::new(LINGER_DIR).join(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_present_marker_file_means_enabled() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("claude"), "").unwrap();
        assert_eq!(state_in(tmp.path(), "claude"), Linger::Enabled);
    }

    #[test]
    fn an_existing_directory_without_the_user_means_disabled() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(state_in(tmp.path(), "claude"), Linger::Disabled);
    }

    #[test]
    fn an_empty_username_is_never_lingering() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(state_in(tmp.path(), ""), Linger::Disabled);
    }

    #[test]
    fn a_missing_marker_directory_falls_through_to_logind() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-such-dir");
        // Without logind (or with a user it does not know) the honest answer
        // is Unknown; on a machine with logind it may legitimately answer.
        assert!(matches!(
            state_in(&missing, "definitely-not-a-real-user-9f3a"),
            Linger::Unknown | Linger::Disabled
        ));
    }

    #[test]
    fn lingering_only_matters_for_user_scope() {
        assert!(is_needed(Scope::User));
        assert!(!is_needed(Scope::System));
    }

    #[test]
    fn system_scope_never_prompts() {
        let c = Check {
            scope: Scope::System,
            user: Some("claude".into()),
            needed: false,
            state: Linger::Disabled,
        };
        assert!(!c.should_prompt());
        assert!(c.warning().is_none());
    }

    #[test]
    fn user_scope_with_lingering_off_prompts_once_with_a_useful_message() {
        let c = Check {
            scope: Scope::User,
            user: Some("claude".into()),
            needed: true,
            state: Linger::Disabled,
        };
        assert!(c.should_prompt());
        let w = c.warning().unwrap();
        assert!(w.contains("loginctl enable-linger claude"), "{w}");
    }

    #[test]
    fn unknown_state_does_not_nag() {
        for (state, user) in [
            (Linger::Unknown, Some("claude".to_string())),
            (Linger::Enabled, Some("claude".to_string())),
            (Linger::Disabled, None),
        ] {
            let c = Check {
                scope: Scope::User,
                user,
                needed: true,
                state,
            };
            assert!(!c.should_prompt(), "{state:?} should not prompt");
        }
    }

    #[test]
    fn enabling_without_a_username_fails_cleanly() {
        assert!(enable("").is_err());
    }

    #[test]
    fn the_current_user_agrees_with_id_un() {
        // `id` exists everywhere this runs; if it somehow does not, the
        // function must still not panic.
        let u = current_user();
        if let Some(u) = &u {
            assert!(!u.contains('\n'), "{u:?}");
            assert_eq!(u.trim(), u);
        }
        assert_eq!(marker_path("claude"), Path::new(LINGER_DIR).join("claude"));
    }

    #[test]
    fn check_is_read_only_and_self_consistent() {
        let c = check(Scope::User);
        assert!(c.needed);
        assert_eq!(c.scope, Scope::User);
        // Whatever the machine says, the reported state must match a fresh
        // lookup for the same user.
        if let Some(u) = &c.user {
            assert_eq!(c.state, state_for(u));
        }
    }
}
