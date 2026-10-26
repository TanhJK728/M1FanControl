//! Cross-instance service intent and crash retry journal. No hardware access.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::smc::{Error, Result};

const INSTALLED: &str = "/Library/Application Support/M1FanControl";
const RUNTIME: &str = "/private/var/run/m1fan";
const HEALTHY_PERIOD: Duration = Duration::from_secs(600);
const MAX_BYTES: u64 = 4096;

fn io_error(context: &str, error: std::io::Error) -> Error {
    Error::Io(format!("service state {context}: {error}"))
}

fn directory(path: &Path, uid: u32, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io_error("directory", e))?;
    if !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
        || (private && metadata.mode() & 0o777 != 0o700)
    {
        return Err(Error::Invalid(format!(
            "unsafe service state directory {} (owner/type/permissions)",
            path.display()
        )));
    }
    Ok(())
}

fn open_file(path: &Path, uid: u32, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io_error("open", e))?;
    let metadata = file.metadata().map_err(|e| io_error("metadata", e))?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(Error::Invalid(format!(
            "unsafe service state file {} (owner/type/links/permissions)",
            path.display()
        )));
    }
    Ok(file)
}

fn read_file(path: &Path, uid: u32) -> Result<String> {
    let file = open_file(path, uid, false)?;
    let mut bytes = String::new();
    file.take(MAX_BYTES + 1)
        .read_to_string(&mut bytes)
        .map_err(|e| io_error("read", e))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::Invalid(
            "service state file exceeds 4096 bytes".into(),
        ));
    }
    Ok(bytes)
}

fn enabled_at(installed: &Path, uid: u32) -> Result<bool> {
    directory(installed, uid, false)?;
    match read_file(&installed.join("service.enabled"), uid)?.as_str() {
        "enabled\n" => Ok(true),
        "disabled\n" => Ok(false),
        _ => Err(Error::Invalid(
            "invalid installed service.enabled marker".into(),
        )),
    }
}

/// Check the fixed root-protected installation intent without opening the
/// restart journal. Each transport write can use this independently of its
/// observer's mutable ServiceState borrow.
pub fn installed_enabled() -> Result<bool> {
    for parent in ["/", "/Library", "/Library/Application Support"] {
        directory(Path::new(parent), 0, false)?;
    }
    enabled_at(Path::new(INSTALLED), 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastExit {
    Running,
    Failed,
    Clean,
}

fn parse_journal(text: &str) -> Result<(u8, LastExit, bool)> {
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 5 || lines[0] != "M1FAN_RESTART_V2" || !text.ends_with('\n') {
        return Err(Error::Invalid("invalid service restart journal".into()));
    }
    let level = lines[1]
        .strip_prefix("level=")
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value <= 4)
        .ok_or_else(|| Error::Invalid("invalid service restart level".into()))?;
    let state = match lines[2] {
        "state=running" => LastExit::Running,
        "state=failed" => LastExit::Failed,
        "state=clean" => LastExit::Clean,
        _ => return Err(Error::Invalid("invalid service restart disposition".into())),
    };
    let control_pending = match lines[3] {
        "control_pending=0" => false,
        "control_pending=1" => true,
        _ => {
            return Err(Error::Invalid(
                "invalid service recovery responsibility".into(),
            ));
        }
    };
    if state == LastExit::Clean && control_pending {
        return Err(Error::Invalid(
            "clean journal has unresolved control responsibility".into(),
        ));
    }
    let instance = lines[4].strip_prefix("instance=").unwrap_or("");
    if instance.is_empty()
        || instance.len() > 96
        || !instance.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return Err(Error::Invalid("invalid service restart instance".into()));
    }
    Ok((level, state, control_pending))
}

pub struct ServiceState {
    installed: PathBuf,
    runtime: PathBuf,
    uid: u32,
    _lock: File,
    instance: String,
    level: u8,
    delay: Duration,
    unclean: bool,
    control_pending: bool,
    healthy_since: Option<Instant>,
    finished: bool,
}

