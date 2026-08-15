# Field help copy

Per-field help text for the notcron builders. One `###` entry per field, keyed
stably so it can be turned into a lookup table. Every claim here was checked
against the man pages on systemd 257 (`systemd.timer(5)`, `systemd.service(5)`,
`systemd.exec(5)`, `systemd.mount(5)`, `systemd.automount(5)`, `mount(8)`,
`nfs(5)`, `mount.cifs(8)`), and the mount-option behaviour was verified by
loading probe units.

Shape of every entry:

```
### <FieldKey>
**Label:** <label shown in the UI>
**Summary:** <one line, <= ~90 chars, fits the status bar>
**Detail:** <2-5 sentences>
**Examples:** <concrete values>
```

## Unit

### unit.name
**Label:** Name
**Summary:** Short job name; notcron- is prepended and the result becomes the unit filename.
**Detail:** The name is slugified and prefixed, so `backup` becomes `notcron-backup.timer` plus `notcron-backup.service`. It is also the `SyslogIdentifier=`, so it is what you grep for in the journal. Only `A-Z a-z 0-9 . _ -` survive; anything else is rewritten. Renaming later means a new unit — notcron does not move the old files.
**Examples:** backup, certbot-renew, prune-snapshots

### unit.description
**Label:** Description
**Summary:** One-line `Description=`; shown by systemctl status and in journal metadata.
**Detail:** Purely cosmetic to systemd, but it is the line `systemctl list-timers` and `status` put in front of you at 3am, so write what the job does, not what it is called. Must be a single line. The timer file gets the same text with ` (timer)` appended, the automount with ` (automount)`.
**Examples:** Nightly restic backup of /srv to B2, Renew TLS certificates

### unit.scope
**Label:** Scope
**Summary:** user units in ~/.config/systemd/user (no sudo) vs system units in /etc/systemd/system.
**Detail:** User scope needs no privileges and is the default, but a user manager is normally torn down at last logout — enable lingering (`loginctl enable-linger $USER`) or your timers stop existing between sessions. User scope also removes `User=` as an option and shrinks the set of valid `WantedBy=` targets (no `multi-user.target`, no `network-online.target`). System scope writes via sudo and runs as root unless you set `User=`.
**Examples:** user, system

### unit.manual_primary
**Label:** Extra [Service] / [Mount] lines
**Summary:** Free-form directives appended verbatim to the primary file; include the [Section] header.
**Detail:** Anything the builder does not model goes here and survives a parse/render round trip untouched. You must write the section header yourself, because the block is appended after everything notcron generates. Use it for hardening (`ProtectSystem=`, `PrivateTmp=`), niceness, `RemainAfterExit=`, `EnvironmentFile=`, or `TimeoutStartSec=`. Nothing in the block is validated — `systemd-analyze verify` is still your friend.
**Examples:**
[Service]
Nice=19
IOSchedulingClass=idle

### unit.manual_secondary
**Label:** Extra [Timer] / [Automount] lines
**Summary:** Free-form directives appended verbatim to the second file of the pair.
**Detail:** Same mechanics as the primary block, aimed at the `.timer` or `.automount`. Useful for the timer knobs the builder deliberately does not expose: `WakeSystem=`, `FixedRandomDelay=`, `OnUnitInactiveSec=`, `DeferReactivation=` (systemd 257+), `RemainAfterElapse=`. Not shown for standalone services, which are a single file.
**Examples:**
[Timer]
FixedRandomDelay=true
WakeSystem=true

### builder.preview
**Label:** Preview the unit files
**Summary:** Renders every file exactly as it will be written, without touching disk.
**Detail:** Shows the full text of both files including the notcron marker header and your manual block. An incomplete unit still previews, with the validation error on the first line. Also reachable with `p` from anywhere in the builder.
**Examples:** (no value)

### builder.save
**Label:** Save and install
**Summary:** Writes the files, runs daemon-reload, then enables and starts the unit.
**Detail:** Validation runs first and blocks the write. You get a confirmation listing the target directory and any files that would be overwritten; system scope escalates via sudo at that point. After writing, notcron reloads the manager and enables the primary unit — the `.timer`, the `.service`, or the `.automount`. Mounts additionally ask whether to mount right now. Also bound to Ctrl-S.
**Examples:** (no value)

## Timer

### timer.schedule
**Label:** When
**Summary:** Pick a preset, a cron expression, or a raw OnCalendar spec; validated before it lands.
**Detail:** Calendar choices render as one or more `OnCalendar=` lines, which systemd ORs together — the timer fires when any of them elapses. The interval preset renders `OnBootSec=` plus `OnUnitActiveSec=` instead, a monotonic timer measured from boot and from the last activation of the service. Every generated calendar spec is run through `systemd-analyze calendar` before it can be saved, and the builder's status line shows the next elapse. If the service is still running when the timer elapses, systemd does not start a second copy — it leaves the running one alone.
**Examples:** *-*-* 03:00:00, Mon..Fri *-*-* 09:00:00, every 15 minutes, at boot + 2min

### timer.persistent
**Label:** Persistent
**Summary:** Catch up a run missed while the machine was off. Calendar timers only.
**Detail:** With `Persistent=true` systemd stamps the last trigger time on disk — `/var/lib/systemd/timers/stamp-<unit>.timer` for system units, `~/.local/share/systemd/timers/` for user units. When the timer next starts, if the calendar would have fired at least once during the gap, the service is triggered immediately; multiple missed occurrences still collapse into a single run. It has no effect whatsoever on `OnBootSec=`/`OnUnitActiveSec=` timers, so notcron omits the directive entirely for interval and boot schedules. The catch-up run is still subject to `RandomizedDelaySec=`. When retiring a timer, `systemctl clean --what=state` removes the stamp file.
**Examples:** yes (default), no

