//! Advisory checks on a unit, over and above the structural validation in
//! [`crate::unit::model::Unit::validate`].
//!
//! Everything here is *diagnostic*: it never blocks the user, it tells them
//! what systemd is going to do differently from what a shell would. The two
//! traps this exists for are that systemd does not run `ExecStart=` through a
//! shell (so `|`, `>`, `*` and friends are just literal arguments) and does
//! not resolve it through `$PATH` the way a login shell does.
//!
//! All functions are pure apart from stat()ing the filesystem, and none of
//! them panic on unreadable or missing paths.

use crate::unit::escape;
use crate::unit::model::{Body, Scope, ServiceOpts, Unit};
use std::path::{Path, PathBuf};

/// How seriously to take a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// The unit will load, but probably will not do what was intended.
    Warning,
    /// The unit will fail to start.
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

/// One advisory finding about a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
}

impl Diagnostic {
    pub fn warning(message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Warning,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            level: Level::Error,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.level.as_str(), self.message)
    }
}

/// True when any diagnostic in the set is an error.
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.level == Level::Error)
}

// ---------------------------------------------------------------------------
// Command line splitting
// ---------------------------------------------------------------------------

/// Split an `ExecStart=` value into its arguments, undoing the quoting that
/// [`escape::exec_quote`] applies.
///
/// Follows systemd's rules: whitespace separates arguments, `"` and `'` quote
/// runs, and `\` escapes the next character (inside double quotes and out;
/// single quotes are literal). Unterminated quotes are tolerated — the rest
/// of the line simply becomes the last argument — because this runs on
/// half-typed input in the builder.
pub fn split_exec(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            '\\' => {
                have = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '"' => {
                have = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                cur.push(n);
                            }
                        }
                        other => cur.push(other),
                    }
                }
            }
            '\'' => {
                have = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            other => {
                have = true;
                cur.push(other);
            }
        }
    }
    if have {
        args.push(cur);
    }
    args
}

/// systemd's `ExecStart=` prefix characters: `-` ignore failure, `@` set
/// argv[0], `:` no variable expansion, `+` `!` `!!` privilege modifiers.
const EXEC_PREFIXES: [char; 5] = ['-', '@', ':', '+', '!'];

/// Strip any leading `ExecStart=` prefix characters from a token.
pub fn strip_exec_prefixes(token: &str) -> &str {
    token.trim_start_matches(|c| EXEC_PREFIXES.contains(&c))
}

/// The binary an `ExecStart=` line will actually execute, with quoting undone
/// and prefix characters removed. `None` for an empty line.
pub fn exec_binary(line: &str) -> Option<String> {
    let args = split_exec(line);
    let first = strip_exec_prefixes(args.first()?);
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// True when the line already hands the work to a shell, e.g.
/// `/bin/sh -c '...'`. Recognises the common shells by basename.
pub fn is_shell_wrapped(line: &str) -> bool {
    let args = split_exec(line);
    let Some(bin) = args.first().map(|a| strip_exec_prefixes(a)) else {
        return false;
    };
    let base = bin.rsplit('/').next().unwrap_or(bin);
    if !matches!(base, "sh" | "bash" | "dash" | "zsh" | "ksh" | "busybox") {
        return false;
    }
    // The -c may not be argv[1] (`bash -l -c ...`), so look at the flags.
    args.iter()
        .skip(1)
        .take_while(|a| a.starts_with('-'))
        .any(|a| a.contains('c'))
}

// ---------------------------------------------------------------------------
// Locating binaries
// ---------------------------------------------------------------------------

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Resolve a bare command name against an explicit `PATH`-style search list.
/// Split out from [`which`] so it is testable without touching the
/// environment (which is process-global and hostile to parallel tests).
pub fn which_in(cmd: &str, path: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return None;
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(cmd))
        .find(|p| is_executable_file(p))
}

