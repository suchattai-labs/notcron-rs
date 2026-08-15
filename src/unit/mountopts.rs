//! `Options=` of a `.mount` unit, as a structured, editable list.
//!
//! The builder shows a toggle menu instead of a free-text field, but the
//! option string on disk stays the authority: this module parses it into an
//! ordered list, lets the menu flip entries on and off, and composes it back.
//! Nothing here renders anything, so all of it is unit-testable.
//!
//! Two properties matter more than the menu itself:
//!
//! * **Nothing is dropped.** Only a subset of mount options is worth offering
//!   as toggles; every other option a unit already carries survives verbatim,
//!   in its original position, reachable through a free-text "other options"
//!   entry. This is the same bargain the `notcron:manual` block strikes for
//!   directives the model does not understand.
//! * **Order is preserved.** Mount options are last-wins, so reordering them
//!   can change what the kernel does. Toggling an option off and on again
//!   appends it; it never rewrites the list.
//!
//! Deliberately *not* offered: `x-systemd.automount`, `x-systemd.requires=`,
//! `x-systemd.device-timeout=`, `x-systemd.mount-timeout=`,
//! `x-systemd.makefs` and `x-systemd.growfs`. Those are read by
//! `systemd-fstab-generator`, which is not involved in a hand-written unit
//! file; `systemd.mount(5)` documents several of them as fstab-only, and
//! probing this host confirmed that a unit carrying `x-systemd.automount`
//! generates no automount unit and one carrying `x-systemd.requires=` gains
//! no `Requires=`. Offering them would promise behaviour that does not
//! happen. They still parse back into the free-text entry, so opening someone
//! else's unit and saving it keeps them.
//!
//! `nofail` and `_netdev` were probed the same way and *are* honoured in a
//! unit file, so both stay on the menu.

use crate::fieldhelp;

/// One comma-separated element of an `Options=` string.
///
/// `value` distinguishes `vers=4.2` (`Some("4.2")`) from a bare `ro`
/// (`None`), and keeps `foo=` (`Some("")`) distinct from `foo`, so composing
/// a parsed list reproduces it byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opt {
    pub key: String,
    pub value: Option<String>,
}

impl Opt {
    pub fn flag(key: &str) -> Opt {
        Opt {
            key: key.to_string(),
            value: None,
        }
    }

    pub fn valued(key: &str, value: &str) -> Opt {
        Opt {
            key: key.to_string(),
            value: Some(value.to_string()),
        }
    }

    /// The option as it appears in the string.
    pub fn text(&self) -> String {
        match &self.value {
            Some(v) => format!("{}={}", self.key, v),
            None => self.key.clone(),
        }
    }
}

/// Split an `Options=` string into its elements, in order.
///
/// Empty elements are dropped (`a,,b` is `a,b`) and surrounding whitespace is
/// trimmed, since neither survives a trip through `mount(8)` anyway.
pub fn parse_options(s: &str) -> Vec<Opt> {
    s.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| match e.split_once('=') {
            Some((k, v)) => Opt::valued(k.trim(), v.trim()),
            None => Opt::flag(e),
        })
        .collect()
}

/// Join options back into an `Options=` string.
pub fn compose(opts: &[Opt]) -> String {
    opts.iter().map(Opt::text).collect::<Vec<_>>().join(",")
}

/// Which extra options a filesystem takes on top of the generic set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Block devices and anything else without a dedicated option set.
    Generic,
    Nfs,
    Cifs,
    Bind,
}

/// The family implied by a `Type=` value.
///
/// Driven by the fstype rather than the builder's preset, because the preset
/// only seeds the field and the user is free to type something else
/// afterwards. `none` is how a bind mount spells its type.
pub fn family_for(fstype: &str) -> Family {
    match fstype.trim().to_ascii_lowercase().as_str() {
        "nfs" | "nfs4" => Family::Nfs,
        "cifs" | "smb3" | "smb" => Family::Cifs,
        "none" => Family::Bind,
        _ => Family::Generic,
    }
}