### timer.randomized_delay
**Label:** RandomizedDelaySec
**Summary:** Add a random 0..N delay to every firing, to stagger jobs across hosts.
**Detail:** Defaults to 0. The delay is redrawn before each iteration and added on top of the computed elapse time — set `FixedRandomDelay=true` in the manual block to draw it once per machine instead, which keeps the stagger but removes the jitter. This is the opposite of `AccuracySec=`: accuracy coalesces wakeups, randomization spreads them out. When both are set the random delay is applied first, then the result may be nudged to coalesce with other timers, so a large `AccuracySec=` will partially undo the spreading. It also applies to `Persistent=` catch-up runs, which is what stops a fleet from stampeding after a power cut.
**Examples:** 30s, 5min, 1h

### timer.accuracy_sec
**Label:** AccuracySec (generated)
**Summary:** notcron pins AccuracySec=1s; systemd's own default is 1min, not 1s.
**Detail:** systemd schedules a timer anywhere inside a window that starts at the configured time and ends `AccuracySec=` later, picking a host-stable randomized position so unrelated timers share one CPU wakeup. The stock default is 1 minute, which is a power-saving choice and the reason a stock `OnCalendar=*:00` job can visibly run at 03:00:47. notcron writes `AccuracySec=1s` because a job you scheduled by hand should run when you said; raise it in the manual block on battery-powered or dense hosts, and set `1us` if you also want `RandomizedDelaySec=` to spread cleanly.
**Examples:** 1s (notcron default), 1min (systemd default), 1us

### timer.wanted_by
**Label:** WantedBy (generated)
**Summary:** Timers are always installed into timers.target; the service itself has no [Install].
**Detail:** `systemctl enable` on the `.timer` symlinks it under `timers.target`, which is what makes it start at boot (or at user-manager start, under lingering). The paired `.service` deliberately has no `[Install]` section — it is pulled in by the timer's `Unit=`, and enabling it would make it run at boot as well as on schedule. Timer units also gain automatic ordering after `sysinit.target`, and calendar timers additionally after `time-set.target` and `time-sync.target` so they do not fire against an unset clock.
**Examples:** timers.target

## Service

### service.type
**Label:** Type
**Summary:** How systemd decides the unit has finished starting. Scheduled scripts want oneshot.
**Detail:** `simple` considers the unit started the instant the process is forked — before `execve()`, so a missing binary or a bogus `User=` still reports success. `exec` waits for the `execve()` to succeed, so startup errors are reported honestly; it is the better default for anything long-running. `oneshot` waits for the process to exit, which is what a scheduled script wants: dependencies wait for it, and the timer's next elapse is measured against a run that actually completed. A oneshot without `RemainAfterExit=yes` never reaches `active` — it goes activating -> dead, and `systemctl status` showing `inactive (dead)` after a successful run is correct, not a failure. `forking` expects the process to daemonize and the parent to exit (pair it with `PIDFile=`); `notify` expects the program to call `sd_notify(3)` with `READY=1` and is the right answer only if the program actually supports it.
**Examples:** oneshot (timer jobs), exec (long-running), simple, forking, notify

### service.exec_start
**Label:** ExecStart
**Summary:** Absolute path plus arguments. No shell — pipes, globs and $VAR do not expand.
**Detail:** systemd splits the line into argv itself; there is no shell involved, so `|`, `>`, `*`, `&&` and backticks are passed through as literal arguments or rejected. The first token must be an absolute path (`/usr/bin/rsync`, not `rsync`) because `PATH` lookup is not performed. Quote arguments containing spaces; `%` must be written `%%` to escape specifier expansion. For anything with shell syntax, use the wrapper below.
**Examples:** /usr/local/bin/backup.sh --full, /usr/bin/rsync -a /data/ /srv/backup/

### service.shell_wrap
**Label:** Wrap in /bin/sh -c
**Summary:** Toggle the command between plain argv and /bin/sh -c "…" so shell syntax works.
**Detail:** Rewrites `ExecStart=` to `/bin/sh -c "<command>"`, quoting as needed, and toggles back off the same way. Use it when you need a pipeline, a redirect, a glob, or `&&`. Costs you one extra process and makes the shell's exit status the service's exit status — so `a | b` reports the status of `b`, and a failing first stage looks like success unless you add `set -o pipefail` (and switch to `/bin/bash -c`, since POSIX sh has no pipefail).
**Examples:** /bin/sh -c "df -h | mail -s disk me@example.com"

### service.exec_start_pre
**Label:** ExecStartPre
**Summary:** Runs to completion before ExecStart; a non-zero exit aborts the whole unit.
**Detail:** Same no-shell rules as `ExecStart=`. Failure here means the service never starts and is marked failed, which makes it the natural place for a precondition check — mount present, lock acquired, network reachable. Prefix the path with `-` to make failure non-fatal. It runs with the same `User=`, `WorkingDirectory=` and `Environment=` as the main command.
**Examples:** /usr/bin/mountpoint -q /srv/backup, -/usr/bin/mkdir -p /var/cache/job

### service.exec_stop_post
**Label:** ExecStopPost
**Summary:** Runs after the service stops, on success and on failure alike.
**Detail:** The cleanup/notification hook: it fires even when the main command failed, was killed, or timed out, so it is where you release locks or ping a monitor. `$SERVICE_RESULT`, `$EXIT_CODE` and `$EXIT_STATUS` are set in its environment, which lets one command handle both outcomes. Do not put the "job succeeded" notification here without checking `$SERVICE_RESULT`.
**Examples:** /usr/local/bin/notify-result, /bin/sh -c "rm -f /run/myjob.lock"