impl ServiceState {
    pub fn begin() -> Result<Self> {
        crate::platform::require_root()?;
        // Avoid an intermediate symlink or writable installation ancestor.
        for parent in ["/", "/Library", "/Library/Application Support"] {
            directory(Path::new(parent), 0, false)?;
        }
        Self::begin_at(Path::new(INSTALLED), Path::new(RUNTIME), 0)
    }

    /// Explicit paths/UID for isolated tests. Production callers use `begin`;
    /// no environment variable can change service intent or journal paths.
    #[doc(hidden)]
    pub fn begin_at(installed: &Path, runtime: &Path, uid: u32) -> Result<Self> {
        if unsafe { libc::geteuid() } != uid {
            return Err(Error::Permission(
                "service state UID differs from caller".into(),
            ));
        }
        directory(installed, uid, false)?;
        match fs::DirBuilder::new().mode(0o700).create(runtime) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error("create runtime", e)),
        }
        directory(runtime, uid, true)?;
        let lock = open_file(&runtime.join("restart.lock"), uid, true)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::Control(format!(
                "another service instance owns the restart journal: {}",
                std::io::Error::last_os_error()
            )));
        }
        let journal_path = runtime.join("restart.state");
        let previous = match fs::symlink_metadata(&journal_path) {
            Ok(_) => Some(parse_journal(&read_file(&journal_path, uid)?)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_error("journal metadata", e)),
        };
        let unclean = previous.is_some_and(|(_, state, _)| state != LastExit::Clean);
        let level = match previous {
            Some((level, state, _)) if state != LastExit::Clean => level.saturating_add(1).min(4),
            _ => 0,
        };
        let instance = format!(
            "{:x}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| Error::Invalid("system time predates Unix epoch".into()))?
                .as_nanos()
        );
        let state = Self {
            installed: installed.to_path_buf(),
            runtime: runtime.to_path_buf(),
            uid,
            _lock: lock,
            instance,
            level,
            delay: Duration::from_secs(match level {
                0 => 0,
                1 => 5,
                2 => 15,
                3 => 30,
                _ => 60,
            }),
            unclean,
            control_pending: previous.is_some_and(|(_, _, pending)| pending),
            healthy_since: None,
            finished: false,
        };
        state.persist(LastExit::Running)?;
        Ok(state)
    }

    pub fn restart_delay(&self) -> Duration {
        self.delay
    }

    pub fn was_unclean(&self) -> bool {
        self.unclean
    }

    /// Hardware restoration responsibility is separate from process failure.
    /// A read-only startup crash must never authorize overriding foreign manual mode.
    pub fn recovery_pending(&self) -> bool {
        self.control_pending
    }

    /// Set true durably before the first possible control write. Clear only after
    /// automatic ownership has been verified on the real control path.
    pub fn set_recovery_pending(&mut self, pending: bool) -> Result<()> {
        if self.finished {
            return Err(Error::Invalid("service journal already finished".into()));
        }
        if self.control_pending == pending {
            return Ok(());
        }
        let previous = self.control_pending;
        self.control_pending = pending;
        let result = self.persist(LastExit::Running);
        if result.is_err() {
            self.control_pending = previous;
        }
        result
    }

    pub fn instance_id(&self) -> &str {
        &self.instance
    }

    /// Missing, malformed or unsafe intent fails closed. The caller must cease
    /// control, restore automatic mode, and avoid further acquisition.
    pub fn enabled(&self) -> Result<bool> {
        enabled_at(&self.installed, self.uid)
    }

    pub fn healthy_tick(&mut self) -> Result<()> {
        self.healthy_tick_at(Instant::now())
    }

    fn healthy_tick_at(&mut self, now: Instant) -> Result<()> {
        if self.finished {
            return Err(Error::Invalid("service journal already finished".into()));
        }
        let start = *self.healthy_since.get_or_insert(now);
        if self.level != 0 && now.saturating_duration_since(start) >= HEALTHY_PERIOD {
            let previous = self.level;
            self.level = 0;
            if let Err(error) = self.persist(LastExit::Running) {
                self.level = previous;
                return Err(error);
            }
        }
        Ok(())
    }

    pub fn unhealthy(&mut self) {
        self.healthy_since = None;
    }

    pub fn finish(&mut self, clean: bool) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let previous = (self.level, self.control_pending);
        if clean {
            self.level = 0;
            self.control_pending = false;
        }
        if let Err(error) = self.persist(if clean {
            LastExit::Clean
        } else {
            LastExit::Failed
        }) {
            (self.level, self.control_pending) = previous;
            return Err(error);
        }
        self.finished = true;
        Ok(())
    }

    fn persist(&self, state: LastExit) -> Result<()> {
        directory(&self.runtime, self.uid, true)?;
        let path = self.runtime.join("restart.state");
        // Validate an existing destination before replacement. Runtime is private
        // and the lifetime journal lock serializes trusted writers.
        if fs::symlink_metadata(&path).is_ok() {
            open_file(&path, self.uid, false)?;
        }
        let temporary = self.runtime.join(format!("restart.{}.new", self.instance));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .map_err(|e| io_error("create journal", e))?;
        let result = (|| {
            let state = match state {
                LastExit::Running => "running",
                LastExit::Failed => "failed",
                LastExit::Clean => "clean",
            };
            writeln!(
                file,
                "M1FAN_RESTART_V2\nlevel={}\nstate={}\ncontrol_pending={}\ninstance={}",
                self.level,
                state,
                u8::from(self.control_pending),
                self.instance
            )
            .map_err(|e| io_error("write journal", e))?;
            file.sync_all().map_err(|e| io_error("sync journal", e))?;
            fs::rename(&temporary, &path).map_err(|e| io_error("publish journal", e))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_parser_rejects_corruption_and_unknown_schema() {
        for text in [
            "",
            "M1FAN_RESTART_V1\nlevel=1\nstate=running\ninstance=ab\n",
            "M1FAN_RESTART_V2\nlevel=99\nstate=running\ncontrol_pending=0\ninstance=ab\n",
            "M1FAN_RESTART_V2\nlevel=1\nstate=oops\ncontrol_pending=0\ninstance=ab\n",
            "M1FAN_RESTART_V2\nlevel=1\nstate=running\ncontrol_pending=0\ninstance=../../file\n",
            "M1FAN_RESTART_V2\nlevel=1\nstate=clean\ncontrol_pending=1\ninstance=ab\n",
            "M1FAN_RESTART_V2\nlevel=1\nstate=running\ncontrol_pending=unknown\ninstance=ab\n",
        ] {
            assert!(parse_journal(text).is_err());
        }
    }

    #[test]
    fn ten_continuous_healthy_minutes_clear_budget_and_unhealthy_resets_timer() {
        let path = std::env::temp_dir().join(format!(
            "m1fan-journal-clock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let app = path.join("app");
        fs::create_dir(&app).unwrap();
        let runtime = path.join("runtime");
        let uid = unsafe { libc::geteuid() };
        drop(ServiceState::begin_at(&app, &runtime, uid).unwrap());
        let mut state = ServiceState::begin_at(&app, &runtime, uid).unwrap();
        assert_eq!(state.level, 1);
        state.set_recovery_pending(true).unwrap();
        let now = Instant::now();
        state.healthy_tick_at(now).unwrap();
        state
            .healthy_tick_at(now + Duration::from_secs(599))
            .unwrap();
        assert_eq!(state.level, 1);
        state.unhealthy();
        state
            .healthy_tick_at(now + Duration::from_secs(600))
            .unwrap();
        assert_eq!(state.level, 1);
        state
            .healthy_tick_at(now + Duration::from_secs(1200))
            .unwrap();
        assert_eq!(state.level, 0);
        assert!(state.recovery_pending());
        drop(state);
        let state = ServiceState::begin_at(&app, &runtime, uid).unwrap();
        assert_eq!(state.restart_delay(), Duration::from_secs(5));
        drop(state);
        fs::remove_dir_all(path).unwrap();
    }
}