/// Resolve a bare command name against the inherited `$PATH`. Falls back to
/// the POSIX default search list when `$PATH` is unset.
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into());
    which_in(cmd, &path)
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// Check one `Exec*=` line. `field` names the directive in the messages, so
/// this serves `ExecStart=`, `ExecStartPre=` and `ExecStopPost=` alike.
pub fn check_exec_line(field: &str, line: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if line.trim().is_empty() {
        return out;
    }
    let Some(bin) = exec_binary(line) else {
        out.push(Diagnostic::error(format!("{field}= has no command")));
        return out;
    };

    if bin.starts_with('/') {
        let p = Path::new(&bin);
        match std::fs::metadata(p) {
            Err(_) => out.push(Diagnostic::error(format!(
                "{field}=: '{bin}' does not exist"
            ))),
            Ok(m) if m.is_dir() => out.push(Diagnostic::error(format!(
                "{field}=: '{bin}' is a directory, not a program"
            ))),
            Ok(_) if !is_executable_file(p) => out.push(Diagnostic::error(format!(
                "{field}=: '{bin}' exists but is not executable (chmod +x it)"
            ))),
            Ok(_) => {}
        }
    } else if bin.contains('/') {
        // A relative path: systemd resolves it against nothing useful.
        out.push(Diagnostic::error(format!(
            "{field}=: '{bin}' is a relative path; systemd requires an absolute one"
        )));
    } else {
        // A bare name. Modern systemd searches a fixed list of directories,
        // but it is not $PATH and relying on it is a portability trap.
        let msg = match which(&bin) {
            Some(p) => format!(
                "{field}=: '{bin}' is not an absolute path; systemd does not search $PATH — use '{}'",
                p.display()
            ),
            None => format!(
                "{field}=: '{bin}' is not an absolute path and was not found on $PATH; systemd does not search $PATH"
            ),
        };
        out.push(Diagnostic::warning(msg));
    }

    if escape::needs_shell(line) && !is_shell_wrapped(line) {
        out.push(Diagnostic::warning(format!(
            "{field}= contains shell syntax, but systemd does not use a shell; \
             wrap it as: /bin/sh -c {}",
            escape::exec_quote(line)
        )));
    }
    out
}