/// Whether an option carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A bare word: `ro`, `noatime`.
    Flag,
    /// `key=value`: `vers=4.2`.
    Value,
}

/// One option the menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub key: &'static str,
    /// Key into `docs/field-help.md`; the label, summary and example value
    /// all come from there rather than being restated here.
    pub help: &'static str,
    pub kind: Kind,
    /// Options sharing a group contradict each other, so the menu treats them
    /// as one setting with several positions rather than as independent
    /// checkboxes: turning one on turns its siblings off, in place.
    pub group: Option<&'static str>,
    /// `None` means every filesystem offers it.
    pub family: Option<Family>,
    /// The value is a path, so the file picker is the natural way to fill it.
    pub path: bool,
}

const fn flag(key: &'static str, help: &'static str) -> Spec {
    Spec {
        key,
        help,
        kind: Kind::Flag,
        group: None,
        family: None,
        path: false,
    }
}

const fn value(key: &'static str, help: &'static str) -> Spec {
    Spec {
        key,
        help,
        kind: Kind::Value,
        group: None,
        family: None,
        path: false,
    }
}

const fn in_group(s: Spec, group: &'static str) -> Spec {
    Spec {
        group: Some(group),
        ..s
    }
}

const fn of(s: Spec, family: Family) -> Spec {
    Spec {
        family: Some(family),
        ..s
    }
}

/// Options offered whatever the filesystem is.
///
/// `ro`/`rw` and `noatime`/`relatime` are exclusive pairs: each names an
/// either-or setting, and listing both halves would only ever produce a
/// string whose meaning depends on which came last.
pub const GENERIC: &[Spec] = &[
    in_group(flag("ro", "mount.opt.ro"), "access"),
    in_group(flag("rw", "mount.opt.rw"), "access"),
    in_group(flag("noatime", "mount.opt.noatime"), "atime"),
    in_group(flag("relatime", "mount.opt.relatime"), "atime"),
    flag("nofail", "mount.opt.nofail"),
    flag("noexec", "mount.opt.noexec"),
    flag("nosuid", "mount.opt.nosuid"),
    flag("nodev", "mount.opt.nodev"),
    flag("defaults", "mount.opt.defaults"),
    flag("_netdev", "mount.opt._netdev"),
];

pub const NFS: &[Spec] = &[
    of(value("vers", "mount.opt.nfs.vers"), Family::Nfs),
    of(
        in_group(flag("hard", "mount.opt.nfs.hard"), "nfs-timeout"),
        Family::Nfs,
    ),
    of(
        in_group(flag("soft", "mount.opt.nfs.soft"), "nfs-timeout"),
        Family::Nfs,
    ),
    of(value("timeo", "mount.opt.nfs.timeo"), Family::Nfs),
    of(value("retrans", "mount.opt.nfs.retrans"), Family::Nfs),
    of(value("rsize", "mount.opt.nfs.rsize"), Family::Nfs),
    of(value("wsize", "mount.opt.nfs.wsize"), Family::Nfs),
    of(
        in_group(flag("bg", "mount.opt.nfs.bg"), "nfs-fork"),
        Family::Nfs,
    ),
    of(
        in_group(flag("fg", "mount.opt.nfs.fg"), "nfs-fork"),
        Family::Nfs,
    ),
];

pub const CIFS: &[Spec] = &[
    of(
        Spec {
            path: true,
            ..value("credentials", "mount.opt.cifs.credentials")
        },
        Family::Cifs,
    ),
    of(value("username", "mount.opt.cifs.username"), Family::Cifs),
    of(value("uid", "mount.opt.cifs.uid"), Family::Cifs),
    of(value("gid", "mount.opt.cifs.gid"), Family::Cifs),
    of(value("vers", "mount.opt.cifs.vers"), Family::Cifs),
    of(value("iocharset", "mount.opt.cifs.iocharset"), Family::Cifs),
];