### service.restart
**Label:** Restart
**Summary:** Whether to relaunch when the process exits. Timer jobs leave this at no.
**Detail:** `no` is the default and the only sane setting for a timer-driven job — the timer is the retry mechanism, and a restarting oneshot would loop. `on-failure` is the recommendation for long-running services: it covers non-zero exits, unclean signals, operation timeouts and watchdog trips. `on-abnormal` restarts only on signals, timeouts and watchdog, so a service that chooses to exit cleanly stays down. `always` restarts regardless, including after a clean exit. Note that `always` and `on-success` are rejected outright for `Type=oneshot`, and that restarts are throttled by `StartLimitIntervalSec=`/`StartLimitBurst=` — a fast crash loop ends in `failed`, not an infinite retry.
**Examples:** no, on-failure, on-abnormal, always

### service.restart_sec
**Label:** RestartSec
**Summary:** Sleep before restarting. Defaults to 100ms, which is usually far too eager.
**Detail:** Only consulted when `Restart=` is not `no`. The 100ms default will burn through the default start rate limit (5 starts in 10s) almost instantly on a service that fails fast, so a service that depends on something slow — a database, a network mount — wants seconds, not milliseconds. Accepts a bare number of seconds or a time span. For real backoff, add `RestartSteps=`/`RestartMaxDelaySec=` in the manual block.
**Examples:** 5s, 30s, 2min

### service.working_directory
**Label:** WorkingDirectory
**Summary:** Absolute directory the command runs in. Defaults to / for system units.
**Detail:** Unset means `/` for system services and the user's home directory for user services — the `/` case is what breaks scripts carrying relative paths that worked fine from a shell. Prefix with `-` to tolerate the directory being missing rather than failing the unit. `~` expands to the home directory of `User=`. Setting it may add an implicit dependency on the mount that provides the path.
**Examples:** /srv/app, /var/lib/myjob, -/mnt/scratch

### service.user
**Label:** User
**Summary:** Run as this user. System scope only — in a user unit it is at best a no-op.
**Detail:** For system units the default is root and `User=` drops to the named account, initialising its supplementary groups from the user database. In a user unit there is no privilege to switch identity: the only accepted value is the user the manager already runs as, and naming anyone else makes the service fail at start. Since notcron defaults to user scope, treat this field as "switch to system scope first". The account must already exist when the unit starts; `Group=` and `DynamicUser=` belong in the manual block.
**Examples:** backup, www-data, nobody

### service.environment
**Label:** Environment
**Summary:** One KEY=VALUE per line. The unit's environment is nearly empty otherwise.
**Detail:** Services do not inherit your login shell's environment — no profile, no `.bashrc`, and a minimal `PATH`. That is the usual reason a script that runs by hand fails under a timer. Values with spaces need quoting around the whole assignment (`"VAR=a b"`); `$` is literal, and specifier expansion means a literal `%` must be doubled. Never put secrets here: unit environment is readable over D-Bus by unprivileged clients and is inherited across the whole process tree. Use `EnvironmentFile=` or `LoadCredential=` in the manual block for those.
**Examples:** PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin, TZ=Europe/Warsaw, "BACKUP_TAG=nightly run"

### service.wanted_by
**Label:** WantedBy
**Summary:** Which target enable hooks the service to. multi-user.target for normal system daemons.
**Detail:** `systemctl enable` creates a symlink in `<target>.wants/`, so this is what "starts at boot" actually means. `multi-user.target` is the normal non-graphical system boot target; `graphical.target` pulls it in, so choosing graphical only delays the service until the display stack is up. `network-online.target` is a *wants* target and needs an `After=network-online.target` ordering line (manual block) to be worth anything — it also only means "the network was configured once", not "the internet works". In user scope the picker's system targets do not exist: a user manager has `default.target`, `basic.target` and `graphical-session.target`, so pick `default.target` there and enable lingering if it must start without a login.
**Examples:** multi-user.target, default.target (user scope), graphical.target, network-online.target

## Mount

### mount.preset
**Label:** Preset
**Summary:** Picks a starting Type= and Options= for a block device, NFS, CIFS or bind mount.
**Detail:** A convenience only — selecting a preset overwrites the `Type=` and `Options=` fields with defaults for that flavour, and you can edit both afterwards. Block devices get `Type=auto` so the kernel probes the filesystem, plus `defaults,nofail`. NFS and CIFS get `_netdev` so the mount is ordered after the network. Bind uses `Type=none` with `bind`, which is how a bind mount is spelled in unit/fstab form.
**Examples:** block device / local filesystem, NFS export, SMB/CIFS share, bind mount

### mount.what
**Label:** What
**Summary:** The mount source: device node, UUID/LABEL identifier, remote share, or bind source.
**Detail:** Written to `What=`; mandatory. For block devices prefer a stable path — `/dev/sdb1` renumbers, `/dev/disk/by-uuid/...` does not — and a device path makes systemd add an automatic dependency on the corresponding `.device` unit, so the mount waits for the disk to appear. NFS wants `host:/export`, CIFS wants `//host/share` (forward slashes; backslashes are not reliably converted past the share name). For a bind mount this is the source directory, and systemd creates it as a directory if it does not exist.
**Examples:** /dev/disk/by-uuid/9c1f-…, LABEL=data, nas.lan:/export/backups, //nas.lan/media, /srv/source

### mount.where
**Label:** Where
**Summary:** Absolute mount point. The unit filename is derived from it and must match.
**Detail:** systemd requires the filename to be the escaped mount point — `/srv/my-share` becomes `srv-my\x2dshare.mount` — and notcron does that escaping for you, so this field also decides the unit name. The path must be absolute and must not be a symlink; systemd creates the directory (mode 0755) if it is missing. A mount nested under another mount automatically gains a requirement and ordering on the parent. Changing this later produces a differently named unit, not a rename.
**Examples:** /mnt/data, /srv/backup, /media/nas