/// Check a whole `[Service]` block in a given scope.
pub fn check_service(svc: &ServiceOpts, scope: Scope) -> Vec<Diagnostic> {
    let mut out = check_exec_line("ExecStart", &svc.exec_start);
    if let Some(pre) = &svc.exec_start_pre {
        out.extend(check_exec_line("ExecStartPre", pre));
    }
    if let Some(post) = &svc.exec_stop_post {
        out.extend(check_exec_line("ExecStopPost", post));
    }

    if let Some(wd) = svc.working_directory.as_deref() {
        let wd = wd.trim();
        if !wd.is_empty() && wd != "~" {
            if !wd.starts_with('/') {
                out.push(Diagnostic::error(format!(
                    "WorkingDirectory=: '{wd}' must be an absolute path"
                )));
            } else if !std::fs::metadata(wd).map(|m| m.is_dir()).unwrap_or(false) {
                out.push(Diagnostic::warning(format!(
                    "WorkingDirectory=: '{wd}' is not an existing directory"
                )));
            }
        }
    }

    if let Some(user) = svc.run_as.as_deref() {
        if !user.trim().is_empty() && scope == Scope::User {
            out.push(Diagnostic::error(format!(
                "User={user} is not allowed in a user-scope unit; \
                 user units always run as you — switch the unit to system scope"
            )));
        }
    }

    if let Some(group) = svc.group.as_deref() {
        if !group.trim().is_empty() && scope == Scope::User {
            out.push(Diagnostic::error(format!(
                "Group={group} is not allowed in a user-scope unit; \
                 a user manager cannot change credentials — switch the unit to system scope"
            )));
        }
    }

    for e in &svc.environment {
        if !e.trim().is_empty() && !e.contains('=') {
            out.push(Diagnostic::error(format!(
                "Environment entry '{e}' is not KEY=VALUE"
            )));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Suggested fixes
// ---------------------------------------------------------------------------

/// A change the UI can apply on one keypress to clear a [`Diagnostic`].
///
/// The diagnostics that carry a fix are the two that already did the work of
/// computing the answer: the bare-command warning resolved the binary against
/// `$PATH`, and the shell-syntax warning already quoted the replacement line.
/// Rather than recompute either, [`autofix`] reads them back out of the
/// message, so the fix offered can never disagree with the text explaining it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    /// Swap the program of `field`'s command line for this absolute path,
    /// keeping the arguments.
    AbsolutePath { field: String, path: String },
    /// Replace `field`'s whole value with this shell-wrapped line.
    ShellWrap { field: String, line: String },
}

impl Fix {
    /// The directive the fix applies to, e.g. `ExecStart`.
    pub fn field(&self) -> &str {
        match self {
            Fix::AbsolutePath { field, .. } | Fix::ShellWrap { field, .. } => field,
        }
    }

    /// One line describing what pressing the key will do.
    pub fn label(&self) -> String {
        match self {
            Fix::AbsolutePath { field, path } => format!("set {field}= to use '{path}'"),
            Fix::ShellWrap { field, .. } => format!("wrap {field}= in /bin/sh -c"),
        }
    }

    /// The value `field` should take, given what it holds now.
    pub fn apply(&self, current: &str) -> String {
        match self {
            Fix::AbsolutePath { path, .. } => replace_program(current, path),
            Fix::ShellWrap { line, .. } => line.clone(),
        }
    }
}

/// Swap the first token of a command line, keeping the rest verbatim.
/// Whitespace inside the remainder is preserved, since it may be quoted.
fn replace_program(current: &str, path: &str) -> String {
    let trimmed = current.trim_start();
    match trimmed.find(char::is_whitespace) {
        Some(i) => format!("{path}{}", &trimmed[i..]),
        None => path.to_string(),
    }
}

/// The directive a message names, i.e. the `ExecStart` of `ExecStart=: ...`.
fn message_field(msg: &str) -> Option<&str> {
    let (field, _) = msg.split_once('=')?;
    if field.is_empty() || field.contains(char::is_whitespace) {
        None
    } else {
        Some(field)
    }
}

/// The value between the first pair of single quotes after `after`.
fn quoted_after<'a>(msg: &'a str, after: &str) -> Option<&'a str> {
    let rest = msg.split_once(after)?.1;
    let rest = rest.strip_prefix('\'')?;
    rest.split_once('\'').map(|(v, _)| v)
}

/// The one-keypress fix for a diagnostic, if it has one.
pub fn autofix(d: &Diagnostic) -> Option<Fix> {
    let msg = &d.message;
    let field = message_field(msg)?.to_string();
    if let Some(path) = quoted_after(msg, "use ") {
        // Only an absolute path is worth substituting in.
        if path.starts_with('/') {
            return Some(Fix::AbsolutePath {
                field,
                path: path.to_string(),
            });
        }
        return None;
    }
    let line = msg.split_once("wrap it as: ")?.1.trim();
    if line.is_empty() {
        return None;
    }
    Some(Fix::ShellWrap {
        field,
        line: line.to_string(),
    })
}

