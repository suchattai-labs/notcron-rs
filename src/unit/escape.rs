//! Native implementation of `systemd-escape` name/path escaping, plus the
//! `ExecStart=` argument quoting rules.
//!
//! Mount and automount unit filenames are not free-form: systemd derives the
//! unit name from the mount point, so `/srv/my-share` must become
//! `srv-my\x2dshare.mount` exactly. Doing this natively keeps unit naming
//! correct without shelling out on every keystroke in the builder.

/// Characters systemd keeps verbatim in an escaped unit name. `-` and `\`
/// are deliberately excluded here: they carry meaning and are always escaped.
fn is_valid_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'.')
}

fn escape_byte(b: u8, out: &mut String) {
    out.push_str(&format!("\\x{b:02x}"));
}

/// `unit_name_escape()`: `/` becomes `-`, anything outside the valid set
/// becomes `\xNN`, and the bare names `.` and `..` are escaped whole.
pub fn escape_name(s: &str) -> String {
    if s == "." {
        return "\\x2e".into();
    }
    if s == ".." {
        return "\\x2e\\x2e".into();
    }
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b == b'/' {
            out.push('-');
        } else if is_valid_char(b) {
            out.push(b as char);
        } else {
            escape_byte(b, &mut out);
        }
    }
    out
}

/// The inverse of [`escape_name`], for reading a unit name back into a path
/// component. Malformed `\x` sequences are passed through verbatim.
pub fn unescape_name(s: &str) -> String {
    let b = s.as_bytes();
    let mut bytes: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' {
            bytes.push(b'/');
            i += 1;
        } else if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            match u8::from_str_radix(&s[i + 2..i + 4], 16) {
                Ok(v) => {
                    bytes.push(v);
                    i += 4;
                }
                Err(_) => {
                    bytes.push(b[i]);
                    i += 1;
                }
            }
        } else {
            bytes.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Collapse `//`, drop `.` components and strip trailing slashes, the way
/// systemd's `path_simplify()` does. `..` is left alone: it makes a path
/// non-normalized, which the caller rejects.
fn simplify(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        if p.is_empty() || p == "." {
            continue;
        }
        parts.push(p);
    }
    let joined = parts.join("/");
    if path.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

/// `systemd-escape --path`: the unit name (without suffix) for a mount point.
///
/// Returns an error for relative paths and for paths containing `..`, both of
/// which systemd refuses to escape reversibly.
pub fn escape_path(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(format!("'{path}' is not an absolute path"));
    }
    let simplified = simplify(path);
    if simplified.split('/').any(|c| c == "..") {
        return Err(format!("'{path}' is not a normalized path (contains '..')"));
    }
    let trimmed = simplified.trim_matches('/');
    if trimmed.is_empty() {
        // The root mount point is the single unit name "-".
        return Ok("-".into());
    }
    Ok(escape_name(trimmed))
}

/// `systemd-escape --unescape --path`: turn a mount unit name back into an
/// absolute path.
pub fn unescape_path(name: &str) -> String {
    if name == "-" {
        return "/".into();
    }
    format!("/{}", unescape_name(name))
}

/// A slug usable as a unit name, derived from free text. Mirrors the shell
/// script's `slugify`: lowercase, non-alphanumerics collapse to single
/// dashes, trimmed, capped at 40 characters.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true; // suppresses a leading dash
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(40).collect::<String>()
}