### mount.fstype
**Label:** Type
**Summary:** Filesystem type passed to mount(8). `auto` probes, `none` is what bind mounts use.
**Detail:** Written to `Type=`. Nominally optional to systemd, but notcron requires it because the type is also how systemd decides whether a mount is "local" or "network" and therefore how it is ordered at boot — `nfs`, `cifs` and friends are recognised as network, anything unusual (iSCSI-backed, for instance) needs `_netdev` in the options to get the same treatment. `auto` lets the kernel probe a block device via blkid; naming the real type is faster and fails loudly on a wrong guess.
**Examples:** ext4, xfs, btrfs, auto, nfs, cifs, none

### mount.options
**Label:** Options
**Summary:** Comma-separated mount options, same syntax as the fstab fourth field.
**Detail:** Passed straight to `mount(8)` as `-o`. Two caveats specific to unit files: several `x-systemd.*` options only work in `/etc/fstab` and are silently ignored here (verified on this host — `x-systemd.automount`, `x-systemd.requires=`, `x-systemd.device-timeout=`, `x-systemd.mount-timeout=`, `x-systemd.makefs`, `x-systemd.growfs`), whereas `nofail` and `_netdev` *are* honoured in a unit file and do change the generated dependencies. Never put a password in here — the options string is world-readable via `systemctl show` and `/proc/self/mountinfo`; use `credentials=` for CIFS. A literal `%` must be written `%%`.
**Examples:** defaults,nofail; rw,soft,timeo=100,noatime,_netdev; rw,noatime,_netdev,credentials=/etc/cifs-credentials,uid=0,gid=0; bind

### mount.automount
**Label:** Companion .automount
**Summary:** Also write an .automount unit so the filesystem mounts on first access, not at boot.
**Detail:** Installs an autofs trigger on the mount point; the `.mount` itself is only started when something touches the path. This is the right answer for a flaky NFS/CIFS server, because boot no longer waits for it and an unreachable server costs you a hang at first access instead of at startup. When automount is on, notcron enables and starts the `.automount` as the primary unit — enabling the `.mount` as well would defeat the point. Note that the `x-systemd.automount` mount option does *not* work from a unit file (only from fstab), which is exactly why the companion unit exists.
**Examples:** yes, no

### mount.timeout_idle
**Label:** TimeoutIdleSec
**Summary:** Unmount after this long with no access. Disabled by default; notcron seeds 120.
**Detail:** Written to the `.automount` unit. After the interval with no filesystem activity, systemd asks autofs to expire the mount and stops the `.mount` unit; the next access mounts it again. "Idle" means nothing is holding a reference — an open file, a process whose cwd is inside, or a mount nested below all keep it busy, so on a share something polls the timeout will simply never take effect. Set 0 to disable. Value is bare seconds or a time span.
**Examples:** 120 (notcron default), 5min, 0 (never unmount)

## Mount options

Per-option help for the `Options=` toggle menu. Options marked *(value)* prompt
for a value; the Examples line is what the prompt should hint.

### mount.opt.ro
**Label:** ro
**Summary:** Mount read-only. Exclusive with rw.
**Detail:** Blocks writes at the VFS level for this mount point. On a bind mount `ro` needs a `remount,bind,ro` pass to actually apply on older util-linux, and even then only the new mount point is read-only — the original path stays writable, since the superblock is shared. Worth pairing with `noexec,nosuid,nodev` for anything you did not create.
**Examples:** ro

### mount.opt.rw
**Label:** rw
**Summary:** Mount read-write. The default; exclusive with ro.
**Detail:** Explicit is fine, but it changes nothing on its own — `defaults` already includes `rw`. Relevant mainly as the opposite half of the toggle, and when you want to be sure a later `ro` in the option list is not what wins. If a read-write mount fails, systemd retries read-only unless `ReadWriteOnly=yes` is set on the unit.
**Examples:** rw

### mount.opt.noatime
**Label:** noatime
**Summary:** Never update access times. The one atime option that actually changes behaviour.
**Detail:** Suppresses inode atime updates for all inode types, directories included, so it implies `nodiratime`. Since Linux 2.6.30 the kernel already behaves as `relatime`, meaning atime is only rewritten when it was older than mtime/ctime — so switching from the default to `relatime` buys nothing, and `noatime` is the change with a measurable effect. Costs you the ability of tools that care about "read since last modified" (mutt, some backup heuristics) to work correctly.
**Examples:** noatime

### mount.opt.relatime
**Label:** relatime
**Summary:** Update atime only when it is older than mtime/ctime. Already the kernel default.
**Detail:** Writes atime lazily: only when the previous atime predates the current modify or change time (plus once a day in practice). This is the kernel's default behaviour since 2.6.30, so specifying it explicitly is documentation rather than configuration. Use it when you need to override an inherited `noatime` from a preset or a parent mount.
**Examples:** relatime

### mount.opt.nofail
**Label:** nofail
**Summary:** Boot continues even if this mount fails. Changes dependency type, not timing alone.
**Detail:** Without it the mount is *required* by `local-fs.target`/`remote-fs.target` and ordered before it, so a failure takes the boot into emergency mode. With `nofail` the target only *wants* the mount, and the ordering is dropped entirely — boot proceeds without waiting and without caring about the result. Verified honoured in unit files, not just fstab: with `nofail` the generated unit loses its `Before=local-fs.target`. Because the ordering goes away too, anything that genuinely needs the data must depend on the mount unit itself.
**Examples:** nofail

### mount.opt.noexec
**Label:** noexec
**Summary:** Refuse direct execution of binaries from this filesystem.
**Detail:** Blocks `execve()` on the mount; it does not stop `sh script.sh` or an interpreter reading a file from the path, so it is a speed bump, not a boundary. Standard hygiene on anything holding untrusted or user-supplied data, and free of cost on data-only mounts. Pair with `nosuid` and `nodev`.
**Examples:** noexec

