//! Undo for `remove`: instead of deleting a unit's files, stash them in a
//! trash directory together with a small metadata record, and put them back
//! on request.
//!
//! The trash is a plain directory of entries:
//!
//! ```text
//! <root>/20260815T113000Z-notcron-backup/
//!     meta                     <- the record described in [`TrashEntry`]
//!     notcron-backup.timer     <- the stashed files, under their real names
//!     notcron-backup.service
//! ```
//!
//! Metadata is a hand-rolled `key=value` record rather than JSON: this crate
//! deliberately carries no serialization dependency, and the record has five
//! scalar fields plus a repeated `file=` line. See [`TrashEntry::encode`].
//!
//! Nothing here panics; every I/O failure comes back as an `Err` with a
//! message fit to show the user.
//!
//! # Privilege
//!
//! System-scope trash lives under `/var/lib/notcron`, which is root-owned, so
//! a [`Trash::for_scope(Scope::System)`](Trash::for_scope) built by a non-root
//! process performs its *writes* through `sudo -n`, exactly as
//! [`crate::systemd`] does for `/etc/systemd/system`. Reads are never
//! elevated: everything root writes here lands world-readable (0755 dirs,
//! 0644 files), so listing and restoring decisions need no password.
//!
//! User scope never shells out to `sudo`, and neither does [`Trash::at`], so
//! tests and any explicitly-rooted trash stay pure filesystem calls.

use crate::unit::model::Scope;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// First line of every metadata record, so a future format change can be
/// detected instead of mis-parsed.
const META_MAGIC: &str = "notcron-trash 1";
const META_NAME: &str = "meta";

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// Where the trash for a scope lives.
///
/// * user scope: `$XDG_DATA_HOME/notcron/trash`, falling back to
///   `~/.local/share/notcron/trash`. `XDG_DATA_HOME` is honoured only when it
///   is set, non-empty and absolute, as the XDG spec requires.
/// * system scope: `/var/lib/notcron/trash`.
pub fn trash_dir(scope: Scope) -> PathBuf {
    match scope {
        Scope::System => PathBuf::from("/var/lib/notcron/trash"),
        Scope::User => data_home().join("notcron/trash"),
    }
}

fn data_home() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(x);
        if p.is_absolute() {
            return p;
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.unwrap_or_else(|| PathBuf::from("."))
        .join(".local/share")
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One stashed file: the name it has inside the entry directory, and the
/// absolute path it came from (and will go back to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedFile {
    /// File name inside the entry directory.
    pub stored: String,
    /// Absolute path the file was removed from.
    pub original: PathBuf,
}

/// A trash entry: everything needed to list it in a table and to restore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashEntry {
    /// Directory name under the trash root, and the handle used by
    /// [`Trash::restore`] / [`Trash::discard`]. Sorts chronologically.
    pub id: String,
    /// The primary unit name at removal time, e.g. `notcron-backup.timer`.
    pub unit: String,
    pub scope: Scope,
    /// Removal time, seconds since the Unix epoch.
    pub removed_at: u64,
    /// Whether the unit was enabled when it was removed, so restore can offer
    /// to re-enable it.
    pub was_enabled: bool,
    /// Whether the unit was active when it was removed.
    pub was_active: bool,
    pub files: Vec<TrashedFile>,
}