/// Every advisory check that applies to a unit. Mount units have no command
/// to check, so they come back clean.
pub fn check_unit(u: &Unit) -> Vec<Diagnostic> {
    match &u.body {
        Body::Timer(t) => check_service(&t.service, u.scope),
        Body::Service(s) => check_service(&s.service, u.scope),
        Body::Mount(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn levels(d: &[Diagnostic]) -> Vec<Level> {
        d.iter().map(|x| x.level).collect()
    }

    fn joined(d: &[Diagnostic]) -> String {
        d.iter()
            .map(|x| x.message.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    #[test]
    fn splits_plain_arguments() {
        assert_eq!(
            split_exec("/usr/bin/rsync -a /a /b"),
            ["/usr/bin/rsync", "-a", "/a", "/b"]
        );
        assert_eq!(split_exec("   /bin/true  "), ["/bin/true"]);
        assert!(split_exec("").is_empty());
        assert!(split_exec("   ").is_empty());
    }

    #[test]
    fn split_undoes_exec_quote() {
        for args in [
            vec!["/bin/echo", "a b", "say \"hi\"", "back\\slash", ""],
            vec!["/bin/sh", "-c", "df -h | mail -s 'disk' me@example.com"],
        ] {
            let line = args
                .iter()
                .map(|a| escape::exec_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(split_exec(&line), args, "line {line}");
        }
    }

    #[test]
    fn split_handles_single_quotes_and_unterminated_quotes() {
        assert_eq!(split_exec("/bin/sh -c 'a b'"), ["/bin/sh", "-c", "a b"]);
        assert_eq!(
            split_exec("/bin/echo \"unterminated"),
            ["/bin/echo", "unterminated"]
        );
        assert_eq!(split_exec("/bin/echo ''"), ["/bin/echo", ""]);
    }

    #[test]
    fn exec_binary_strips_prefix_characters() {
        assert_eq!(strip_exec_prefixes("-@/bin/true"), "/bin/true");
        assert_eq!(exec_binary("-/bin/true").unwrap(), "/bin/true");
        assert_eq!(exec_binary("!!/bin/true arg").unwrap(), "/bin/true");
        assert_eq!(exec_binary("\"/opt/my prog\" -x").unwrap(), "/opt/my prog");
        assert!(exec_binary("").is_none());
        assert!(exec_binary("-").is_none());
    }

    #[test]
    fn detects_shell_wrapping() {
        assert!(is_shell_wrapped("/bin/sh -c 'echo hi > /tmp/x'"));
        assert!(is_shell_wrapped("/usr/bin/bash -l -c 'x'"));
        assert!(is_shell_wrapped("-/bin/sh -c x"));
        assert!(!is_shell_wrapped("/bin/sh /opt/script.sh"));
        assert!(!is_shell_wrapped("/usr/bin/rsync -a /a /b"));
        assert!(!is_shell_wrapped(""));
    }

    #[test]
    fn accepts_an_existing_executable() {
        let d = check_exec_line("ExecStart", "/bin/sh -c \"echo hi > /tmp/x\"");
        assert!(d.is_empty(), "{}", joined(&d));
    }

    #[test]
    fn empty_lines_produce_nothing() {
        assert!(check_exec_line("ExecStartPre", "").is_empty());
        assert!(check_exec_line("ExecStartPre", "   ").is_empty());
    }

    #[test]
    fn missing_binary_is_an_error() {
        let d = check_exec_line("ExecStart", "/nonexistent/definitely/not-here --flag");
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("does not exist"));
    }

    #[test]
    fn a_directory_is_not_a_program() {
        let d = check_exec_line("ExecStart", "/tmp");
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("directory"));
    }

    #[test]
    fn non_executable_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("script.sh");
        std::fs::write(&f, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let d = check_exec_line("ExecStart", &f.to_string_lossy());
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("not executable"), "{}", joined(&d));

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_exec_line("ExecStart", &f.to_string_lossy()).is_empty());
    }

    #[test]
    fn relative_paths_are_an_error() {
        let d = check_exec_line("ExecStart", "./backup.sh");
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("relative path"));
    }

    #[test]
    fn bare_command_warns_and_suggests_the_absolute_path() {
        let d = check_exec_line("ExecStart", "sh -x");
        assert_eq!(levels(&d), [Level::Warning]);
        assert!(d[0].message.contains("$PATH"), "{}", joined(&d));
        // /bin/sh or /usr/bin/sh depending on the distro's usr-merge.
        assert!(d[0].message.contains("/sh"), "{}", joined(&d));
    }

    #[test]
    fn bare_command_not_on_path_still_warns() {
        let d = check_exec_line("ExecStart", "definitely-not-a-real-binary-xyzzy");
        assert_eq!(levels(&d), [Level::Warning]);
        assert!(d[0].message.contains("was not found"), "{}", joined(&d));
    }

    #[test]
    fn which_in_finds_only_executables() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let exe = dir.path().join("runme");
        std::fs::write(&exe, "x").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("/nonexistent-dir::{}", dir.path().display());
        assert_eq!(which_in("runme", &path), Some(exe));
        assert_eq!(which_in("plain", &path), None);
        assert_eq!(which_in("absent", &path), None);
        // A name with a slash is a path, not something to search for.
        assert_eq!(which_in("sub/runme", &path), None);
        // A directory of the right name is not a command.
        assert_eq!(which_in("..", &path), None);
    }

    #[test]
    fn shell_syntax_suggests_wrapping() {
        let d = check_exec_line("ExecStart", "/usr/bin/df -h | /usr/bin/mail -s disk root");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].level, Level::Warning);
        assert!(d[0].message.contains("/bin/sh -c"), "{}", joined(&d));
        // The suggestion must be a quoted, paste-able command line.
        assert!(d[0]
            .message
            .contains("\"/usr/bin/df -h | /usr/bin/mail -s disk root\""));
    }

    #[test]
    fn already_wrapped_commands_are_not_nagged() {
        let d = check_exec_line("ExecStart", "/bin/sh -c \"df -h | mail -s disk root\"");
        assert!(d.is_empty(), "{}", joined(&d));
    }

    #[test]
    fn a_bare_command_with_shell_syntax_gets_both_diagnostics() {
        let d = check_exec_line("ExecStart", "df -h | mail root");
        assert_eq!(levels(&d), [Level::Warning, Level::Warning]);
    }

    #[test]
    fn user_on_a_user_scope_unit_is_an_error() {
        let svc = ServiceOpts {
            exec_start: "/bin/true".into(),
            run_as: Some("nobody".into()),
            ..ServiceOpts::default()
        };
        let d = check_service(&svc, Scope::User);
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("User=nobody"));
        // Legal in system scope.
        assert!(check_service(&svc, Scope::System).is_empty());
        // An empty User= is not a mistake.
        let svc2 = ServiceOpts {
            run_as: Some(String::new()),
            ..svc.clone()
        };
        assert!(check_service(&svc2, Scope::User).is_empty());
    }

    #[test]
    fn working_directory_is_checked() {
        let base = ServiceOpts {
            exec_start: "/bin/true".into(),
            ..ServiceOpts::default()
        };
        let rel = ServiceOpts {
            working_directory: Some("relative/dir".into()),
            ..base.clone()
        };
        assert_eq!(levels(&check_service(&rel, Scope::User)), [Level::Error]);

        let missing = ServiceOpts {
            working_directory: Some("/nonexistent/dir/xyzzy".into()),
            ..base.clone()
        };
        assert_eq!(
            levels(&check_service(&missing, Scope::User)),
            [Level::Warning]
        );

        let ok = ServiceOpts {
            working_directory: Some("/tmp".into()),
            ..base.clone()
        };
        assert!(check_service(&ok, Scope::User).is_empty());

        // "~" is systemd's own shorthand for the caller's home directory.
        let tilde = ServiceOpts {
            working_directory: Some("~".into()),
            ..base
        };
        assert!(check_service(&tilde, Scope::User).is_empty());
    }

    #[test]
    fn environment_entries_must_be_key_value() {
        let svc = ServiceOpts {
            exec_start: "/bin/true".into(),
            environment: vec!["A=1".into(), "  ".into(), "NOPE".into()],
            ..ServiceOpts::default()
        };
        let d = check_service(&svc, Scope::User);
        assert_eq!(levels(&d), [Level::Error]);
        assert!(d[0].message.contains("NOPE"));
    }

    #[test]
    fn pre_and_post_lines_are_checked_too() {
        let svc = ServiceOpts {
            exec_start: "/bin/true".into(),
            exec_start_pre: Some("/nonexistent/pre".into()),
            exec_stop_post: Some("/nonexistent/post".into()),
            ..ServiceOpts::default()
        };
        let d = check_service(&svc, Scope::System);
        assert_eq!(d.len(), 2);
        assert!(d[0].message.starts_with("ExecStartPre=:"));
        assert!(d[1].message.starts_with("ExecStopPost=:"));
        assert!(has_errors(&d));
    }

    #[test]
    fn check_unit_covers_every_body() {
        let mut t = Unit::new_timer(Scope::User);
        t.name = "x".into();
        if let Body::Timer(b) = &mut t.body {
            b.service.exec_start = "/nonexistent/x".into();
        }
        assert!(has_errors(&check_unit(&t)));

        let mut s = Unit::new_service(Scope::System);
        s.name = "x".into();
        if let Body::Service(b) = &mut s.body {
            b.service.exec_start = "/bin/true".into();
        }
        assert!(check_unit(&s).is_empty());

        assert!(check_unit(&Unit::new_mount()).is_empty());
    }

    #[test]
    fn diagnostics_render_with_their_level() {
        assert_eq!(Diagnostic::warning("hi").to_string(), "warning: hi");
        assert_eq!(Diagnostic::error("bye").to_string(), "error: bye");
        assert!(!has_errors(&[Diagnostic::warning("hi")]));
    }

    // -----------------------------------------------------------------
    // Suggested fixes
    // -----------------------------------------------------------------

    #[test]
    fn the_bare_command_warning_yields_an_absolute_path_fix() {
        let Some(real) = which("true") else {
            eprintln!("skipping: no 'true' on PATH");
            return;
        };
        let d = check_exec_line("ExecStart", "true --quiet");
        let fix = autofix(&d[0]).expect("a fix");
        assert_eq!(
            fix,
            Fix::AbsolutePath {
                field: "ExecStart".into(),
                path: real.display().to_string(),
            }
        );
        // Applying it swaps the program and keeps the arguments verbatim.
        assert_eq!(
            fix.apply("true --quiet 'a b'"),
            format!("{} --quiet 'a b'", real.display())
        );
        assert_eq!(fix.apply("true"), real.display().to_string());
        assert!(fix.label().contains(&real.display().to_string()));
        assert_eq!(fix.field(), "ExecStart");
    }

    #[test]
    fn the_shell_syntax_warning_yields_a_wrapping_fix() {
        let d = check_exec_line("ExecStart", "/bin/df -h | /usr/bin/mail me");
        let warn = d
            .iter()
            .find(|x| x.message.contains("wrap it as"))
            .expect("the shell warning");
        let fix = autofix(warn).expect("a fix");
        let Fix::ShellWrap { field, line } = &fix else {
            panic!("expected a wrap, got {fix:?}");
        };
        assert_eq!(field, "ExecStart");
        assert!(line.starts_with("/bin/sh -c "), "{line}");
        // The fix replaces the whole value, whatever it held.
        assert_eq!(fix.apply("anything at all"), *line);
        assert!(is_shell_wrapped(&fix.apply("")));
    }

    /// A diagnostic that has no computed answer must not fake one.
    #[test]
    fn diagnostics_without_an_answer_offer_no_fix() {
        for d in [
            Diagnostic::error("ExecStart=: '/nope' does not exist"),
            Diagnostic::warning("WorkingDirectory=: '/nope' is not an existing directory"),
            Diagnostic::error("Environment entry 'BAD' is not KEY=VALUE"),
            Diagnostic::warning("nothing structured here at all"),
        ] {
            assert_eq!(autofix(&d), None, "{d}");
        }
        // Not found on $PATH: the message names no replacement, so neither
        // does the fix.
        let d = check_exec_line("ExecStart", "definitely-not-on-path-xyzzy");
        assert_eq!(autofix(&d[0]), None, "{}", d[0]);
    }

    #[test]
    fn a_fix_is_offered_for_every_exec_directive() {
        if which("true").is_none() {
            eprintln!("skipping: no 'true' on PATH");
            return;
        }
        for field in ["ExecStart", "ExecStartPre", "ExecStopPost"] {
            let d = check_exec_line(field, "true");
            let fix = autofix(&d[0]).unwrap_or_else(|| panic!("{field}"));
            assert_eq!(fix.field(), field);
        }
    }

    #[test]
    fn group_is_refused_in_user_scope_just_as_user_is() {
        let svc = ServiceOpts {
            exec_start: "/bin/true".into(),
            group: Some("backup".into()),
            ..ServiceOpts::default()
        };
        let d = check_service(&svc, Scope::User);
        assert!(has_errors(&d), "{}", joined(&d));
        assert!(joined(&d).contains("Group=backup"), "{}", joined(&d));
        // System scope is fine.
        assert!(check_service(&svc, Scope::System).is_empty());
    }
}