### mount.opt.nosuid
**Label:** nosuid
**Summary:** Ignore setuid/setgid bits and file capabilities on this filesystem.
**Detail:** Stops a setuid root binary on the mount from actually gaining privilege — the single most important option on removable media and on any filesystem a remote party can write to. Also disables file capabilities. Effectively mandatory for CIFS/NFS shares from a machine you do not control.
**Examples:** nosuid

### mount.opt.nodev
**Label:** nodev
**Summary:** Do not interpret character or block device nodes on this filesystem.
**Detail:** Prevents a device node smuggled onto the filesystem (say a writable `/dev/sda` on a USB stick or an NFS export) from being usable as a device. Together with `nosuid` and `noexec` this is the standard hardening triple for any mount that is not a trusted system filesystem.
**Examples:** nodev

### mount.opt.defaults
**Label:** defaults
**Summary:** Shorthand for rw,suid,dev,exec,auto,nouser,async.
**Detail:** Expands to the traditional default set, though the *effective* defaults also depend on kernel and filesystem. It is a placeholder more than a setting: options listed after it override it, which is why `defaults,nofail` is the idiomatic pairing. Do not combine it with the hardening options and then wonder which won — order matters, later wins.
**Examples:** defaults, defaults,nofail

### mount.opt.nfs.vers
**Label:** vers= *(value)*
**Summary:** Pin the NFS protocol version instead of negotiating down from 4.2.
**Detail:** Without it the client tries 4.2 first and negotiates downward until the server agrees; with it, a server that cannot speak the named version fails the mount instead of quietly falling back. Pinning is what you want when a silent drop to NFSv3 would lose you the security or locking semantics you designed around. `nfsvers=` is the canonical spelling, `vers=` the compatibility alias.
**Examples:** vers=4.2, vers=4.1, vers=3

### mount.opt.nfs.hard
**Label:** hard
**Summary:** Retry NFS requests forever. The default, and the correct choice for writable mounts.
**Detail:** With `hard`, a request that times out is retried indefinitely — an unreachable server makes processes block in uninterruptible sleep until it comes back, which looks like a hang but loses nothing. That is the trade: availability of the client versus integrity of the data. Combine with `intr`-era expectations carefully (modern kernels allow SIGKILL on NFSv4), and reach for an automount with an idle timeout rather than `soft` if the hangs are the problem.
**Examples:** hard

### mount.opt.nfs.soft
**Label:** soft
**Summary:** Fail requests after retrans retries — and risk silent data corruption.
**Detail:** After `retrans` retransmissions the client returns EIO to the application instead of retrying forever (`softerr` returns ETIMEDOUT instead). The man page is blunt about the consequence: a soft timeout can cause silent data corruption, because a write that was actually delivered may be reported as failed, or an application may ignore the error and carry on. Use it only when client responsiveness matters more than the data, keep it to read-mostly mounts, and mitigate with TCP and a larger `retrans`.
**Examples:** soft

### mount.opt.nfs.timeo
**Label:** timeo= *(value)*
**Summary:** Retry timeout in *deciseconds*, not seconds. 600 means 60 seconds.
**Detail:** The tenths-of-a-second unit is the classic trap: `timeo=100` is 10 seconds, not 100. For NFS over TCP the default is 600 (60s) and the client backs off linearly, adding `timeo` after each retransmission up to 600 seconds; values above the default are clamped back to it. For UDP the default is 60 (6s) with doubling backoff. Only meaningful together with `retrans` and `soft`/`hard`, since it decides how long each attempt waits before the retry policy applies.
**Examples:** timeo=600, timeo=100, timeo=50

### mount.opt.nfs.retrans
**Label:** retrans= *(value)*
**Summary:** How many retries before "server not responding" and recovery kicks in.
**Detail:** Defaults to 3 for UDP and 2 for TCP. After this many retransmissions the client logs "server not responding" and then acts according to `hard` (keep retrying) or `soft` (fail the request). Raising it is the standard way to make a `soft` mount less dangerous on a flaky link — total wait is roughly `retrans` × `timeo` plus backoff.
**Examples:** retrans=2, retrans=3, retrans=5

### mount.opt.nfs.rsize
**Label:** rsize= *(value)*
**Summary:** Max bytes per NFS READ. Leave unset to let client and server negotiate.
**Detail:** Unset means the two ends agree on the largest size they both support, which is almost always right on a modern stack. The Linux client caps at 1048576; values under 1024 become 4096, values over the cap are clamped, and anything not a page-size multiple or power of two is rounded down. Only worth pinning when you are working around a specific server or a lossy link where smaller reads retransmit more cheaply.
**Examples:** rsize=1048576, rsize=131072, rsize=32768

### mount.opt.nfs.wsize
**Label:** wsize= *(value)*
**Summary:** Max bytes per NFS WRITE. Same negotiation and clamping rules as rsize.
**Detail:** Mirrors `rsize=` for the write path, with the same 1048576 ceiling, the same rounding, and the same "negotiated by default" behaviour. Lowering it can help on congested or high-loss networks where a large write that has to be retransmitted costs more than the extra round trips; on a healthy LAN it just adds overhead.
**Examples:** wsize=1048576, wsize=131072, wsize=32768

### mount.opt.nfs.bg
**Label:** bg
**Summary:** Fork the mount attempt into the background instead of failing at boot.
**Detail:** If the mount times out or fails, `mount(8)` forks a child that keeps trying and the parent exits 0 immediately. Under systemd this is messier than it sounds: `systemd-fstab-generator` rewrites `bg` into `x-systemd.mount-timeout=infinity,retry=10000` plus `fg,nofail` to make the job-control semantics work — and that rewriting only happens for fstab entries, not for a hand-written unit file. In a notcron-generated unit, prefer the companion `.automount` (or `nofail`) over `bg`.
**Examples:** bg

