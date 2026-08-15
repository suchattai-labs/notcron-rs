//! Completion sources for the builder's text fields.
//!
//! Pure data: nothing here draws anything or reads a key. A caller asks for
//! the completions of a partially typed string and gets back the candidates
//! plus the longest common prefix, which is the standard tab-completion
//! contract — complete to the common prefix first, list on the second tab.

use std::path::{Path, PathBuf};

/// The result of completing a partial input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Completion {
    /// Every candidate, sorted, each a full replacement for the input.
    pub candidates: Vec<String>,
    /// The longest string every candidate starts with, also a full
    /// replacement for the input. Equal to the input when nothing matched,
    /// and equal to the single candidate when exactly one matched.
    pub common: String,
}

impl Completion {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// True when the input is already complete and unambiguous.
    pub fn is_unique(&self) -> bool {
        self.candidates.len() == 1
    }
}

/// Build a [`Completion`] from candidates plus the input to fall back on.
fn assemble(mut candidates: Vec<String>, input: &str) -> Completion {
    candidates.sort();
    candidates.dedup();
    let common = longest_common_prefix(&candidates).unwrap_or_else(|| input.to_string());
    Completion { candidates, common }
}

/// The longest prefix shared by every string, or `None` for an empty slice.
/// Operates on `char` boundaries so it never splits a UTF-8 sequence.
pub fn longest_common_prefix(items: &[String]) -> Option<String> {
    let first = items.first()?;
    let mut len = first.chars().count();
    for other in &items[1..] {
        let common = first
            .chars()
            .zip(other.chars())
            .take_while(|(a, b)| a == b)
            .count();
        len = len.min(common);
    }
    Some(first.chars().take(len).collect())
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `$HOME`, or `/` if it is unset — never an error, so completion of `~/`
/// degrades instead of failing.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~` or `~/` against `$HOME`. Other users' homes (`~bob`)
/// are deliberately not expanded: that needs a passwd lookup for a form
/// almost nobody types into a unit file.
pub fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return home().to_string_lossy().into_owned();
    }
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest).to_string_lossy().into_owned(),
        None => s.to_string(),
    }
}

/// Split a partial path into (directory to list, filename prefix to match).
/// A trailing `/` means "list this directory", with an empty prefix.
fn split_partial(partial: &str) -> (String, &str) {
    match partial.rsplit_once('/') {
        // "/usr/bi" -> ("/usr/", "bi"); "/bi" -> ("/", "bi")
        Some((dir, base)) => (format!("{dir}/"), base),
        // No slash at all: relative to the current directory.
        None => (String::new(), partial),
    }
}

/// Complete a partial filesystem path.
///
/// Candidates keep the shape the user typed: a `~/` stays a `~/`, a relative
/// path stays relative. Directories come back with a trailing `/` so a second
/// completion descends into them. Unreadable directories yield no candidates
/// rather than an error.
///
/// `dirs_only` restricts the result to directories, for fields like
/// `WorkingDirectory=`.
pub fn complete_path(partial: &str, dirs_only: bool) -> Completion {
    let (dir_part, base) = split_partial(partial);
    // The directory actually read: tilde-expanded, and "" means the cwd.
    let lookup = if dir_part.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(expand_tilde(&dir_part))
    };

    let Ok(rd) = std::fs::read_dir(&lookup) else {
        return Completion {
            candidates: Vec::new(),
            common: partial.to_string(),
        };
    };

    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.starts_with(base) {
            continue;
        }
        // Hidden entries only show up once the user has typed the dot.
        if name.starts_with('.') && !base.starts_with('.') {
            continue;
        }
        // file_type() does not follow symlinks; metadata() does, which is
        // what matters for "can I cd into this".
        let is_dir = std::fs::metadata(ent.path())
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if dirs_only && !is_dir {
            continue;
        }
        out.push(format!("{dir_part}{name}{}", if is_dir { "/" } else { "" }));
    }
    assemble(out, partial)
}

/// Complete a partial path to an executable file, for `ExecStart=` fields.
pub fn complete_executable(partial: &str) -> Completion {
    let c = complete_path(partial, false);
    let kept: Vec<String> = c
        .candidates
        .into_iter()
        .filter(|p| {
            p.ends_with('/') || {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(expand_tilde(p))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
        })
        .collect();
    assemble(kept, partial)
}

// ---------------------------------------------------------------------------
// Users and groups
// ---------------------------------------------------------------------------

/// Which accounts to offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accounts {
    /// Accounts a human could plausibly be running jobs as: `root`, plus
    /// anything with a real login shell and a uid at or above 1000. This is
    /// the default because `User=` on a timer is nearly always a person or a
    /// purpose-built service account, and offering all ~40 of a distro's
    /// system accounts buries them.
    #[default]
    Login,
    /// Every account in the file, system users included — needed when the
    /// unit really should run as `www-data` or `nobody`.
    All,
}

/// Shells that mean "this account cannot log in".
fn is_nologin(shell: &str) -> bool {
    let base = shell.rsplit('/').next().unwrap_or(shell);
    shell.is_empty() || matches!(base, "nologin" | "false" | "sync")
}