pub const BIND: &[Spec] = &[
    of(
        in_group(flag("bind", "mount.opt.bind"), "bind"),
        Family::Bind,
    ),
    of(
        in_group(flag("rbind", "mount.opt.rbind"), "bind"),
        Family::Bind,
    ),
];

impl Family {
    /// The heading the family-specific block gets in the menu.
    pub fn label(self) -> &'static str {
        match self {
            Family::Generic => "Generic",
            Family::Nfs => "NFS",
            Family::Cifs => "SMB/CIFS",
            Family::Bind => "Bind mount",
        }
    }

    /// The options this family adds on top of [`GENERIC`].
    pub fn extras(self) -> &'static [Spec] {
        match self {
            Family::Generic => &[],
            Family::Nfs => NFS,
            Family::Cifs => CIFS,
            Family::Bind => BIND,
        }
    }
}

/// Every option offered for `family`: the generic set, then its extras.
pub fn offered(family: Family) -> Vec<&'static Spec> {
    GENERIC.iter().chain(family.extras()).collect()
}

/// The spec for `key` within `family`, if it is offered there.
///
/// `vers=` exists for both NFS and CIFS with different help text; the family
/// decides which one is meant, and they never both apply at once.
pub fn spec_for(family: Family, key: &str) -> Option<&'static Spec> {
    offered(family).into_iter().find(|s| s.key == key)
}

/// The value the prompt should start from, taken from the help document's
/// deliberately realistic first example (`vers=4.2` for NFS, `vers=3.1.1` for
/// CIFS). Empty when there is nothing sensible to suggest.
pub fn suggested_value(spec: &Spec) -> String {
    let Some(e) = fieldhelp::entry(spec.help) else {
        return String::new();
    };
    let ex = e.first_example();
    match ex.split_once('=') {
        Some((k, v)) if k == spec.key => v.to_string(),
        _ => String::new(),
    }
}

/// An `Options=` string being edited, plus the filesystem that decides what
/// is on offer for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionSet {
    opts: Vec<Opt>,
    family: Family,
}

impl OptionSet {
    pub fn new(options: &str, fstype: &str) -> OptionSet {
        OptionSet {
            opts: parse_options(options),
            family: family_for(fstype),
        }
    }

    pub fn family(&self) -> Family {
        self.family
    }

    /// The composed `Options=` string.
    pub fn text(&self) -> String {
        compose(&self.opts)
    }

    fn index_of(&self, key: &str) -> Option<usize> {
        self.opts.iter().position(|o| o.key == key)
    }

    pub fn is_on(&self, key: &str) -> bool {
        self.index_of(key).is_some()
    }

    /// The value currently set for `key`, if it is present and has one.
    pub fn value_of(&self, key: &str) -> Option<&str> {
        self.index_of(key)
            .and_then(|i| self.opts[i].value.as_deref())
    }

    /// Turn `spec` on, or update its value if it is already on.
    ///
    /// An option keeps whatever position it already had. A new option that
    /// displaces an exclusive sibling inherits *that* sibling's position, so
    /// switching `hard` to `soft` edits the list in place rather than moving
    /// the setting to the end where a later option could override it.
    pub fn enable(&mut self, spec: &Spec, value: Option<String>) {
        let new = Opt {
            key: spec.key.to_string(),
            value,
        };
        let at = self
            .index_of(spec.key)
            .or_else(|| self.first_sibling(spec))
            .unwrap_or_else(|| {
                self.opts.push(new.clone());
                self.opts.len() - 1
            });
        self.opts[at] = new;
        self.drop_siblings(spec, at);
    }

    pub fn disable(&mut self, key: &str) {
        self.opts.retain(|o| o.key != key);
    }

    /// Flip a flag; for a value option, turn it on with `default` or off.
    pub fn toggle(&mut self, spec: &Spec, default: Option<String>) {
        if self.is_on(spec.key) {
            self.disable(spec.key);
        } else {
            self.enable(spec, default);
        }
    }

