//! Explicitly opt-in passive native ABI/lifecycle check. Normal cargo test uses
//! the pure event-state unit tests in src/power.rs and never registers with IOKit.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "passive native IOKit registration; run explicitly as an ordinary user"]
fn passive_notification_registration_and_cleanup() {
    use m1fan::power::{NativePower, PowerEvent, PowerSource};
    use std::time::{Duration, Instant};

    let uid = unsafe { libc::geteuid() };
    assert_ne!(uid, 0, "run this passive check without sudo");
    let started = Instant::now();
    let source = NativePower::open().expect("register passive system power notifications");
    std::thread::sleep(Duration::from_millis(200));
    let events = source.drain();
    println!(
        "uid={uid} passive_registration=ok gate={} generation={} stats={:?} events={events:?}",
        source.sleep_pending(),
        source.generation(),
        source.stats()
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, PowerEvent::Error(_)))
    );
    drop(source);
    println!(
        "notification_thread_joined=true elapsed_ms={}",
        started.elapsed().as_millis()
    );
}