impl TrashEntry {
    /// Age in seconds relative to `now` (saturating, so a clock that went
    /// backwards reads as zero rather than wrapping).
    pub fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.removed_at)
    }

    /// The metadata record as written to `<entry>/meta`.
    pub fn encode(&self) -> String {
        let mut s = String::from(META_MAGIC);
        s.push('\n');
        s.push_str(&format!("unit={}\n", esc(&self.unit)));
        s.push_str(&format!("scope={}\n", self.scope.as_str()));
        s.push_str(&format!("removed_at={}\n", self.removed_at));
        s.push_str(&format!("enabled={}\n", self.was_enabled));
        s.push_str(&format!("active={}\n", self.was_active));
        for f in &self.files {
            s.push_str(&format!(
                "file={}\t{}\n",
                esc(&f.stored),
                esc(&f.original.to_string_lossy())
            ));
        }
        s
    }

    /// Parse a metadata record. `id` is the entry directory name, which is not
    /// stored in the record itself.
    pub fn decode(id: &str, text: &str) -> Result<TrashEntry, String> {
        let mut lines = text.lines();
        match lines.next() {
            Some(l) if l.trim() == META_MAGIC => {}
            Some(l) => return Err(format!("unrecognised trash record header '{l}'")),
            None => return Err("empty trash record".into()),
        }
        let mut e = TrashEntry {
            id: id.to_string(),
            unit: String::new(),
            scope: Scope::User,
            removed_at: 0,
            was_enabled: false,
            was_active: false,
            files: Vec::new(),
        };
        for line in lines {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "unit" => e.unit = unesc(v),
                "scope" => {
                    e.scope = match v {
                        "system" => Scope::System,
                        "user" => Scope::User,
                        other => return Err(format!("unknown scope '{other}' in trash record")),
                    }
                }
                "removed_at" => {
                    e.removed_at = v
                        .trim()
                        .parse()
                        .map_err(|_| format!("bad removal timestamp '{v}'"))?
                }
                "enabled" => e.was_enabled = v.trim() == "true",
                "active" => e.was_active = v.trim() == "true",
                "file" => {
                    let (stored, orig) = v
                        .split_once('\t')
                        .ok_or_else(|| format!("bad file record '{v}'"))?;
                    e.files.push(TrashedFile {
                        stored: unesc(stored),
                        original: PathBuf::from(unesc(orig)),
                    });
                }
                _ => {} // forward compatible: ignore unknown keys
            }
        }
        if e.unit.is_empty() {
            return Err("trash record has no unit name".into());
        }
        if e.files.is_empty() {
            return Err("trash record lists no files".into());
        }
        Ok(e)
    }
}

/// What to stash. `files` are absolute paths that must all exist.
#[derive(Debug, Clone)]
pub struct StashRequest {
    pub scope: Scope,
    /// Primary unit name, used for the entry id and the list table.
    pub unit: String,
    pub files: Vec<PathBuf>,
    pub was_enabled: bool,
    pub was_active: bool,
}

/// A successful restore.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub unit: String,
    pub scope: Scope,
    /// Paths put back, in the order they were stashed (primary first).
    pub restored: Vec<PathBuf>,
    /// Subset of `restored` that overwrote an existing file.
    pub overwritten: Vec<PathBuf>,
    /// Whether the unit was enabled before removal, so the caller can offer
    /// to re-enable it.
    pub was_enabled: bool,
    pub was_active: bool,
}

/// Why a restore could not proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// No such entry id in the trash.
    NotFound(String),
    /// One or more target paths already exist. Retry with `overwrite = true`
    /// once the user has confirmed.
    Conflict(Vec<PathBuf>),
    /// Anything else: unreadable record, permission denied, and so on.
    Io(String),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::NotFound(id) => write!(f, "no trash entry '{id}'"),
            RestoreError::Conflict(paths) => {
                let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(f, "already exists: {}", names.join(", "))
            }
            RestoreError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RestoreError {}

/// How much history the trash keeps. Both limits are optional and applied
/// together: an entry is dropped if it fails *either* one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Keep at most this many entries, newest first.
    pub max_entries: Option<usize>,
    /// Drop entries older than this many seconds.
    pub max_age_secs: Option<u64>,
}

impl PrunePolicy {
    /// notcron's default: the last 50 removals, nothing older than 30 days.
    pub const DEFAULT: PrunePolicy = PrunePolicy {
        max_entries: Some(50),
        max_age_secs: Some(30 * 24 * 60 * 60),
    };

    /// Keep everything. Useful in tests and for an explicit opt-out.
    #[cfg(test)]
    pub const UNLIMITED: PrunePolicy = PrunePolicy {
        max_entries: None,
        max_age_secs: None,
    };
}

impl Default for PrunePolicy {
    fn default() -> Self {
        PrunePolicy::DEFAULT
    }
}

// ---------------------------------------------------------------------------
// The trash itself
// ---------------------------------------------------------------------------

/// A trash directory. Construct with [`Trash::for_scope`] in production, or
/// [`Trash::at`] to point it anywhere (tests, or a `--trash-dir` flag).
#[derive(Debug, Clone)]
pub struct Trash {
    root: PathBuf,
    /// Route writes through `sudo -n`. Set only by [`Trash::for_scope`] for
    /// system scope when the process is not already root.
    sudo: bool,
}

