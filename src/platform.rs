use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::control::Clock;
use crate::smc::{Error, Result};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn install_signals() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // Only an atomic flag is touched inside the signal handler.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = signal_handler as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(Error::Io(format!(
                "install signal {signal}: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    // Convert closed stdout pipes into a normal Result so the guard can restore.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    Ok(())
}

pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

pub struct RealClock;
impl Clock for RealClock {
    fn now(&self) -> Duration {
        continuous_time()
    }
    fn wait(&self, duration: Duration) -> bool {
        let end = Instant::now() + duration;
        loop {
            if stopped() {
                return false;
            }
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }
}

/// Unlike an awake-only uptime clock, this includes system sleep. A long pause
/// makes the adaptive controller release ownership instead of using old samples.
#[cfg(target_os = "macos")]
fn continuous_time() -> Duration {
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    }
    static SCALE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    let (numer, denom) = *SCALE.get_or_init(|| {
        let mut info = Timebase { numer: 0, denom: 0 };
        let result = unsafe { mach_timebase_info(&mut info) };
        assert!(result == 0 && info.denom != 0, "mach_timebase_info failed");
        (info.numer, info.denom)
    });
    let nanos =
        u128::from(unsafe { mach_continuous_time() }) * u128::from(numer) / u128::from(denom);
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

#[cfg(not(target_os = "macos"))]
fn continuous_time() -> Duration {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed()
}

pub struct Hardware {
    pub model: String,
    pub chip: String,
    pub os: String,
}

fn command(path: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(path)
        .args(args)
        .output()
        .map_err(|e| Error::Io(format!("{path}: {e}")))?;
    if !output.status.success() {
        return Err(Error::Io(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Hardware {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            model: command("/usr/sbin/sysctl", &["-n", "hw.model"])?,
            chip: command("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"])?,
            os: command("/usr/bin/sw_vers", &["-productVersion"])?,
        })
    }
    pub fn require_m1(&self) -> Result<()> {
        if !cfg!(all(target_os = "macos", target_arch = "aarch64")) || self.chip != "Apple M1" {
            return Err(Error::Invalid(format!(
                "writes support Apple M1 only; detected {} / {}",
                self.model, self.chip
            )));
        }
        Ok(())
    }
}

pub fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(Error::Permission(
            "control/start/stop/daemon-status require sudo; status/monitor/preview need no sudo"
                .into(),
        ));
    }
    Ok(())
}

pub struct ControlLock {
    _file: File,
}
impl ControlLock {
    pub fn acquire() -> Result<Self> {
        require_root()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/var/run/m1fan-control.lock")
            .map_err(|e| Error::Io(format!("open control lock: {e}")))?;
        let meta = file.metadata().map_err(|e| Error::Io(e.to_string()))?;
        if !meta.is_file() || meta.uid() != 0 || meta.nlink() != 1 || meta.mode() & 0o022 != 0 {
            return Err(Error::Invalid(
                "unsafe control lock ownership/type/permissions".into(),
            ));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::Io(
                "another m1fan controller owns control; use sudo m1fan stop for a background controller, or Ctrl+C for foreground control".into(),
            ));
        }
        Ok(Self { _file: file })
    }
}
