//! Pure filesystem lifecycle tests; no service, root privileges or hardware.
use m1fan::service_state::ServiceState;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    base: PathBuf,
    app: PathBuf,
    runtime: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "m1fan-service-state-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let app = base.join("Application Support");
        fs::create_dir(&app).unwrap();
        let runtime = base.join("runtime");
        let fixture = Self { base, app, runtime };
        fixture.marker("enabled\n");
        fixture
    }
    fn marker(&self, value: &str) {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.app.join("service.enabled"))
            .unwrap();
        file.write_all(value.as_bytes()).unwrap();
    }
    fn begin(&self) -> m1fan::smc::Result<ServiceState> {
        ServiceState::begin_at(&self.app, &self.runtime, unsafe { libc::geteuid() })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).unwrap();
    }
}

#[test]
fn abandoned_instances_have_bounded_cross_process_schedule() {
    let fixture = Fixture::new();
    let mut previous = String::new();
    for seconds in [0, 5, 15, 30, 60, 60, 60] {
        let state = fixture.begin().unwrap();
        assert_eq!(state.restart_delay(), Duration::from_secs(seconds));
        assert_eq!(state.was_unclean(), seconds != 0);
        assert!(
            !state.recovery_pending(),
            "read-only crash must not claim manual ownership"
        );
        assert_ne!(state.instance_id(), previous);
        previous = state.instance_id().to_owned();
        // Drop deliberately leaves running evidence, equivalent to crash death.
    }
}

#[test]
fn failure_counts_once_and_verified_clean_exit_resets() {
    let fixture = Fixture::new();
    let mut state = fixture.begin().unwrap();
    state.finish(false).unwrap();
    drop(state);
    let mut state = fixture.begin().unwrap();
    assert_eq!(state.restart_delay(), Duration::from_secs(5));
    state.finish(true).unwrap();
    state.finish(false).unwrap(); // terminal operation is idempotent
    drop(state);
    let state = fixture.begin().unwrap();
    assert_eq!(state.restart_delay(), Duration::ZERO);
    assert!(!state.was_unclean());
}

#[test]
fn persistent_intent_is_rechecked_and_corruption_fails_closed() {
    let fixture = Fixture::new();
    let state = fixture.begin().unwrap();
    assert!(state.enabled().unwrap());
    fixture.marker("disabled\n");
    assert!(!state.enabled().unwrap());
    fixture.marker("enabled\nextra\n");
    assert!(state.enabled().is_err());
    fs::remove_file(fixture.app.join("service.enabled")).unwrap();
    assert!(state.enabled().is_err());
}

#[test]
fn journal_lifetime_lock_rejects_duplicate_service_instances() {
    let fixture = Fixture::new();
    let first = fixture.begin().unwrap();
    let before = fs::read(fixture.runtime.join("restart.state")).unwrap();
    assert!(fixture.begin().is_err());
    assert_eq!(
        fs::read(fixture.runtime.join("restart.state")).unwrap(),
        before
    );
    drop(first);
    assert!(fixture.begin().is_ok());
}

#[test]
fn unsafe_marker_links_and_permissions_are_rejected() {
    let fixture = Fixture::new();
    let state = fixture.begin().unwrap();
    let marker = fixture.app.join("service.enabled");
    let outside = fixture.base.join("outside");
    fs::rename(&marker, &outside).unwrap();
    symlink(&outside, &marker).unwrap();
    assert!(state.enabled().is_err());
    fs::remove_file(&marker).unwrap();
    fs::hard_link(&outside, &marker).unwrap();
    assert!(state.enabled().is_err());
    fs::remove_file(&marker).unwrap();
    fs::rename(&outside, &marker).unwrap();
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(state.enabled().is_err());
}

#[test]
fn corrupted_journal_is_preserved_without_replacement() {
    let fixture = Fixture::new();
    drop(fixture.begin().unwrap());
    let path = fixture.runtime.join("restart.state");
    fs::write(&path, "corrupt unique evidence\n").unwrap();
    assert!(fixture.begin().is_err());
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "corrupt unique evidence\n"
    );
}

#[test]
fn unsafe_runtime_and_wrong_uid_are_rejected() {
    let fixture = Fixture::new();
    assert!(ServiceState::begin_at(&fixture.app, &fixture.runtime, u32::MAX).is_err());
    assert!(!fixture.runtime.exists());
    fs::create_dir(&fixture.runtime).unwrap();
    fs::set_permissions(&fixture.runtime, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(fixture.begin().is_err());
    fs::remove_dir(&fixture.runtime).unwrap();
    symlink(&fixture.app, &fixture.runtime).unwrap();
    assert!(fixture.begin().is_err());
}

#[test]
fn armed_control_responsibility_survives_crash_and_failed_stop() {
    let fixture = Fixture::new();
    let mut state = fixture.begin().unwrap();
    state.set_recovery_pending(true).unwrap();
    drop(state);
    let mut state = fixture.begin().unwrap();
    assert!(state.recovery_pending());
    fixture.marker("disabled\n");
    assert!(!state.enabled().unwrap());
    state.finish(false).unwrap();
    drop(state);
    let mut state = fixture.begin().unwrap();
    assert!(
        state.recovery_pending(),
        "disabled startup/failed stop retains responsibility"
    );
    state.set_recovery_pending(false).unwrap();
    drop(state);
    let state = fixture.begin().unwrap();
    assert!(
        !state.recovery_pending(),
        "verified automatic release remains clear after crash"
    );
}

#[test]
fn successful_clean_exit_clears_responsibility_and_retry_budget() {
    let fixture = Fixture::new();
    let mut state = fixture.begin().unwrap();
    state.set_recovery_pending(true).unwrap();
    state.finish(true).unwrap();
    assert!(state.set_recovery_pending(true).is_err());
    drop(state);
    let state = fixture.begin().unwrap();
    assert!(!state.recovery_pending());
    assert_eq!(state.restart_delay(), Duration::ZERO);
}

#[test]
fn failed_durable_arming_does_not_report_in_memory_responsibility() {
    let fixture = Fixture::new();
    let mut state = fixture.begin().unwrap();
    let temporary = fixture
        .runtime
        .join(format!("restart.{}.new", state.instance_id()));
    fs::write(&temporary, "occupied unique file").unwrap();
    assert!(state.set_recovery_pending(true).is_err());
    assert!(!state.recovery_pending());
    assert_eq!(
        fs::read_to_string(temporary).unwrap(),
        "occupied unique file"
    );
}