/// Quote one argument for an `ExecStart=` line, matching the shell script's
/// `sd_quote`.
pub fn exec_quote(arg: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./:@%+=-".contains(c);
    if arg.is_empty() {
        return "\"\"".into();
    }
    if arg.chars().all(safe) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// True when a command line contains anything that only a shell can run, so
/// it must go through `/bin/sh -c`. Mirrors the shell script's `needs_shell`.
pub fn needs_shell(line: &str) -> bool {
    !line
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " _./:=@%+,-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn escapes_paths_like_systemd() {
        assert_eq!(escape_path("/mnt/data").unwrap(), "mnt-data");
        assert_eq!(escape_path("/srv/my-share").unwrap(), "srv-my\\x2dshare");
        assert_eq!(escape_path("/").unwrap(), "-");
        assert_eq!(escape_path("///").unwrap(), "-");
        assert_eq!(escape_path("/mnt/x/y//z/").unwrap(), "mnt-x-y-z");
        assert_eq!(escape_path("/mnt/wibble.d").unwrap(), "mnt-wibble.d");
        assert_eq!(escape_path("/mnt/it's").unwrap(), "mnt-it\\x27s");
        assert_eq!(
            escape_path("/mnt/\u{fc}n\u{ef}code").unwrap(),
            "mnt-\\xc3\\xbcn\\xc3\\xafcode"
        );
    }

    #[test]
    fn rejects_unescapable_paths() {
        assert!(escape_path("relative/path").is_err());
        assert!(escape_path("/mnt/../etc").is_err());
    }

    #[test]
    fn path_escaping_round_trips() {
        for (input, want) in [
            ("/mnt/data", "/mnt/data"),
            ("/srv/my-share", "/srv/my-share"),
            ("/", "/"),
            ("///", "/"),
            ("/mnt/it's", "/mnt/it's"),
            ("/mnt/a b/c", "/mnt/a b/c"),
            ("/mnt/x/y//z/", "/mnt/x/y/z"),
            ("/mnt/\u{fc}n\u{ef}code", "/mnt/\u{fc}n\u{ef}code"),
        ] {
            let esc = escape_path(input).unwrap();
            assert_eq!(unescape_path(&esc), want, "path {input}");
        }
    }

    /// The real `systemd-escape` is the authority; check against it when the
    /// binary exists so CI without systemd still passes.
    #[test]
    fn matches_systemd_escape_binary() {
        let Ok(probe) = Command::new("systemd-escape")
            .arg("--path")
            .arg("/x")
            .output()
        else {
            eprintln!("skipping: systemd-escape not available");
            return;
        };
        assert!(probe.status.success());
        for p in [
            "/mnt/data",
            "/srv/my-share",
            "/mnt/a-b",
            "/",
            "/mnt/wibble.d",
            "/mnt/x/y//z/",
            "/mnt/it's",
            "/mnt/a b",
            "/mnt/\u{fc}n\u{ef}code",
            "/var/lib/machines/one",
        ] {
            let out = Command::new("systemd-escape")
                .arg("--path")
                .arg(p)
                .output()
                .expect("systemd-escape should run");
            let want = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert_eq!(escape_path(p).unwrap(), want, "path {p}");
        }
    }

    #[test]
    fn escapes_dot_names() {
        assert_eq!(escape_name("."), "\\x2e");
        assert_eq!(escape_name(".."), "\\x2e\\x2e");
    }

    #[test]
    fn slugifies() {
        assert_eq!(slugify("Backup Script"), "backup-script");
        assert_eq!(
            slugify("/usr/local/bin/backup.sh"),
            "usr-local-bin-backup-sh"
        );
        assert_eq!(slugify("--weird--"), "weird");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify(&"x".repeat(60)).len(), 40);
    }

    #[test]
    fn quotes_exec_arguments() {
        assert_eq!(exec_quote("/bin/true"), "/bin/true");
        assert_eq!(exec_quote("--full"), "--full");
        assert_eq!(exec_quote(""), "\"\"");
        assert_eq!(exec_quote("a b"), "\"a b\"");
        assert_eq!(exec_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(exec_quote("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn detects_shell_syntax() {
        assert!(!needs_shell("/usr/bin/rsync -a /data /backup"));
        assert!(needs_shell("df -h | mail -s disk me@example.com"));
        assert!(needs_shell("echo hi > /tmp/x"));
        assert!(needs_shell("a && b"));
    }
}
