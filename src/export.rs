//! Exporting a unit's files somewhere other than the unit directory: to a
//! directory of your choosing, or to stdout for pasting into a ticket or gist.
//!
//! Rendering is not reimplemented here — every path goes through
//! [`crate::unit::generate::render`], so an exported file is byte-for-byte
//! what `install` would have written. A unit read off disk and one built in
//! the TUI are both just a [`Unit`], so both export identically; for entries
//! that could not be modelled, [`export_files`] and [`to_text`] take the raw
//! [`RenderedFile`] list directly.

use crate::unit::generate::{self, RenderedFile};
use crate::unit::model::Unit;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Separator between files in the stdout form.
fn separator(name: &str) -> String {
    format!("# ---- {name} ----\n")
}

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Files written, in render order (primary unit first).
    pub written: Vec<PathBuf>,
    /// Files that already existed and were replaced because `overwrite` was
    /// set. A subset of `written`.
    pub replaced: Vec<PathBuf>,
    pub dir: PathBuf,
}

/// Why an export could not proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// The unit itself is not renderable; the message comes from
    /// [`Unit::validate`].
    Invalid(String),
    /// One or more destination files exist. Retry with `overwrite = true`
    /// once the user has confirmed; nothing was written.
    Exists(Vec<PathBuf>),
    /// Destination missing and uncreatable, not writable, disk full, and so
    /// on. Already phrased for the user.
    Io(String),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Invalid(m) => write!(f, "cannot export: {m}"),
            ExportError::Exists(paths) => {
                let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(f, "already exists: {}", names.join(", "))
            }
            ExportError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ExportError {}

/// The files a unit would export as. Validates first, so a half-built unit
/// from the TUI fails here rather than producing nonsense files.
pub fn files(u: &Unit) -> Result<Vec<RenderedFile>, ExportError> {
    u.validate().map_err(ExportError::Invalid)?;
    Ok(generate::render(u))
}

/// Write a unit's files into `dir`, each under its real unit filename.
///
/// The directory is created if missing. An existing destination file is a
/// [`ExportError::Exists`] unless `overwrite` is set — nothing is clobbered
/// silently, and the check happens before any file is written, so a refused
/// export leaves the destination untouched.
pub fn export(u: &Unit, dir: &Path, overwrite: bool) -> Result<ExportReport, ExportError> {
    export_files(&files(u)?, dir, overwrite)
}

/// [`export`] for files that are already rendered — used for units read off
/// disk that notcron cannot model, where the bytes are all there is.
pub fn export_files(
    rendered: &[RenderedFile],
    dir: &Path,
    overwrite: bool,
) -> Result<ExportReport, ExportError> {
    if rendered.is_empty() {
        return Err(ExportError::Invalid("the unit has no files".into()));
    }
    fs::create_dir_all(dir).map_err(|e| ExportError::Io(explain(dir, &e, "creating")))?;
    if !dir.is_dir() {
        return Err(ExportError::Io(format!(
            "{} is not a directory",
            dir.display()
        )));
    }

    let targets: Vec<PathBuf> = rendered.iter().map(|f| dir.join(&f.name)).collect();
    let existing: Vec<PathBuf> = targets.iter().filter(|p| p.exists()).cloned().collect();
    if !existing.is_empty() && !overwrite {
        return Err(ExportError::Exists(existing));
    }

    let mut report = ExportReport {
        dir: dir.to_path_buf(),
        replaced: existing,
        ..ExportReport::default()
    };
    for (f, path) in rendered.iter().zip(&targets) {
        fs::write(path, &f.body).map_err(|e| ExportError::Io(explain(path, &e, "writing")))?;
        report.written.push(path.clone());
    }
    Ok(report)
}

/// Every file concatenated with `# ---- <filename> ----` separators, ready to
/// paste into a ticket or a gist.
pub fn to_text(rendered: &[RenderedFile]) -> String {
    let mut out = String::new();
    for f in rendered {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&separator(&f.name));
        out.push_str(&f.body);
    }
    out
}

/// [`to_text`] for a unit, validating it first.
pub fn text(u: &Unit) -> Result<String, ExportError> {
    Ok(to_text(&files(u)?))
}

/// Write the pasteable form to any sink — stdout in the CLI, a buffer in
/// tests. A broken pipe (`notcron export | head`) is not an error.
pub fn write_text(u: &Unit, out: &mut impl Write) -> Result<(), ExportError> {
    let body = text(u)?;
    match out.write_all(body.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(ExportError::Io(format!("writing to output: {e}"))),
    }
}