### mount.opt.nfs.fg
**Label:** fg
**Summary:** Fail the mount in the foreground. The default.
**Detail:** `mount(8)` exits non-zero if any part of the request times out or fails, which is what makes the `.mount` unit report failure honestly. Keep it, and control boot behaviour with `nofail` or an automount instead of by backgrounding the mount itself.
**Examples:** fg

### mount.opt.cifs.credentials
**Label:** credentials= *(value)*
**Summary:** Read username/password from a root-only file instead of the options string.
**Detail:** The file holds `username=`, `password=`, optionally `password2=` and `domain=`, one per line. This is the only acceptable way to authenticate a CIFS mount from a unit file: the `Options=` string is exposed by `systemctl show`, `/proc/self/mountinfo` and the journal, so `password=` there is equivalent to publishing it. Create the file `0600 root:root` outside any backed-up or world-readable tree, and remember the mount happens before most of userspace — keep it on a local filesystem.
**Examples:** credentials=/etc/cifs-credentials, credentials=/root/.smbcreds-nas

### mount.opt.cifs.username
**Label:** username= *(value)*
**Summary:** SMB user to connect as. Use credentials= instead when a password is involved.
**Detail:** Falls back to `$USER` if unset, which is rarely what a system mount wants. Write it as `username=`, not `user=` — the abbreviation can confuse `mount(8)` into treating the request as a non-superuser mount. The old `user%password` and `workgroup/user` forms are deprecated; put the domain in `domain=` and the password in a credentials file.
**Examples:** username=backup, username=svc-media

### mount.opt.cifs.uid
**Label:** uid= *(value)*
**Summary:** Owner for files when the server sends no ownership info. Defaults to 0.
**Detail:** SMB servers frequently expose no POSIX ownership, so every file appears owned by uid 0 unless you say otherwise — which is why a share mounted "successfully" is still unreadable to the service that needs it. Accepts a numeric id or a username (mount.cifs 1.10+). Pair with `gid=`, and note this is a display/permission mapping on the client, not an access decision on the server.
**Examples:** uid=1000, uid=backup, uid=0

### mount.opt.cifs.gid
**Label:** gid= *(value)*
**Summary:** Group for files when the server sends no ownership info. Defaults to 0.
**Detail:** The group half of the same mapping as `uid=`; same numeric-or-name rules. Use it when a service account needs group access to the share — combined with `file_mode=`/`dir_mode=` (manual entry) it is how you get sane permissions out of a server with no Unix extensions.
**Examples:** gid=1000, gid=backup, gid=0

### mount.opt.cifs.vers
**Label:** vers= *(value)*
**Summary:** SMB dialect. Default negotiates the highest ≥2.1; pin it for predictability.
**Detail:** Accepted values include `1.0`, `2.0`, `2.1`, `3.0`, `3.02`/`3.0.2`, `3.1.1`/`3.11`, plus `3` (3.0 and above) and `default`. Since kernel 4.13.5 the default negotiates the best dialect ≥2.1; pre-4.13 kernels defaulted to `1.0`. Pin `3.1.1` against modern Windows/Samba — it is the only dialect with pre-auth integrity. Never use `1.0` unless a legacy appliance forces it.
**Examples:** vers=3.1.1, vers=3.0, vers=2.1

### mount.opt.cifs.iocharset
**Label:** iocharset= *(value)*
**Summary:** Charset used to convert path names to and from Unicode.
**Detail:** Only relevant when the server does *not* support Unicode for path names — otherwise Unicode is used regardless and this setting is unused. Unset means the kernel's `nls_default` from build time. Set it when a legacy server hands you mangled non-ASCII filenames; the matching `nls_*` module must be available.
**Examples:** iocharset=utf8, iocharset=iso8859-2

### mount.opt.bind
**Label:** bind
**Summary:** Attach an existing directory at a second path. Non-recursive.
**Detail:** Re-attaches (part of) one filesystem somewhere else; both paths are equal citizens in the VFS, and the original may even be unmounted afterwards. Only the single filesystem at the source is carried over — anything mounted *below* the source is not. Pair with `Type=none`. Changing flags on a bind (`ro`, `nosuid`, `noexec`, `noatime`) is a userspace remount trick, not atomic, and a read-only bind leaves the original path writable.
**Examples:** bind

### mount.opt.rbind
**Label:** rbind
**Summary:** Recursive bind: brings nested submounts across too.
**Detail:** Same as `bind` but the whole subtree, including every filesystem mounted underneath the source, appears at the target. Use it when the source is a mount point with children (a container rootfs, `/srv` with its own per-dataset mounts) and a plain `bind` would silently give you an empty-looking directory. Note that mount flags are not applied recursively by the classic syscall — `rbind,ro` does not make the children read-only.
**Examples:** rbind

### mount.opt.x_systemd_automount
**Label:** x-systemd.automount
**Summary:** fstab-only. In a unit file it is ignored — use the companion .automount instead.
**Detail:** In `/etc/fstab` this makes `systemd-fstab-generator` emit an `.automount` unit alongside the mount. In a `.mount` unit file nothing generates anything, and the option is simply passed to `mount(8)` and discarded — verified on this host: a unit carrying it produced no automount unit at all. notcron's "Companion .automount" toggle writes the real unit and is the only thing that works here. Note also that when automounting is in play, `auto`/`noauto` become meaningless.
**Examples:** x-systemd.automount (fstab only)

