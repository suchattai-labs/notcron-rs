//! The per-field help copy, read from `docs/field-help.md`.
//!
//! The markdown file is the single source of truth: it is embedded at compile
//! time and parsed once on first use, rather than being duplicated as string
//! literals next to each widget. That way the text a user reads in the TUI and
//! the text a reviewer reads in the repo cannot drift apart.
//!
//! The shape parsed is the one the document declares for itself:
//!
//! ```text
//! ### <key>
//! **Label:** <label>
//! **Summary:** <one line>
//! **Detail:** <a few sentences>
//! **Examples:** <comma-separated>
//! ```
//!
//! Anything else in the file -- prose, headings, fences -- is ignored.

use std::sync::OnceLock;

const SOURCE: &str = include_str!("../docs/field-help.md");

/// One `###` block of the help document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub label: String,
    pub summary: String,
    pub detail: String,
    pub examples: String,
}

impl Entry {
    /// The first of the comma-separated examples, which is the one written to
    /// be a usable default. A trailing parenthetical note (`(fstab only)`) is
    /// stripped, since it is commentary rather than part of the value.
    pub fn first_example(&self) -> &str {
        let first = self
            .examples
            .split(',')
            .next()
            .unwrap_or(&self.examples)
            .trim();
        match first.split_once(" (") {
            Some((v, _)) => v.trim(),
            None => first,
        }
    }
}

fn parse(src: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if let Some(key) = t.strip_prefix("### ") {
            out.push(Entry {
                key: key.trim().to_string(),
                label: String::new(),
                summary: String::new(),
                detail: String::new(),
                examples: String::new(),
            });
            continue;
        }
        let Some(cur) = out.last_mut() else { continue };
        if let Some(rest) = t.strip_prefix("**Label:**") {
            cur.label = rest.trim().to_string();
        } else if let Some(rest) = t.strip_prefix("**Summary:**") {
            cur.summary = rest.trim().to_string();
        } else if let Some(rest) = t.strip_prefix("**Detail:**") {
            cur.detail = rest.trim().to_string();
        } else if let Some(rest) = t.strip_prefix("**Examples:**") {
            cur.examples = rest.trim().to_string();
        }
    }
    out
}

fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| parse(SOURCE))
}

/// The help block for `key`, or `None` if the document has no such entry.
pub fn entry(key: &str) -> Option<&'static Entry> {
    entries().iter().find(|e| e.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_parses_into_complete_entries() {
        let all = entries();
        assert!(all.len() > 30, "only {} entries parsed", all.len());
        for e in all {
            assert!(!e.label.is_empty(), "{} has no label", e.key);
            assert!(!e.summary.is_empty(), "{} has no summary", e.key);
            assert!(!e.detail.is_empty(), "{} has no detail", e.key);
        }
    }

    #[test]
    fn keys_are_unique() {
        let all = entries();
        for (i, e) in all.iter().enumerate() {
            assert!(
                !all[..i].iter().any(|p| p.key == e.key),
                "duplicate help key {}",
                e.key
            );
        }
    }

    #[test]
    fn a_known_entry_reads_back() {
        let e = entry("mount.opt.nfs.timeo").expect("nfs timeo help");
        assert_eq!(e.label, "timeo= *(value)*");
        assert!(e.summary.contains("deciseconds"));
        assert_eq!(e.first_example(), "timeo=600");
    }

    #[test]
    fn parenthetical_notes_are_not_part_of_an_example() {
        let e = entry("mount.opt.x_systemd_automount").expect("automount help");
        assert_eq!(e.first_example(), "x-systemd.automount");
    }

    #[test]
    fn unknown_keys_are_absent_rather_than_fatal() {
        assert!(entry("mount.opt.nope").is_none());
    }
}