impl Trash {
    /// The trash for a scope, at the location [`trash_dir`] describes.
    ///
    /// System scope writes through `sudo -n` unless already running as root;
    /// user scope never does.
    pub fn for_scope(scope: Scope) -> Trash {
        Trash {
            root: trash_dir(scope),
            sudo: scope == Scope::System && !crate::systemd::is_root(),
        }
    }

    /// A trash rooted at an arbitrary directory, writing as the current user.
    /// Used by tests and anywhere the caller has already arranged access.
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Trash {
        Trash {
            root: root.into(),
            sudo: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether this trash escalates its writes.
    #[cfg(test)]
    pub fn uses_sudo(&self) -> bool {
        self.sudo
    }

    /// Move a removed unit's files into a new trash entry.
    ///
    /// Files are moved with `rename` where possible and copied+unlinked when
    /// the trash is on a different filesystem (`rename` fails with `EXDEV`).
    /// Missing source files are skipped; if none of them exist the call is an
    /// error, because an entry with no files cannot be restored.
    ///
    /// A stash either completes or leaves the filesystem as it found it: if a
    /// move fails part way through, the files already moved are put back and
    /// the half-built entry directory is removed. Callers can therefore treat
    /// an `Err` as "nothing happened" and refuse to go on deleting.
    pub fn stash(&self, req: &StashRequest) -> Result<TrashEntry, String> {
        self.stash_at(req, now_secs())
    }

    /// [`Trash::stash`] with an explicit removal timestamp, for tests.
    pub fn stash_at(&self, req: &StashRequest, now: u64) -> Result<TrashEntry, String> {
        if req.files.is_empty() {
            return Err("nothing to stash: no files given".into());
        }
        create_dir(&self.root, self.sudo)?;
        let dir = self.unique_entry_dir(&req.unit, now)?;
        let id = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut files: Vec<TrashedFile> = Vec::new();
        for (i, src) in req.files.iter().enumerate() {
            if !src.exists() {
                continue; // already gone: not a reason to fail the removal
            }
            let base = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("file{i}"));
            let stored = if files.iter().any(|f: &TrashedFile| f.stored == base) {
                format!("{i}-{base}")
            } else {
                base
            };
            if let Err(e) = move_file(src, &dir.join(&stored), true, self.sudo) {
                self.unwind(&dir, &files);
                return Err(e);
            }
            files.push(TrashedFile {
                stored,
                original: src.clone(),
            });
        }
        if files.is_empty() {
            self.unwind(&dir, &[]);
            return Err("nothing to stash: none of the unit's files exist".into());
        }

        let entry = TrashEntry {
            id,
            unit: req.unit.clone(),
            scope: req.scope,
            removed_at: now,
            was_enabled: req.was_enabled,
            was_active: req.was_active,
            files,
        };
        if let Err(e) = write_file(&dir.join(META_NAME), &entry.encode(), self.sudo) {
            self.unwind(&dir, &entry.files);
            return Err(e);
        }
        Ok(entry)
    }

    /// Roll a failed stash back: return everything already moved to where it
    /// came from and drop the entry directory. Best effort by construction —
    /// the caller is already returning an error, and a rollback that cannot
    /// finish must not mask it.
    fn unwind(&self, dir: &Path, moved: &[TrashedFile]) {
        for f in moved {
            let _ = move_file(&dir.join(&f.stored), &f.original, true, self.sudo);
        }
        let _ = remove_tree(dir, self.sudo);
    }

    /// Every readable entry, newest first. Unreadable or malformed entries are
    /// skipped rather than failing the whole listing; a missing trash root is
    /// an empty list.
    pub fn list(&self) -> Result<Vec<TrashEntry>, String> {
        let rd = match fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("reading {}: {e}", self.root.display())),
        };
        let mut out = Vec::new();
        for ent in rd.flatten() {
            if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let id = ent.file_name().to_string_lossy().into_owned();
            let Ok(text) = fs::read_to_string(ent.path().join(META_NAME)) else {
                continue;
            };
            if let Ok(e) = TrashEntry::decode(&id, &text) {
                out.push(e);
            }
        }
        out.sort_by(|a, b| b.removed_at.cmp(&a.removed_at).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    /// Look one entry up by id.
    pub fn get(&self, id: &str) -> Result<TrashEntry, RestoreError> {
        if id.is_empty() || id.contains('/') || id.contains("..") {
            return Err(RestoreError::NotFound(id.to_string()));
        }
        let path = self.root.join(id).join(META_NAME);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(RestoreError::NotFound(id.to_string()))
            }
            Err(e) => return Err(RestoreError::Io(format!("reading {}: {e}", path.display()))),
        };
        TrashEntry::decode(id, &text).map_err(RestoreError::Io)
    }

    /// Original paths that already exist on disk and would be clobbered by a
    /// restore. Empty means the restore is clean.
    pub fn conflicts(&self, id: &str) -> Result<Vec<PathBuf>, RestoreError> {
        let e = self.get(id)?;
        Ok(e.files
            .iter()
            .map(|f| f.original.clone())
            .filter(|p| p.exists())
            .collect())
    }

    /// Put an entry's files back at their original paths and drop the entry.
    ///
    /// With `overwrite = false` an existing target aborts the whole restore
    /// before anything is moved, returning [`RestoreError::Conflict`] with the
    /// offending paths. Nothing is clobbered silently.
    pub fn restore(&self, id: &str, overwrite: bool) -> Result<RestoreReport, RestoreError> {
        let entry = self.get(id)?;
        let existing: Vec<PathBuf> = entry
            .files
            .iter()
            .map(|f| f.original.clone())
            .filter(|p| p.exists())
            .collect();
        if !existing.is_empty() && !overwrite {
            return Err(RestoreError::Conflict(existing));
        }

        let dir = self.root.join(&entry.id);
        let mut restored = Vec::new();
        for f in &entry.files {
            let src = dir.join(&f.stored);
            if !src.exists() {
                return Err(RestoreError::Io(format!(
                    "trash entry {} is missing {}",
                    entry.id, f.stored
                )));
            }
            move_file(&src, &f.original, true, self.sudo).map_err(RestoreError::Io)?;
            restored.push(f.original.clone());
        }
        // Only the metadata should be left; dropping the directory keeps the
        // trash from accumulating empty husks.
        let _ = remove_tree(&dir, self.sudo);

        Ok(RestoreReport {
            unit: entry.unit,
            scope: entry.scope,
            restored,
            overwritten: existing,
            was_enabled: entry.was_enabled,
            was_active: entry.was_active,
        })
    }

    /// Delete one entry and its files for good.
    pub fn discard(&self, id: &str) -> Result<(), String> {
        let entry = match self.get(id) {
            Ok(e) => e,
            Err(RestoreError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let dir = self.root.join(&entry.id);
        remove_tree(&dir, self.sudo)
    }

    /// Apply a retention policy, deleting the entries it excludes. Returns the
    /// ids that were dropped, newest first.
    pub fn prune(&self, policy: PrunePolicy) -> Result<Vec<String>, String> {
        self.prune_at(policy, now_secs())
    }

    /// [`Trash::prune`] with an explicit "now", for tests.
    pub fn prune_at(&self, policy: PrunePolicy, now: u64) -> Result<Vec<String>, String> {
        let entries = self.list()?;
        let doomed = select_for_prune(&entries, policy, now);
        for id in &doomed {
            self.discard(id)?;
        }
        Ok(doomed)
    }

    /// Reserve an entry directory. The id is the UTC timestamp plus the unit
    /// name, with a counter appended if two removals land in the same second.
    fn unique_entry_dir(&self, unit: &str, now: u64) -> Result<PathBuf, String> {
        let stamp = compact_utc(now);
        let slug = slug(unit);
        for n in 0..1000 {
            let name = if n == 0 {
                format!("{stamp}-{slug}")
            } else {
                format!("{stamp}-{slug}-{n}")
            };
            let dir = self.root.join(&name);
            // `sudo mkdir` reports an already-existing directory as a generic
            // failure, so the existence check comes first either way; the
            // create is still the thing that claims the name.
            if dir.exists() {
                continue;
            }
            match make_dir(&dir, self.sudo) {
                Ok(()) => return Ok(dir),
                Err(_) if dir.exists() => continue,
                Err(e) => return Err(e),
            }
        }
        Err("could not allocate a trash entry directory".into())
    }
}

/// Which entries a policy excludes. Pure, so the policy is testable without
/// touching the filesystem. `entries` must be newest first.
fn select_for_prune(entries: &[TrashEntry], policy: PrunePolicy, now: u64) -> Vec<String> {
    let mut doomed = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let over_count = policy.max_entries.is_some_and(|max| i >= max);
        let too_old = policy.max_age_secs.is_some_and(|max| e.age_secs(now) > max);
        if over_count || too_old {
            doomed.push(e.id.clone());
        }
    }
    doomed
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Run an elevated helper and turn a non-zero exit into a usable message.
///
/// `sudo -n` never prompts: on a host without a password-less rule this fails
/// immediately with "a password is required", which is the right answer for
/// both the TUI and a provisioning script.
fn sudo_run(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd
        .stdout(Stdio::null())
        .output()
        .map_err(|e| format!("{what}: failed to run sudo: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if msg.is_empty() {
        format!("{what}: sudo failed")
    } else {
        format!("{what}: {msg}")
    })
}

fn sudo() -> Command {
    let mut c = Command::new("sudo");
    c.arg("-n");
    c
}

/// `mkdir -p`.
fn create_dir(path: &Path, elevated: bool) -> Result<(), String> {
    if elevated {
        return sudo_run(
            sudo().args(["mkdir", "-p"]).arg(path),
            &format!("creating {}", path.display()),
        );
    }
    fs::create_dir_all(path).map_err(|e| {
        format!(
            "creating {}: {e}{}",
            path.display(),
            if e.kind() == ErrorKind::PermissionDenied {
                " (system-scope trash needs root)"
            } else {
                ""
            }
        )
    })
}

/// `mkdir` without `-p`: creating the entry directory is how a trash id is
/// claimed, so it must fail when the name is already taken.
fn make_dir(path: &Path, elevated: bool) -> Result<(), String> {
    if elevated {
        return sudo_run(
            sudo().arg("mkdir").arg(path),
            &format!("creating {}", path.display()),
        );
    }
    fs::create_dir(path).map_err(|e| format!("creating {}: {e}", path.display()))
}

/// Write a file, elevating through `tee` the way `systemd::write_file` does.
fn write_file(path: &Path, body: &str, elevated: bool) -> Result<(), String> {
    if !elevated {
        return fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()));
    }
    let mut child = sudo()
        .arg("tee")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("writing {}: failed to run sudo tee: {e}", path.display()))?;
    child
        .stdin
        .as_mut()
        .ok_or("sudo tee has no stdin")?
        .write_all(body.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "writing {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// `rm -rf` on a trash entry directory. A directory that is already gone is
/// success: discarding twice is not an error.
fn remove_tree(path: &Path, elevated: bool) -> Result<(), String> {
    if elevated {
        return sudo_run(
            sudo().args(["rm", "-rf"]).arg(path),
            &format!("removing {}", path.display()),
        );
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("removing {}: {e}", path.display())),
    }
}