### mount.opt.x_systemd_idle_timeout
**Label:** x-systemd.idle-timeout= *(value)*
**Summary:** fstab-only spelling of TimeoutIdleSec=. Use the automount field in the builder.
**Detail:** Sets the idle timeout of the automount unit the fstab generator creates. Since the generator is not involved in a unit file, this option does nothing here; the equivalent is `TimeoutIdleSec=` in the `[Automount]` section, which notcron exposes directly. Kept in the menu only so it is recognisable when reading someone else's fstab.
**Examples:** x-systemd.idle-timeout=120 (fstab only)

### mount.opt.x_systemd_device_timeout
**Label:** x-systemd.device-timeout= *(value)*
**Summary:** fstab-only. How long to wait for the backing device; ignored in a unit file.
**Detail:** The man page states outright that this option can only be used in `/etc/fstab` and is ignored as part of `Options=` in a unit file. To bound device waiting from a unit, put `TimeoutSec=` in the `[Mount]` section (manual block) for the mount command itself, or add an explicit `After=`/`Requires=` on the `.device` unit. The default device wait comes from `DefaultDeviceTimeoutSec=`.
**Examples:** x-systemd.device-timeout=10s (fstab only)

### mount.opt.x_systemd_requires
**Label:** x-systemd.requires= *(value)*
**Summary:** fstab-only. Adds Requires=+After= on another unit; ignored in a unit file.
**Detail:** Handy in fstab to make a mount wait for a decryption service, an overlay's lower mount, or an external journal device. In a `.mount` unit it has no effect — probed on this host, the option produced no extra dependency. Write the real thing in the manual block instead: a `[Unit]` section with `Requires=` and `After=` lines, which is both explicit and visible to `systemctl show`.
**Examples:** x-systemd.requires=/mnt/lower (fstab only)

### mount.opt._netdev
**Label:** _netdev
**Summary:** Force "this is a network mount" when the fs type does not say so. Works in unit files.
**Detail:** Normally systemd infers network-ness from the filesystem type; `_netdev` overrides that for cases the type cannot express — iSCSI or DRBD-backed block devices, for instance. It moves the mount from `local-fs-pre.target`/`local-fs.target` to `remote-fs-pre.target`/`remote-fs.target`, and pulls in and orders after `network-online.target`. Confirmed effective from a unit file's `Options=` on this host. Do not confuse it with `nofail`: `_netdev` changes *when* the mount is attempted, `nofail` changes *whether the boot cares* if it fails — flaky remote shares usually want both.
**Examples:** _netdev

## Schedule presets

### schedule.every_minutes
**Label:** Every N minutes
**Summary:** Monotonic interval timer: OnBootSec=Nmin plus OnUnitActiveSec=Nmin.
**Detail:** Not a calendar timer — the clock runs from boot for the first firing and from the last *activation* of the service for every one after, so a job that takes longer than the interval simply runs back to back rather than overlapping. Monotonic timers pause across suspend (unless `WakeSystem=` is set), and `Persistent=` does not apply, so notcron omits it. Accepts 1..1440.
**Examples:** 15 → OnBootSec=15min, OnUnitActiveSec=15min

### schedule.every_hours
**Label:** Every N hours
**Summary:** Same monotonic interval, in hours: OnBootSec=Nh plus OnUnitActiveSec=Nh.
**Detail:** Identical semantics to the minutes preset, just a coarser unit; accepts 1..168 (one week). Because the interval is measured from the last activation rather than the wall clock, the firing time drifts across reboots — if you need "06:00 every day" rather than "every 24 hours", use the daily preset instead.
**Examples:** 6 → OnBootSec=6h, OnUnitActiveSec=6h

### schedule.daily
**Label:** Daily at HH:MM
**Summary:** Realtime calendar timer: OnCalendar=*-*-* HH:MM:00.
**Detail:** Fires at the given local wall-clock time every day, which means it follows DST — the 02:30 job either runs twice or not at all on the switch days. Because it is a calendar timer, `Persistent=` works, so a run missed while the machine was off is caught up on the next start. Enter the time as 24-hour `HH:MM`.
**Examples:** 03:00 → *-*-* 03:00:00

### schedule.weekly
**Label:** Weekly on a weekday at HH:MM
**Summary:** OnCalendar=Day *-*-* HH:MM:00, where Day is Mon..Sun.
**Detail:** The weekday prefix is a filter on top of a daily spec, not a separate mechanism, so `Sat *-*-* 04:00:00` reads "every day at 04:00, but only Saturdays". Multiple days and ranges are legal in raw `OnCalendar` form (`Mon..Fri`, `Mon,Thu`) if you need them — the preset offers one day, the raw entry offers the rest.
**Examples:** Sun 03:30 → Sun *-*-* 03:30:00

### schedule.monthly
**Label:** Monthly on a day of the month at HH:MM
**Summary:** OnCalendar=*-*-DD HH:MM:00 for a fixed day number.
**Detail:** Day 1..31. Beware the tail of the range: a spec for day 31 simply does not fire in months that lack it, and day 29 skips most Februaries — `*-*-01` is the only day number that never misses. For "last day of month", write a raw spec with `~` (`*-*~01`), which the preset does not generate.
**Examples:** 1 at 05:00 → *-*-01 05:00:00

### schedule.boot
**Label:** At boot, after a delay
**Summary:** One-shot monotonic timer: OnBootSec= only, no repeat.
**Detail:** Fires once, the given time after boot. If the delay is already in the past when the timer unit is activated — which is the normal case for a user manager that starts at login, or for a unit enabled long after boot — it elapses immediately. In a user unit consider `OnStartupSec=` in the manual block instead, since that measures from the user manager's own start rather than the machine's.
**Examples:** 1min, 30s, 2h