/// Parse `/etc/passwd` content into matching user names.
///
/// Malformed lines (too few fields, non-numeric uid) and comments are
/// skipped; a corrupt line must not lose the rest of the file.
pub fn users_from_passwd(contents: &str, prefix: &str, which: Accounts) -> Vec<String> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let (name, uid, shell) = (f[0], f[2], f[6]);
        if name.is_empty() {
            continue;
        }
        let Ok(uid) = uid.parse::<u32>() else {
            continue;
        };
        if which == Accounts::Login && uid != 0 && (uid < 1000 || uid == 65534 || is_nologin(shell))
        {
            continue;
        }
        if name.starts_with(prefix) {
            out.push(name.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Parse `/etc/group` content into matching group names. Groups have no
/// shell to judge them by, so [`Accounts::Login`] keeps `root` and gid >= 1000.
pub fn groups_from_group(contents: &str, prefix: &str, which: Accounts) -> Vec<String> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 3 {
            continue;
        }
        let (name, gid) = (f[0], f[2]);
        if name.is_empty() {
            continue;
        }
        let Ok(gid) = gid.parse::<u32>() else {
            continue;
        };
        if which == Accounts::Login && gid != 0 && (gid < 1000 || gid == 65534) {
            continue;
        }
        if name.starts_with(prefix) {
            out.push(name.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn read_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Complete a user name against `/etc/passwd`.
pub fn complete_user(prefix: &str, which: Accounts) -> Completion {
    let text = read_or_empty(Path::new("/etc/passwd"));
    assemble(users_from_passwd(&text, prefix, which), prefix)
}

/// Complete a group name against `/etc/group`.
pub fn complete_group(prefix: &str, which: Accounts) -> Completion {
    let text = read_or_empty(Path::new("/etc/group"));
    assemble(groups_from_group(&text, prefix, which), prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn common_prefix_of_candidates() {
        assert_eq!(longest_common_prefix(&[]), None);
        assert_eq!(longest_common_prefix(&s(&["only"])).unwrap(), "only");
        assert_eq!(
            longest_common_prefix(&s(&["backup-a", "backup-b"])).unwrap(),
            "backup-"
        );
        assert_eq!(longest_common_prefix(&s(&["a", "b"])).unwrap(), "");
        // Must not split a multi-byte character.
        assert_eq!(
            longest_common_prefix(&s(&["\u{fc}ber", "\u{fc}nter"])).unwrap(),
            "\u{fc}"
        );
    }

    struct Tree(tempfile::TempDir);

    impl Tree {
        /// bin/{run,plain}, data/, data2/, README
        fn new() -> Tree {
            let d = tempfile::tempdir().unwrap();
            let p = d.path();
            std::fs::create_dir(p.join("bin")).unwrap();
            std::fs::create_dir(p.join("data")).unwrap();
            std::fs::create_dir(p.join("data2")).unwrap();
            std::fs::write(p.join("README"), "x").unwrap();
            std::fs::write(p.join(".hidden"), "x").unwrap();
            std::fs::write(p.join("bin/plain"), "x").unwrap();
            let run = p.join("bin/run");
            std::fs::write(&run, "x").unwrap();
            std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
            Tree(d)
        }

        fn at(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0.path().display())
        }
    }

    #[test]
    fn completes_a_directory_listing() {
        let t = Tree::new();
        let c = complete_path(&t.at(""), false);
        assert_eq!(
            c.candidates,
            s(&[
                &t.at("README"),
                &t.at("bin/"),
                &t.at("data/"),
                &t.at("data2/")
            ])
        );
        // Hidden files stay hidden until the dot is typed.
        assert!(!c.candidates.iter().any(|x| x.contains("hidden")));
        let hidden = complete_path(&t.at(".h"), false);
        assert_eq!(hidden.candidates, s(&[&t.at(".hidden")]));
    }

    #[test]
    fn completes_to_the_common_prefix_first() {
        let t = Tree::new();
        let c = complete_path(&t.at("d"), false);
        assert_eq!(c.candidates, s(&[&t.at("data/"), &t.at("data2/")]));
        assert_eq!(c.common, t.at("data"));
        assert!(!c.is_unique());
    }

    #[test]
    fn a_unique_match_completes_fully_with_a_slash_for_directories() {
        let t = Tree::new();
        let c = complete_path(&t.at("b"), false);
        assert!(c.is_unique());
        assert_eq!(c.common, t.at("bin/"));
        let f = complete_path(&t.at("RE"), false);
        assert_eq!(f.common, t.at("README"));
    }

    #[test]
    fn trailing_slash_lists_the_directory() {
        let t = Tree::new();
        let c = complete_path(&t.at("bin/"), false);
        assert_eq!(c.candidates, s(&[&t.at("bin/plain"), &t.at("bin/run")]));
    }

    #[test]
    fn dirs_only_filters_out_files() {
        let t = Tree::new();
        let c = complete_path(&t.at(""), true);
        assert_eq!(
            c.candidates,
            s(&[&t.at("bin/"), &t.at("data/"), &t.at("data2/")])
        );
    }

    #[test]
    fn executable_completion_keeps_directories_and_programs() {
        let t = Tree::new();
        let c = complete_executable(&t.at("bin/"));
        assert_eq!(c.candidates, s(&[&t.at("bin/run")]));
        // Directories survive so the user can keep descending.
        let top = complete_executable(&t.at(""));
        assert!(top.candidates.contains(&t.at("bin/")));
        assert!(!top.candidates.contains(&t.at("README")));
    }

    #[test]
    fn no_match_leaves_the_input_alone() {
        let t = Tree::new();
        let c = complete_path(&t.at("zzz"), false);
        assert!(c.is_empty());
        assert_eq!(c.common, t.at("zzz"));
    }

    #[test]
    fn unreadable_or_missing_directories_do_not_panic() {
        let c = complete_path("/nonexistent-dir-xyzzy/foo", false);
        assert!(c.is_empty());
        assert_eq!(c.common, "/nonexistent-dir-xyzzy/foo");
        // /proc/1/root is unreadable for a non-root user; either way, no panic.
        let _ = complete_path("/proc/1/root/", false);
        let _ = complete_path("", false);
    }

    #[test]
    fn relative_paths_complete_against_the_cwd() {
        // Whatever the cwd is, "src/" holds this very file.
        let c = complete_path("src/comp", false);
        assert!(
            c.candidates.iter().any(|x| x == "src/complete.rs"),
            "{:?}",
            c.candidates
        );
    }

    #[test]
    fn tilde_is_expanded_for_lookup_but_kept_in_candidates() {
        assert_eq!(expand_tilde("~"), home().to_string_lossy());
        assert_eq!(
            expand_tilde("~/.config"),
            home().join(".config").to_string_lossy()
        );
        assert_eq!(expand_tilde("/usr/~x"), "/usr/~x");
        assert_eq!(expand_tilde("~bob/x"), "~bob/x");

        // Complete inside $HOME through the tilde form.
        let c = complete_path("~/", false);
        let direct = complete_path(&format!("{}/", home().display()), false);
        assert_eq!(c.candidates.len(), direct.candidates.len());
        assert!(c.candidates.iter().all(|x| x.starts_with("~/")));
    }

    const PASSWD: &str = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
www-data:x:33:33:www-data:/var/www:/usr/sbin/nologin
nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin
# a comment
claude:x:999:999::/home/claude:/bin/bash
alice:x:1000:1000::/home/alice:/bin/bash
albert:x:1001:1001::/home/albert:/bin/sh
frozen:x:1002:1002::/home/frozen:/usr/sbin/nologin
truncated:x:1003
:x:1004:1004::/home/blank:/bin/sh
bogus:x:notanumber:0::/x:/bin/sh
";

    #[test]
    fn passwd_login_filter_keeps_humans_and_root() {
        let all = users_from_passwd(PASSWD, "", Accounts::Login);
        assert_eq!(all, s(&["albert", "alice", "root"]));
    }

    #[test]
    fn passwd_all_includes_system_accounts() {
        let all = users_from_passwd(PASSWD, "", Accounts::All);
        assert!(all.contains(&"www-data".to_string()));
        assert!(all.contains(&"nobody".to_string()));
        assert!(all.contains(&"claude".to_string()));
        // Still no malformed entries.
        assert!(!all.contains(&"truncated".to_string()));
        assert!(!all.contains(&"bogus".to_string()));
        assert!(!all.iter().any(|x| x.is_empty()));
    }

    #[test]
    fn passwd_matches_a_prefix() {
        assert_eq!(
            users_from_passwd(PASSWD, "al", Accounts::Login),
            s(&["albert", "alice"])
        );
        assert!(users_from_passwd(PASSWD, "zz", Accounts::All).is_empty());
        assert_eq!(
            users_from_passwd("", "a", Accounts::All),
            Vec::<String>::new()
        );
    }

    const GROUP: &str = "\
root:x:0:
adm:x:4:syslog
# comment
claude:x:999:
alice:x:1000:
allies:x:1001:alice,albert
short:x
bad:x:nope:
";

    #[test]
    fn group_parsing_mirrors_passwd() {
        assert_eq!(
            groups_from_group(GROUP, "", Accounts::Login),
            s(&["alice", "allies", "root"])
        );
        let all = groups_from_group(GROUP, "", Accounts::All);
        assert!(all.contains(&"adm".to_string()));
        assert!(!all.contains(&"short".to_string()));
        assert!(!all.contains(&"bad".to_string()));
        assert_eq!(
            groups_from_group(GROUP, "al", Accounts::All),
            s(&["alice", "allies"])
        );
    }

    #[test]
    fn live_account_completion_finds_root() {
        // /etc/passwd and /etc/group always have root; if they are somehow
        // unreadable the functions must still return empty, not panic.
        let u = complete_user("roo", Accounts::Login);
        assert!(u.candidates.is_empty() || u.candidates.contains(&"root".to_string()));
        let g = complete_group("roo", Accounts::Login);
        assert!(g.candidates.is_empty() || g.candidates.contains(&"root".to_string()));
    }
}