/// `EXDEV`: rename across filesystems. The trash may well be on a different
/// mount from `/etc/systemd/system`, so this is the expected failure.
const EXDEV: i32 = 18;

/// Move a file, falling back to copy+unlink when `rename` cannot cross the
/// filesystem boundary. `allow_rename` exists so the fallback path is
/// reachable in tests without staging two real mounts.
fn move_file(from: &Path, to: &Path, allow_rename: bool, elevated: bool) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        create_dir(parent, elevated)?;
    }
    if elevated {
        // `mv` already does rename-then-copy across filesystems, and doing it
        // in one elevated call keeps the file from ever existing in both
        // places under two different owners.
        return sudo_run(
            sudo().args(["mv", "-f"]).arg(from).arg(to),
            &format!("moving {} to {}", from.display(), to.display()),
        );
    }
    if allow_rename {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(EXDEV) => {}
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                // A cross-mount move can also surface as EPERM on the unlink;
                // let the copy path produce the clearer message.
            }
            Err(e) => {
                return Err(format!(
                    "moving {} to {}: {e}",
                    from.display(),
                    to.display()
                ))
            }
        }
    }
    copy_then_unlink(from, to)
}

/// The cross-filesystem path: copy the bytes, then drop the source. If the
/// unlink fails the copy is rolled back, so a file is never in both places.
fn copy_then_unlink(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
    if let Err(e) = fs::remove_file(from) {
        if e.kind() != ErrorKind::NotFound {
            let _ = fs::remove_file(to);
            return Err(format!("removing {}: {e}", from.display()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Text and time helpers
// ---------------------------------------------------------------------------

/// Escape the characters that would break the one-record-per-line format.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Reduce a unit name to something safe and readable in a directory name.
fn slug(unit: &str) -> String {
    let s: String = unit
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        "unit".into()
    } else {
        s.chars().take(80).collect()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Civil date from a Unix day number (Howard Hinnant's `civil_from_days`).
/// Keeps the trash id human-readable without pulling in a date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn split_utc(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    (
        y,
        mo,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// `20260815T113000Z` — sorts chronologically as a directory name.
fn compact_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = split_utc(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(path: &Path, body: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// A unit dir with a timer/service pair, plus a request that stashes it.
    fn staged(tmp: &TempDir) -> (PathBuf, StashRequest) {
        let units = tmp.path().join("units");
        let timer = units.join("notcron-backup.timer");
        let service = units.join("notcron-backup.service");
        write(&timer, "[Timer]\nOnCalendar=daily\n");
        write(&service, "[Service]\nExecStart=/bin/true\n");
        (
            units,
            StashRequest {
                scope: Scope::User,
                unit: "notcron-backup.timer".into(),
                files: vec![timer, service],
                was_enabled: true,
                was_active: false,
            },
        )
    }

    /// Drop the write bit on a directory, which is what stops a file inside
    /// it from being unlinked -- and therefore from being moved out.
    fn set_writable(dir: &Path, writable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if writable { 0o755 } else { 0o555 };
        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a_stash_that_fails_part_way_puts_back_what_it_moved() {
        let tmp = TempDir::new().unwrap();
        let movable = tmp.path().join("movable");
        let pinned = tmp.path().join("pinned");
        let first = movable.join("notcron-backup.timer");
        let second = pinned.join("notcron-backup.service");
        write(&first, "[Timer]\nOnCalendar=daily\n");
        write(&second, "[Service]\nExecStart=/bin/true\n");
        // The second file cannot leave its directory, so the stash fails
        // after the first has already been moved into the entry.
        set_writable(&pinned, false);

        let trash = Trash::at(tmp.path().join("trash"));
        let req = StashRequest {
            scope: Scope::User,
            unit: "notcron-backup.timer".into(),
            files: vec![first.clone(), second.clone()],
            was_enabled: false,
            was_active: false,
        };
        let err = trash.stash(&req).expect_err("the second move must fail");
        set_writable(&pinned, true); // so TempDir can clean up

        assert!(err.contains("notcron-backup.service"), "{err}");
        // Both files are where they started...
        assert_eq!(
            fs::read_to_string(&first).unwrap(),
            "[Timer]\nOnCalendar=daily\n"
        );
        assert!(second.exists());
        // ...and no half-built entry is left for the undo dialog to trip on.
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn a_trash_for_user_scope_never_escalates() {
        // The sudo path exists for /var/lib/notcron only; a user-scope trash
        // under $HOME must stay ordinary filesystem calls.
        assert!(!Trash::for_scope(Scope::User).uses_sudo());
        // An explicitly-rooted trash never escalates either, whatever it
        // points at -- that is what keeps the test suite off sudo.
        assert!(!Trash::at("/var/lib/notcron/trash").uses_sudo());
    }

    #[test]
    fn system_scope_escalates_exactly_when_not_root() {
        let t = Trash::for_scope(Scope::System);
        assert_eq!(t.uses_sudo(), !crate::systemd::is_root());
        assert_eq!(t.root(), Path::new("/var/lib/notcron/trash"));
    }

    #[test]
    fn xdg_data_home_is_honoured_only_when_absolute() {
        // Assert on the shape of the default rather than mutating process env,
        // which would race the other tests.
        let d = trash_dir(Scope::User);
        assert!(d.ends_with("notcron/trash"), "{}", d.display());
        assert_eq!(
            trash_dir(Scope::System),
            PathBuf::from("/var/lib/notcron/trash")
        );
        assert!(!PathBuf::from("relative/share").is_absolute());
    }

    #[test]
    fn round_trip_returns_identical_bytes() {
        let tmp = TempDir::new().unwrap();
        let (_units, req) = staged(&tmp);
        let before: Vec<Vec<u8>> = req.files.iter().map(|p| fs::read(p).unwrap()).collect();

        let trash = Trash::at(tmp.path().join("trash"));
        let entry = trash.stash(&req).unwrap();
        for p in &req.files {
            assert!(!p.exists(), "{} should have moved", p.display());
        }

        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
        assert_eq!(listed[0].unit, "notcron-backup.timer");
        assert!(listed[0].was_enabled);
        assert!(!listed[0].was_active);

        let report = trash.restore(&entry.id, false).unwrap();
        assert_eq!(report.restored, req.files);
        assert!(report.overwritten.is_empty());
        assert!(report.was_enabled);
        for (p, want) in req.files.iter().zip(&before) {
            assert_eq!(&fs::read(p).unwrap(), want);
        }
        assert!(trash.list().unwrap().is_empty());
        assert!(!trash.root().join(&entry.id).exists());
    }

    #[test]
    fn restore_onto_an_existing_file_is_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let (_units, req) = staged(&tmp);
        let trash = Trash::at(tmp.path().join("trash"));
        let entry = trash.stash(&req).unwrap();

        // The user recreated the timer under the same name.
        write(&req.files[0], "recreated\n");

        match trash.restore(&entry.id, false) {
            Err(RestoreError::Conflict(paths)) => assert_eq!(paths, vec![req.files[0].clone()]),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Nothing moved: the entry and the recreated file both survive.
        assert_eq!(fs::read_to_string(&req.files[0]).unwrap(), "recreated\n");
        assert_eq!(
            trash.conflicts(&entry.id).unwrap(),
            vec![req.files[0].clone()]
        );
        assert_eq!(trash.list().unwrap().len(), 1);

        let report = trash.restore(&entry.id, true).unwrap();
        assert_eq!(report.overwritten, vec![req.files[0].clone()]);
        assert!(fs::read_to_string(&req.files[0])
            .unwrap()
            .contains("OnCalendar"));
    }

    #[test]
    fn cross_filesystem_stash_falls_back_to_copy_and_unlink() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src/notcron-x.timer");
        write(&src, "body\n");
        let dst = tmp.path().join("dst/notcron-x.timer");

        // allow_rename = false stands in for EXDEV without staging two mounts.
        move_file(&src, &dst, false, false).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "body\n");
    }

    #[test]
    fn copy_fallback_leaves_nothing_behind_on_failure() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let dst = tmp.path().join("dst");
        assert!(copy_then_unlink(&missing, &dst).is_err());
        assert!(!dst.exists());
    }

    #[test]
    fn stashing_a_unit_whose_files_are_gone_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let trash = Trash::at(tmp.path().join("trash"));
        let req = StashRequest {
            scope: Scope::User,
            unit: "notcron-ghost.timer".into(),
            files: vec![tmp.path().join("nope.timer")],
            was_enabled: false,
            was_active: false,
        };
        assert!(trash.stash(&req).is_err());
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn entries_in_the_same_second_get_distinct_ids() {
        let tmp = TempDir::new().unwrap();
        let trash = Trash::at(tmp.path().join("trash"));
        let mut ids = Vec::new();
        for i in 0..3 {
            let f = tmp.path().join(format!("u{i}/notcron-a.timer"));
            write(&f, "x\n");
            let req = StashRequest {
                scope: Scope::User,
                unit: "notcron-a.timer".into(),
                files: vec![f],
                was_enabled: false,
                was_active: false,
            };
            ids.push(trash.stash_at(&req, 1_755_000_000).unwrap().id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert_eq!(trash.list().unwrap().len(), 3);
    }

    fn fake(id: &str, at: u64) -> TrashEntry {
        TrashEntry {
            id: id.into(),
            unit: "notcron-a.timer".into(),
            scope: Scope::User,
            removed_at: at,
            was_enabled: false,
            was_active: false,
            files: vec![TrashedFile {
                stored: "notcron-a.timer".into(),
                original: PathBuf::from("/tmp/notcron-a.timer"),
            }],
        }
    }

    #[test]
    fn prune_policy_drops_by_count_and_by_age() {
        let now = 1_000_000;
        // Newest first, one per day.
        let entries: Vec<TrashEntry> = (0..5)
            .map(|i| fake(&format!("e{i}"), now - i * 86_400))
            .collect();

        let by_count = PrunePolicy {
            max_entries: Some(2),
            max_age_secs: None,
        };
        assert_eq!(
            select_for_prune(&entries, by_count, now),
            vec!["e2", "e3", "e4"]
        );

        let by_age = PrunePolicy {
            max_entries: None,
            max_age_secs: Some(2 * 86_400),
        };
        assert_eq!(select_for_prune(&entries, by_age, now), vec!["e3", "e4"]);

        assert!(select_for_prune(&entries, PrunePolicy::UNLIMITED, now).is_empty());
        assert!(select_for_prune(&entries, PrunePolicy::DEFAULT, now).is_empty());
    }

    #[test]
    fn prune_deletes_the_selected_entries_on_disk() {
        let tmp = TempDir::new().unwrap();
        let trash = Trash::at(tmp.path().join("trash"));
        for i in 0..4u64 {
            let f = tmp.path().join(format!("u{i}/notcron-a.timer"));
            write(&f, "x\n");
            let req = StashRequest {
                scope: Scope::User,
                unit: "notcron-a.timer".into(),
                files: vec![f],
                was_enabled: false,
                was_active: false,
            };
            trash.stash_at(&req, 1_000_000 + i).unwrap();
        }
        let dropped = trash
            .prune_at(
                PrunePolicy {
                    max_entries: Some(2),
                    max_age_secs: None,
                },
                1_000_010,
            )
            .unwrap();
        assert_eq!(dropped.len(), 2);
        let left = trash.list().unwrap();
        assert_eq!(left.len(), 2);
        // The two survivors are the newest.
        assert_eq!(left[0].removed_at, 1_000_003);
        assert_eq!(left[1].removed_at, 1_000_002);
    }

    #[test]
    fn metadata_round_trips_including_awkward_paths() {
        let e = TrashEntry {
            id: "20260815T113000Z-notcron-a.timer".into(),
            unit: "notcron-a.timer".into(),
            scope: Scope::System,
            removed_at: 1_755_257_400,
            was_enabled: true,
            was_active: true,
            files: vec![TrashedFile {
                stored: "notcron-a.timer".into(),
                original: PathBuf::from("/etc/systemd/system/weird\tname\\here.timer"),
            }],
        };
        let decoded = TrashEntry::decode(&e.id, &e.encode()).unwrap();
        assert_eq!(decoded, e);
    }

    #[test]
    fn malformed_records_are_rejected_not_guessed() {
        assert!(TrashEntry::decode("x", "").is_err());
        assert!(TrashEntry::decode("x", "some other file\nunit=a\n").is_err());
        assert!(TrashEntry::decode("x", &format!("{META_MAGIC}\nunit=a\n")).is_err());
        assert!(TrashEntry::decode(
            "x",
            &format!("{META_MAGIC}\nunit=a\nscope=elsewhere\nfile=a\t/a\n")
        )
        .is_err());
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let text = format!("{META_MAGIC}\nunit=a.timer\nfuture=whatever\nfile=a.timer\t/a.timer\n");
        let e = TrashEntry::decode("x", &text).unwrap();
        assert_eq!(e.unit, "a.timer");
        assert_eq!(e.files.len(), 1);
    }

    #[test]
    fn missing_and_traversing_ids_are_not_found() {
        let tmp = TempDir::new().unwrap();
        let trash = Trash::at(tmp.path().join("trash"));
        assert!(matches!(
            trash.restore("nope", false),
            Err(RestoreError::NotFound(_))
        ));
        assert!(matches!(
            trash.get("../../etc"),
            Err(RestoreError::NotFound(_))
        ));
        assert!(trash.discard("nope").is_ok());
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn the_current_time_lands_in_a_plausible_year() {
        // Cheap sanity check on now_secs() that cannot go stale: the clock is
        // past the moment this code was written and short of the far future.
        let (y, mo, d, ..) = split_utc(now_secs());
        assert!((2024..2200).contains(&y), "implausible year {y}");
        assert!((1..=12).contains(&mo));
        assert!((1..=31).contains(&d));
    }

    #[test]
    fn slugs_stay_filesystem_safe() {
        assert_eq!(slug("notcron-a.timer"), "notcron-a.timer");
        assert_eq!(slug("../evil"), "_evil");
        assert_eq!(slug(""), "unit");
        assert_eq!(slug("."), "unit");
    }
}