### schedule.cron
**Label:** Cron expression (translated to OnCalendar)
**Summary:** Five cron fields or an @shorthand, converted to an equivalent OnCalendar spec.
**Detail:** Accepts the usual `minute hour day-of-month month day-of-week`, including `*/N` steps, ranges, lists and name forms, plus `@hourly`/`@daily`/`@weekly`/`@monthly`/`@yearly`/`@reboot`. The translation is one-way: what gets stored is the resulting `OnCalendar=` line, and the original expression is kept only as a comment. `@reboot` has no calendar equivalent and becomes `OnBootSec=1min`. Every result is validated with `systemd-analyze calendar` before it can be saved.
**Examples:** 0 3 * * *, */15 * * * *, 0 4 * * mon-fri, @daily

### schedule.oncalendar
**Label:** Raw OnCalendar spec
**Summary:** Type a systemd calendar expression directly; nothing is translated.
**Detail:** The full `systemd.time(7)` grammar: `DayOfWeek Year-Month-Day Hour:Minute:Second`, with `*`, ranges (`Mon..Fri`, `9..17`), lists, `/N` steps, `~` for days counted back from month end, and shorthands like `daily`, `hourly`, `weekly`. Sub-second and timezone suffixes are accepted too (`03:00:00 Europe/Warsaw`). Validated via `systemd-analyze calendar`, whose normalised form and next elapse show up in the status line.
**Examples:** Mon..Fri *-*-* 09:00:00, *-*-* 00/6:00:00, *-*~01 23:59:00, hourly

## Lifecycle actions

### lifecycle.enable
**Label:** Enable (a)
**Summary:** Symlink the unit into its [Install] target so it comes back after a reboot.
**Detail:** Acts on the primary unit — the `.timer`, the standalone `.service`, or the `.automount` — never on a timer's paired service, which has no `[Install]` section by design. Enabling does not start anything now; it only decides what happens at next boot. For user units this additionally requires lingering to be on, or nothing starts until you log in. A unit with no `[Install]` section produces a warning rather than a failure.
**Examples:** systemctl --user enable notcron-backup.timer

### lifecycle.disable
**Label:** Disable (d)
**Summary:** Remove the enable symlinks. Does not stop what is already running.
**Detail:** The mirror of enable: the unit will not come up at next boot, but a currently active timer keeps firing and a running service keeps running until you stop it. `disable --now` does both; notcron's remove path uses that form. Disabling a `.timer` leaves the `.service` file in place and runnable by hand, which is often exactly what you want while debugging.
**Examples:** systemctl --user disable notcron-backup.timer

### lifecycle.start
**Label:** Start (s)
**Summary:** Activate the unit right now, for this boot only.
**Detail:** On a `.timer` this arms the schedule — it does not run the job; to run the job now, start the `.service` instead. On an `.automount` it installs the autofs trigger without mounting. On a standalone service it launches the process. Starting a `Type=oneshot` service blocks until the command finishes, so a slow job makes the UI wait; check the journal (`l`) for what it did.
**Examples:** systemctl --user start notcron-backup.timer

### lifecycle.stop
**Label:** Stop (S)
**Summary:** Deactivate now. Enablement is untouched, so it returns after a reboot.
**Detail:** Stopping a `.timer` disarms the schedule but leaves any in-flight run alone — the job it already triggered continues. Stopping a `.mount` unmounts (and fails if the filesystem is busy). A stop is not a failure: `Restart=` explicitly does not fire when the manager itself stopped the unit.
**Examples:** systemctl --user stop notcron-backup.timer

### lifecycle.daemon_reload
**Label:** Reload systemd (r)
**Summary:** Re-read unit files from disk. Required after any edit outside notcron.
**Detail:** Rebuilds the manager's in-memory view of every unit; without it, systemd keeps running the version it parsed earlier and your edit appears to do nothing. It does not restart anything, but it does re-resolve dependencies and re-arm timers against the new configuration. notcron runs it automatically after installing or removing a unit, so this key is for changes you made by hand. Scope-specific: reloading `--user` does not touch the system manager.
**Examples:** systemctl --user daemon-reload

### lifecycle.remove
**Label:** Remove (x)
**Summary:** Stop, disable, delete every file of the unit, then reload.
**Detail:** Best-effort on the systemctl steps, so a unit that was never enabled still deletes cleanly. Removes both files of a pair — timer and service, or automount and mount. It does not clean up side effects: a `Persistent=` stamp under `/var/lib/systemd/timers` or `~/.local/share/systemd/timers` survives, so run `systemctl clean --what=state` on the timer *before* removing it if the name might be reused.
**Examples:** (no value)

### lifecycle.status
**Label:** Status (i)
**Summary:** systemctl status for the unit: active state, last result, recent log lines.
**Detail:** The first place to look. For a timer-driven job remember the state you care about is on the `.service`, not the `.timer` — and a successful `Type=oneshot` service correctly reads `inactive (dead)` with `status=0/SUCCESS`, which is not an error. `systemctl list-timers` is the complementary view for "when does this fire next".
**Examples:** (no value)

### lifecycle.logs
**Label:** Logs (l)
**Summary:** journalctl for the service unit, which is where the job's output lands.
**Detail:** notcron sets `StandardOutput=journal`, `StandardError=journal` and a per-job `SyslogIdentifier=`, so stdout and stderr from the command are captured with no redirection on your part — this is the main operational advantage over cron's mail-if-you-are-lucky model. Logs are always read from the `.service`, never the `.timer`. For user units the journal is per-user; add `--user` (or use this action) rather than searching the system journal.
**Examples:** (no value)

### lifecycle.view
**Label:** View files (v)
**Summary:** Show the unit files on disk as they currently are.
**Detail:** Reads back from the unit directory rather than re-rendering the model, so it is the way to spot drift — a hand edit, a leftover from an older notcron version, or a drop-in that is not in the file at all. Note that drop-ins under `<unit>.d/` are not shown here; `systemctl cat` is the complete picture.
**Examples:** (no value)