    /// The first option that contradicts `spec` and is not `spec` itself.
    fn first_sibling(&self, spec: &Spec) -> Option<usize> {
        let group = spec.group?;
        self.opts.iter().position(|o| {
            o.key != spec.key
                && spec_for(self.family, &o.key).is_some_and(|s| s.group == Some(group))
        })
    }

    fn drop_siblings(&mut self, spec: &Spec, keep: usize) {
        let Some(group) = spec.group else { return };
        let family = self.family;
        let mut i = 0;
        self.opts.retain(|o| {
            let drop = i != keep
                && o.key != spec.key
                && spec_for(family, &o.key).is_some_and(|s| s.group == Some(group));
            i += 1;
            !drop
        });
    }

    /// Whether an option is one the menu can represent.
    fn known(&self, key: &str) -> bool {
        spec_for(self.family, key).is_some()
    }

    /// Everything the menu cannot represent, in original order. These are
    /// what the free-text entry holds, and the reason no option is ever lost.
    pub fn extras(&self) -> Vec<&Opt> {
        self.opts.iter().filter(|o| !self.known(&o.key)).collect()
    }

    /// The extras as an editable comma-separated string.
    pub fn extras_text(&self) -> String {
        self.extras()
            .into_iter()
            .map(Opt::text)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Replace the free-text extras, keeping them where they were.
    ///
    /// The replacement lands at the position of the first extra rather than
    /// at the end, so editing an unrelated option does not reorder a list
    /// where order decides which setting wins.
    pub fn set_extras(&mut self, text: &str) {
        let at = self
            .opts
            .iter()
            .position(|o| !self.known(&o.key))
            .unwrap_or(self.opts.len());
        let family = self.family;
        self.opts.retain(|o| spec_for(family, &o.key).is_some());
        for (i, o) in parse_options(text).into_iter().enumerate() {
            self.opts.insert(at + i, o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FSTYPES: [(&str, Family); 6] = [
        ("auto", Family::Generic),
        ("ext4", Family::Generic),
        ("nfs", Family::Nfs),
        ("nfs4", Family::Nfs),
        ("cifs", Family::Cifs),
        ("none", Family::Bind),
    ];

    fn families() -> [Family; 4] {
        [Family::Generic, Family::Nfs, Family::Cifs, Family::Bind]
    }

    // -----------------------------------------------------------------
    // The invariant the menu exists to protect: nothing is ever dropped.
    // -----------------------------------------------------------------

    /// Every option the menu offers must survive being written into an
    /// `Options=` string and read back. Written before the offered list was
    /// narrowed, so that narrowing it could not silently start losing
    /// options an existing unit already carries.
    #[test]
    fn every_offered_option_parses_back_unchanged() {
        for family in families() {
            for spec in offered(family) {
                let opt = match spec.kind {
                    Kind::Flag => Opt::flag(spec.key),
                    Kind::Value => {
                        let v = suggested_value(spec);
                        assert!(
                            !v.is_empty(),
                            "{} has no example value to prompt with",
                            spec.key
                        );
                        Opt::valued(spec.key, &v)
                    }
                };
                let text = compose(std::slice::from_ref(&opt));
                assert_eq!(parse_options(&text), vec![opt.clone()], "{}", spec.key);

                // And through the set, which is what the menu actually edits.
                let set = OptionSet::new(&text, fstype_of(family));
                assert!(set.is_on(spec.key), "{} lost by OptionSet", spec.key);
                assert_eq!(set.text(), text, "{}", spec.key);
                assert!(
                    set.extras().is_empty(),
                    "{} should be recognised, not an extra",
                    spec.key
                );
            }
        }
    }

    fn fstype_of(f: Family) -> &'static str {
        match f {
            Family::Generic => "ext4",
            Family::Nfs => "nfs",
            Family::Cifs => "cifs",
            Family::Bind => "none",
        }
    }

    /// A whole option string of everything offered, round-tripped at once.
    #[test]
    fn a_full_option_string_round_trips() {
        for family in families() {
            let all: Vec<Opt> = offered(family)
                .into_iter()
                // Only one member of each exclusive pair, so the string is
                // one a user could actually have produced.
                .filter(|s| s.group.is_none() || first_of_group(family, s))
                .map(|s| match s.kind {
                    Kind::Flag => Opt::flag(s.key),
                    Kind::Value => Opt::valued(s.key, &suggested_value(s)),
                })
                .collect();
            let text = compose(&all);
            assert_eq!(parse_options(&text), all, "{family:?}");
            let set = OptionSet::new(&text, fstype_of(family));
            assert_eq!(set.text(), text, "{family:?}");
        }
    }

    fn first_of_group(family: Family, spec: &Spec) -> bool {
        offered(family)
            .into_iter()
            .find(|s| s.group == spec.group)
            .is_some_and(|s| s.key == spec.key)
    }

    /// The options deliberately left off the menu, and anything else exotic,
    /// must come back byte for byte.
    #[test]
    fn unrecognised_options_survive_untouched() {
        let fstab_only = "x-systemd.automount,x-systemd.requires=/mnt/lower,\
                          x-systemd.device-timeout=10s,x-systemd.mount-timeout=30s,\
                          x-systemd.makefs,x-systemd.growfs,x-systemd.idle-timeout=120";
        let raw = format!("ro,{fstab_only},file_mode=0640");
        let set = OptionSet::new(&raw, "ext4");
        assert_eq!(set.text(), raw, "an untouched set must not change");
        assert!(set.is_on("ro"));
        let extras: Vec<String> = set.extras().into_iter().map(Opt::text).collect();
        assert_eq!(extras.len(), 8, "{extras:?}");
        assert!(extras.contains(&"x-systemd.automount".to_string()));
        assert!(extras.contains(&"file_mode=0640".to_string()));

        // Editing a recognised option leaves every extra exactly where it was.
        let mut set = set;
        set.toggle(spec_for(Family::Generic, "nosuid").unwrap(), None);
        assert_eq!(set.text(), format!("{raw},nosuid"));
        for e in fstab_only.split(',') {
            assert!(set.text().contains(e), "{e} lost");
        }
    }

    #[test]
    fn the_fstab_only_options_are_not_offered_anywhere() {
        let banned = [
            "x-systemd.automount",
            "x-systemd.requires",
            "x-systemd.device-timeout",
            "x-systemd.mount-timeout",
            "x-systemd.makefs",
            "x-systemd.growfs",
            "x-systemd.idle-timeout",
        ];
        for family in families() {
            for spec in offered(family) {
                assert!(
                    !banned.contains(&spec.key),
                    "{} must not be offered: it is inert in a unit file",
                    spec.key
                );
            }
        }
    }

    /// `nofail` and `_netdev` were probed and do work in a unit file, so
    /// they must stay.
    #[test]
    fn the_options_that_do_work_are_kept() {
        for family in families() {
            let keys: Vec<&str> = offered(family).into_iter().map(|s| s.key).collect();
            assert!(keys.contains(&"nofail"), "{family:?}");
            assert!(keys.contains(&"_netdev"), "{family:?}");
        }
    }

    // -----------------------------------------------------------------
    // Parsing and composing
    // -----------------------------------------------------------------

    #[test]
    fn parsing_keeps_order_values_and_empty_values() {
        let opts = parse_options("rw, vers=4.2 ,noatime,foo=");
        assert_eq!(
            opts,
            vec![
                Opt::flag("rw"),
                Opt::valued("vers", "4.2"),
                Opt::flag("noatime"),
                Opt::valued("foo", ""),
            ]
        );
        assert_eq!(compose(&opts), "rw,vers=4.2,noatime,foo=");
        // A bare key and an empty value stay distinguishable.
        assert_ne!(parse_options("foo"), parse_options("foo="));
    }

    #[test]
    fn empty_and_degenerate_strings_are_handled() {
        assert!(parse_options("").is_empty());
        assert!(parse_options("  ").is_empty());
        assert!(parse_options(",,,").is_empty());
        assert_eq!(compose(&[]), "");
        assert_eq!(OptionSet::new("", "ext4").text(), "");
        // A value containing '=' keeps the tail intact.
        assert_eq!(parse_options("a=b=c"), vec![Opt::valued("a", "b=c")]);
    }

    #[test]
    fn every_preset_option_string_round_trips() {
        use crate::unit::model::MountPreset;
        for p in MountPreset::ALL {
            let set = OptionSet::new(p.options(), p.fstype());
            assert_eq!(set.text(), p.options(), "{}", p.label());
            assert!(
                set.extras().is_empty(),
                "{} leaves unrecognised options: {:?}",
                p.label(),
                set.extras_text()
            );
        }
    }

    // -----------------------------------------------------------------
    // Filtering by filesystem
    // -----------------------------------------------------------------

    #[test]
    fn fstype_decides_the_family() {
        for (fstype, family) in FSTYPES {
            assert_eq!(family_for(fstype), family, "{fstype}");
            assert_eq!(family_for(&fstype.to_uppercase()), family, "{fstype}");
        }
        assert_eq!(family_for("  ext4 "), Family::Generic);
    }

    #[test]
    fn each_family_offers_its_own_options_and_no_others() {
        let has = |f: Family, k: &str| offered(f).into_iter().any(|s| s.key == k);
        // The generic set is everywhere.
        for f in families() {
            assert!(
                has(f, "ro") && has(f, "noatime") && has(f, "defaults"),
                "{f:?}"
            );
        }
        assert!(has(Family::Nfs, "soft") && has(Family::Nfs, "retrans"));
        assert!(!has(Family::Generic, "soft") && !has(Family::Cifs, "soft"));
        assert!(has(Family::Cifs, "credentials") && has(Family::Cifs, "iocharset"));
        assert!(!has(Family::Nfs, "credentials") && !has(Family::Bind, "credentials"));
        assert!(has(Family::Bind, "bind") && has(Family::Bind, "rbind"));
        assert!(!has(Family::Generic, "bind") && !has(Family::Nfs, "rbind"));
    }

    #[test]
    fn vers_means_different_things_to_nfs_and_cifs() {
        let nfs = spec_for(Family::Nfs, "vers").expect("nfs vers");
        let cifs = spec_for(Family::Cifs, "vers").expect("cifs vers");
        assert_eq!(nfs.help, "mount.opt.nfs.vers");
        assert_eq!(cifs.help, "mount.opt.cifs.vers");
        assert_eq!(suggested_value(nfs), "4.2");
        assert_eq!(suggested_value(cifs), "3.1.1");
    }

    #[test]
    fn an_option_from_the_wrong_family_becomes_an_extra() {
        // A CIFS option on an NFS mount is not something the menu can show,
        // so it must land in the free-text entry rather than vanish.
        let set = OptionSet::new("rw,credentials=/etc/creds", "nfs");
        assert_eq!(set.extras_text(), "credentials=/etc/creds");
        assert_eq!(set.text(), "rw,credentials=/etc/creds");
    }

    #[test]
    fn every_offered_option_has_help_copy() {
        for family in families() {
            for spec in offered(family) {
                let e = fieldhelp::entry(spec.help)
                    .unwrap_or_else(|| panic!("{} has no help entry {}", spec.key, spec.help));
                assert!(
                    e.label.starts_with(spec.key),
                    "help {} labels '{}', not '{}'",
                    spec.help,
                    e.label,
                    spec.key
                );
                // The help document marks value options as such.
                assert_eq!(
                    e.label.contains("*(value)*"),
                    spec.kind == Kind::Value,
                    "{} disagrees with its help on taking a value",
                    spec.key
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // Toggling
    // -----------------------------------------------------------------

    #[test]
    fn exclusive_pairs_replace_rather_than_accumulate() {
        let ro = spec_for(Family::Generic, "ro").unwrap();
        let rw = spec_for(Family::Generic, "rw").unwrap();
        let mut set = OptionSet::new("rw,noatime,nofail", "ext4");
        set.enable(ro, None);
        // ro takes rw's position; nothing else moves and rw is gone.
        assert_eq!(set.text(), "ro,noatime,nofail");
        assert!(!set.is_on("rw"));
        set.enable(rw, None);
        assert_eq!(set.text(), "rw,noatime,nofail");

        // The other pairs behave the same way.
        for (fstype, a, b, family) in [
            ("ext4", "noatime", "relatime", Family::Generic),
            ("nfs", "hard", "soft", Family::Nfs),
            ("nfs", "bg", "fg", Family::Nfs),
            ("none", "bind", "rbind", Family::Bind),
        ] {
            let mut set = OptionSet::new(a, fstype);
            set.enable(spec_for(family, b).unwrap(), None);
            assert_eq!(set.text(), b, "{a} -> {b}");
            assert!(!set.is_on(a), "{a} should be gone");
        }
    }

    #[test]
    fn a_pair_that_was_already_doubled_up_collapses_to_one() {
        // Someone else's unit can carry both halves; picking one must not
        // leave the loser behind to override it.
        let mut set = OptionSet::new("ro,noatime,rw", "ext4");
        set.enable(spec_for(Family::Generic, "ro").unwrap(), None);
        assert_eq!(set.text(), "ro,noatime");
    }

    #[test]
    fn independent_options_do_not_interfere() {
        let mut set = OptionSet::new("", "ext4");
        for k in ["noexec", "nosuid", "nodev", "nofail", "_netdev"] {
            set.toggle(spec_for(Family::Generic, k).unwrap(), None);
        }
        assert_eq!(set.text(), "noexec,nosuid,nodev,nofail,_netdev");
        set.toggle(spec_for(Family::Generic, "nosuid").unwrap(), None);
        assert_eq!(set.text(), "noexec,nodev,nofail,_netdev");
    }

    #[test]
    fn toggling_a_value_option_uses_the_suggested_value() {
        let vers = spec_for(Family::Nfs, "vers").unwrap();
        let mut set = OptionSet::new("rw", "nfs");
        set.toggle(vers, Some(suggested_value(vers)));
        assert_eq!(set.text(), "rw,vers=4.2");
        // Re-entering a value keeps the position.
        set.enable(vers, Some("3".into()));
        assert_eq!(set.text(), "rw,vers=3");
        set.toggle(vers, None);
        assert_eq!(set.text(), "rw");
    }

    #[test]
    fn enabling_something_already_on_keeps_its_position() {
        let mut set = OptionSet::new("ro,vers=3,noatime", "nfs");
        set.enable(spec_for(Family::Nfs, "vers").unwrap(), Some("4.2".into()));
        assert_eq!(set.text(), "ro,vers=4.2,noatime");
        set.enable(spec_for(Family::Generic, "noatime").unwrap(), None);
        assert_eq!(set.text(), "ro,vers=4.2,noatime");
    }

    #[test]
    fn extras_can_be_edited_in_place() {
        let mut set = OptionSet::new("ro,x-systemd.automount,nofail", "ext4");
        assert_eq!(set.extras_text(), "x-systemd.automount");
        set.set_extras("file_mode=0640,dir_mode=0750");
        assert_eq!(set.text(), "ro,file_mode=0640,dir_mode=0750,nofail");
        set.set_extras("");
        assert_eq!(set.text(), "ro,nofail");
        // With no extras present they append rather than being lost.
        set.set_extras("nobrl");
        assert_eq!(set.text(), "ro,nofail,nobrl");
    }

    #[test]
    fn disabling_something_absent_is_a_no_op() {
        let mut set = OptionSet::new("ro", "ext4");
        set.disable("nosuid");
        assert_eq!(set.text(), "ro");
    }
}