/// Turn an I/O error into something worth showing a user.
fn explain(path: &Path, e: &io::Error, verb: &str) -> String {
    let hint = match e.kind() {
        io::ErrorKind::PermissionDenied => " (no write permission)",
        io::ErrorKind::NotFound => " (parent directory does not exist)",
        _ => "",
    };
    format!("{verb} {}: {e}{hint}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::model::{Body, Scope};
    use tempfile::TempDir;

    fn sample() -> Unit {
        let mut u = Unit::new_timer(Scope::User);
        u.name = "backup".into();
        u.description = "nightly backup".into();
        if let Body::Timer(t) = &mut u.body {
            t.service.exec_start = "/usr/local/bin/backup.sh".into();
        }
        u
    }

    #[test]
    fn export_writes_every_file_under_its_unit_name() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("out");
        let report = export(&sample(), &dir, false).unwrap();
        assert_eq!(
            report.written,
            vec![
                dir.join("notcron-backup.timer"),
                dir.join("notcron-backup.service"),
            ]
        );
        assert!(report.replaced.is_empty());
        // The bytes are exactly what the renderer produces.
        for (f, p) in generate::render(&sample()).iter().zip(&report.written) {
            assert_eq!(fs::read_to_string(p).unwrap(), f.body);
        }
    }

    #[test]
    fn a_missing_destination_directory_is_created() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("deep/nested/out");
        assert!(!dir.exists());
        export(&sample(), &dir, false).unwrap();
        assert!(dir.join("notcron-backup.timer").is_file());
    }

    #[test]
    fn an_existing_file_is_refused_and_nothing_is_written() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let timer = dir.join("notcron-backup.timer");
        fs::write(&timer, "mine\n").unwrap();

        match export(&sample(), &dir, false) {
            Err(ExportError::Exists(paths)) => assert_eq!(paths, vec![timer.clone()]),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&timer).unwrap(), "mine\n");
        // The refusal is total: the second file was not written either.
        assert!(!dir.join("notcron-backup.service").exists());
    }

    #[test]
    fn overwrite_replaces_and_reports_what_it_replaced() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();
        let timer = dir.join("notcron-backup.timer");
        fs::write(&timer, "mine\n").unwrap();

        let report = export(&sample(), &dir, true).unwrap();
        assert_eq!(report.replaced, vec![timer.clone()]);
        assert_eq!(report.written.len(), 2);
        assert!(fs::read_to_string(&timer).unwrap().contains("[Timer]"));
    }

    #[test]
    fn an_unwritable_destination_is_an_error_not_a_panic() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ro");
        fs::create_dir(&dir).unwrap();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o500);
        }
        fs::set_permissions(&dir, perms).unwrap();

        let result = export(&sample(), &dir, false);
        // Running as root defeats the permission bits; only assert when the
        // filesystem actually refused.
        if let Err(e) = &result {
            assert!(matches!(e, ExportError::Io(_)), "{e:?}");
            assert!(e.to_string().contains("notcron-backup"), "{e}");
        }
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o700);
        }
        fs::set_permissions(&dir, perms).unwrap();
    }

    #[test]
    fn an_invalid_unit_is_refused_before_touching_the_filesystem() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("out");
        let u = Unit::new_timer(Scope::User); // no name, no ExecStart
        assert!(matches!(
            export(&u, &dir, false),
            Err(ExportError::Invalid(_))
        ));
        assert!(!dir.exists());
        assert!(text(&u).is_err());
    }

    #[test]
    fn text_form_separates_files_by_name() {
        let body = text(&sample()).unwrap();
        assert!(
            body.starts_with("# ---- notcron-backup.timer ----\n"),
            "{body}"
        );
        assert!(
            body.contains("\n# ---- notcron-backup.service ----\n"),
            "{body}"
        );
        // Every rendered byte survives the concatenation.
        for f in generate::render(&sample()) {
            assert!(body.contains(&f.body), "missing {}", f.name);
        }
    }

    #[test]
    fn write_text_goes_to_any_sink() {
        let mut buf: Vec<u8> = Vec::new();
        write_text(&sample(), &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), text(&sample()).unwrap());
    }

    #[test]
    fn pre_rendered_files_can_be_exported_directly() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("out");
        let raw = vec![RenderedFile {
            name: "foreign.timer".into(),
            body: "[Timer]\nOnCalendar=daily\n".into(),
        }];
        let report = export_files(&raw, &dir, false).unwrap();
        assert_eq!(report.written, vec![dir.join("foreign.timer")]);
        assert_eq!(
            to_text(&raw),
            "# ---- foreign.timer ----\n[Timer]\nOnCalendar=daily\n"
        );
        assert!(matches!(
            export_files(&[], &dir, false),
            Err(ExportError::Invalid(_))
        ));
    }
}
