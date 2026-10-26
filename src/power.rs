//! Passive system power notifications; this module never opens AppleSMC.
//!
//! ABI and ownership: macOS SDK IOPMLib.h:127-224, IOKitLib.h:90-103,
//! 159-183, IOMessage.h:130-175, and CoreFoundation/CFRunLoop.h:24-86.
//! IOAllowPowerChange takes intptr_t; messageArgument contains the token itself.
//! The notification port owns its borrowed CFRunLoopSource.
//!
//! Apple references:
//! https://developer.apple.com/library/archive/qa/qa1340/_index.html
//! https://github.com/apple-oss-distributions/IOKitUser/blob/323ead896d04424f87184d8f6ff0cce811aab106/pwr_mgt.subproj/IOPMLib.c
//! https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/iokit/Kernel/IOPMrootDomain.cpp
//!
//! CanSystemSleep is acknowledged immediately. WillSleep waits for the controller
//! to request acknowledgement, with a separate three-second deadline on the
//! notification thread. Neither notification vetoes sleep. Deadline/overflow
//! acknowledgements report what happened; they do not claim SMC restoration.
//! The callback never waits for a controller or accesses its hardware/logging.
//! On pending-token overflow only, it acknowledges the new token immediately,
//! reports the overflow, and closes the normal-write gate.
//! Token deduplication is scoped to a sleep cycle: XNU notification IDs can be
//! reused. Completed wake/cancellation advances the cycle while retaining every
//! older pending ACK until the notification thread has attempted it.

use crate::smc::{Error, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const ACK_DEADLINE: Duration = Duration::from_secs(3);
const MAX_EVENTS: usize = 128;
const MAX_PENDING: usize = 32;
const MAX_RECENT_TOKENS: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    WillSleep {
        token: isize,
    },
    WillPowerOn,
    DidWake,
    CancelledSleep,
    SleepAcknowledged {
        token: isize,
        deadline: bool,
        success: bool,
    },
    Error(String),
}

pub trait PowerSource {
    fn drain(&self) -> Vec<PowerEvent>;
    fn sleep_pending(&self) -> bool;
    fn generation(&self) -> u64;
    /// Queue an ACK request. The actual result arrives as SleepAcknowledged.
    fn acknowledge_sleep(&self, token: isize) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerStats {
    pub event_overflows: u64,
    pub pending_overflows: u64,
    pub acknowledgement_attempts: u64,
    pub acknowledgement_failures: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    CanSleep,
    WillSleep,
    WillPowerOn,
    DidWake,
    CancelledSleep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Ack {
    token: isize,
    deadline: bool,
}

struct Pending {
    cycle: u64,
    token: isize,
    deadline: Duration,
    requested: bool,
}

#[derive(Default)]
struct State {
    gate: bool,
    generation: u64,
    cycle: u64,
    last: Option<Kind>,
    pending: VecDeque<Pending>,
    recent: VecDeque<(u64, Kind, isize)>,
    events: VecDeque<PowerEvent>,
    stats: PowerStats,
    unreported_overflows: u64,
    last_ack_failure: Option<isize>,
}

impl State {
    fn bump(&mut self, gate: bool) {
        self.gate = gate;
        self.generation = self.generation.wrapping_add(1);
    }

    fn push(&mut self, event: PowerEvent) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
            self.stats.event_overflows = self.stats.event_overflows.saturating_add(1);
            self.unreported_overflows = self.unreported_overflows.saturating_add(1);
            self.bump(true);
        }
        self.events.push_back(event);
    }

    fn fail(&mut self, message: String) {
        self.bump(true);
        self.push(PowerEvent::Error(message));
    }

    fn seen(&self, kind: Kind, token: isize) -> bool {
        self.recent.contains(&(self.cycle, kind, token))
            || (kind == Kind::WillSleep
                && self
                    .pending
                    .iter()
                    .any(|p| p.cycle == self.cycle && p.token == token))
    }

    fn remember(&mut self, cycle: u64, kind: Kind, token: isize) {
        if self.recent.len() == MAX_RECENT_TOKENS {
            self.recent.pop_front();
        }
        self.recent.push_back((cycle, kind, token));
    }

    fn can_sleep(&mut self, token: isize) -> bool {
        if self.seen(Kind::CanSleep, token) {
            return false;
        }
        self.last = Some(Kind::CanSleep);
        self.remember(self.cycle, Kind::CanSleep, token);
        true
    }

    /// A returned ACK is the overflow fallback, issued by the notification thread.
    fn will_sleep(&mut self, token: isize, now: Duration) -> Option<Ack> {
        if self.seen(Kind::WillSleep, token) {
            return None;
        }
        self.last = Some(Kind::WillSleep);
        self.bump(true);
        self.push(PowerEvent::WillSleep { token });
        if self.pending.len() == MAX_PENDING {
            self.stats.pending_overflows = self.stats.pending_overflows.saturating_add(1);
            self.fail(format!(
                "power pending-token queue overflow; immediate sleep ACK for token {token}; total={}",
                self.stats.pending_overflows
            ));
            self.remember(self.cycle, Kind::WillSleep, token);
            return Some(Ack {
                token,
                deadline: false,
            });
        }
        self.pending.push_back(Pending {
            cycle: self.cycle,
            token,
            deadline: now.saturating_add(ACK_DEADLINE),
            requested: false,
        });
        None
    }

    fn transition(&mut self, kind: Kind) {
        if self.last == Some(kind) {
            return;
        }
        self.last = Some(kind);
        let event = match kind {
            Kind::WillPowerOn => {
                self.bump(true);
                PowerEvent::WillPowerOn
            }
            Kind::DidWake | Kind::CancelledSleep => {
                self.bump(false);
                // No stale token is left waiting for a controller after wake/cancel.
                for pending in &mut self.pending {
                    pending.requested = true;
                }
                // Keep old pending entries, but none can suppress the next
                // cycle's notification even when the kernel reuses its ID.
                self.cycle = self.cycle.wrapping_add(1);
                if kind == Kind::DidWake {
                    PowerEvent::DidWake
                } else {
                    PowerEvent::CancelledSleep
                }
            }
            Kind::CanSleep | Kind::WillSleep => return,
        };
        self.push(event);
    }

    fn request_ack(&mut self, token: isize) -> Result<()> {
        let mut found = false;
        for pending in self.pending.iter_mut().filter(|p| p.token == token) {
            pending.requested = true;
            found = true;
        }
        if found {
            return Ok(());
        }
        // A controller may drain an old WillSleep after wake/cancellation has
        // already requested its ACK. Its late request remains idempotent.
        if self
            .recent
            .iter()
            .any(|(_, kind, seen)| *kind == Kind::WillSleep && *seen == token)
        {
            return Ok(());
        }
        Err(Error::Invalid(format!(
            "unknown system sleep notification token {token}"
        )))
    }

    fn due(&mut self, now: Duration, stopping: bool) -> Vec<Ack> {
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            let pending = &self.pending[index];
            if stopping || pending.requested || now >= pending.deadline {
                let pending = self.pending.remove(index).expect("pending index exists");
                self.remember(pending.cycle, Kind::WillSleep, pending.token);
                due.push(Ack {
                    token: pending.token,
                    deadline: now >= pending.deadline,
                });
            } else {
                index += 1;
            }
        }
        due
    }

    fn ack_result(&mut self, ack: Ack, status: i32, sleep: bool) {
        self.stats.acknowledgement_attempts = self.stats.acknowledgement_attempts.saturating_add(1);
        if status != 0 {
            self.stats.acknowledgement_failures =
                self.stats.acknowledgement_failures.saturating_add(1);
            self.last_ack_failure = Some(ack.token);
            self.fail(format!(
                "IOAllowPowerChange failed: token={} status={:#010x} sleep={sleep} failures={}",
                ack.token, status as u32, self.stats.acknowledgement_failures
            ));
        }
        if sleep {
            self.push(PowerEvent::SleepAcknowledged {
                token: ack.token,
                deadline: ack.deadline,
                success: status == 0,
            });
        }
    }

    fn drain(&mut self) -> Vec<PowerEvent> {
        let mut events = Vec::with_capacity(self.events.len() + 1);
        if self.unreported_overflows != 0 {
            events.push(PowerEvent::Error(format!(
                "power event queue overflow: dropped={} total_dropped={} ACK_failures={} last_failed_token={:?}; normal writes gated",
                self.unreported_overflows, self.stats.event_overflows,
                self.stats.acknowledgement_failures, self.last_ack_failure
            )));
            self.unreported_overflows = 0;
        }
        events.extend(self.events.drain(..));
        events
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    gate: AtomicBool,
    generation: AtomicU64,
    stopping: AtomicBool,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn update<T>(&self, operation: impl FnOnce(&mut State) -> T) -> T {
        let mut state = self.lock();
        let result = operation(&mut state);
        if state.gate {
            self.gate.store(true, Ordering::Release);
            self.generation.store(state.generation, Ordering::Release);
        } else {
            self.generation.store(state.generation, Ordering::Release);
            self.gate.store(false, Ordering::Release);
        }
        result
    }
}

pub struct NativePower {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl NativePower {
    pub fn open() -> Result<Self> {
        native::open()
    }

    pub fn stats(&self) -> PowerStats {
        self.shared.lock().stats
    }
}

impl PowerSource for NativePower {
    fn drain(&self) -> Vec<PowerEvent> {
        self.shared.update(State::drain)
    }

    fn sleep_pending(&self) -> bool {
        self.shared.gate.load(Ordering::Acquire)
    }

    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    fn acknowledge_sleep(&self, token: isize) -> Result<()> {
        self.shared.update(|state| state.request_ack(token))
    }
}

impl Drop for NativePower {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::ptr;
    use std::sync::mpsc;

    const CAN_SLEEP: u32 = 0xe000_0270;
    const WILL_SLEEP: u32 = 0xe000_0280;
    const CANCELLED_SLEEP: u32 = 0xe000_0290;
    const DID_WAKE: u32 = 0xe000_0300;
    const WILL_POWER_ON: u32 = 0xe000_0320;
    const POLL: Duration = Duration::from_millis(50);
    type NotificationPort = *mut c_void;
    type RunLoop = *mut c_void;
    type RunLoopSource = *mut c_void;
    type Callback = unsafe extern "C" fn(*mut c_void, u32, u32, *mut c_void);

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IORegisterForSystemPower(
            refcon: *mut c_void,
            port: *mut NotificationPort,
            callback: Callback,
            notifier: *mut u32,
        ) -> u32;
        fn IOAllowPowerChange(connection: u32, token: isize) -> i32;
        fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
        fn IOServiceClose(connection: u32) -> i32;
        fn IONotificationPortDestroy(port: NotificationPort);
        fn IONotificationPortGetRunLoopSource(port: NotificationPort) -> RunLoopSource;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopGetCurrent() -> RunLoop;
        fn CFRunLoopAddSource(run_loop: RunLoop, source: RunLoopSource, mode: *const c_void);
        fn CFRunLoopRemoveSource(run_loop: RunLoop, source: RunLoopSource, mode: *const c_void);
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    }

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    struct Context {
        shared: Arc<Shared>,
        connection: u32,
        scale: Timebase,
    }

    impl Context {
        fn now(&self) -> Duration {
            let nanos = u128::from(unsafe { mach_continuous_time() })
                * u128::from(self.scale.numer)
                / u128::from(self.scale.denom);
            Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
        }

        fn acknowledge(&self, ack: Ack, sleep: bool) {
            let status = unsafe { IOAllowPowerChange(self.connection, ack.token) };
            self.shared
                .update(|state| state.ack_result(ack, status, sleep));
        }

        fn service_acks(&self, stopping: bool) {
            let due = self.shared.update(|state| state.due(self.now(), stopping));
            for ack in due {
                self.acknowledge(ack, true);
            }
        }

        fn event(&self, message: u32, token: isize) {
            match message {
                CAN_SLEEP => {
                    if self.shared.update(|state| state.can_sleep(token)) {
                        self.acknowledge(
                            Ack {
                                token,
                                deadline: false,
                            },
                            false,
                        );
                    }
                }
                WILL_SLEEP => {
                    // This mutex protects only bounded power-event data, never
                    // SMC I/O. Publish the gate and generation together with the
                    // state transition; a concurrent drain cannot reopen it.
                    if let Some(ack) = self
                        .shared
                        .update(|state| state.will_sleep(token, self.now()))
                    {
                        self.acknowledge(ack, true);
                    }
                }
                WILL_POWER_ON => self
                    .shared
                    .update(|state| state.transition(Kind::WillPowerOn)),
                DID_WAKE => self.shared.update(|state| state.transition(Kind::DidWake)),
                CANCELLED_SLEEP => self
                    .shared
                    .update(|state| state.transition(Kind::CancelledSleep)),
                _ => {}
            }
        }
    }

    unsafe extern "C" fn callback(
        refcon: *mut c_void,
        _: u32,
        message: u32,
        argument: *mut c_void,
    ) {
        // refcon points to the stable Box owned until after deregistration.
        let context = unsafe { &*refcon.cast::<Context>() };
        let token = argument as isize;
        if catch_unwind(AssertUnwindSafe(|| context.event(message, token))).is_err() {
            context
                .shared
                .update(|state| state.fail("power callback panicked; writes gated".into()));
            if message == WILL_SLEEP {
                // If it was queued before the panic, service_acks owns the ACK.
                let immediate = context.shared.update(|state| {
                    if state
                        .pending
                        .iter()
                        .any(|p| p.cycle == state.cycle && p.token == token)
                    {
                        let _ = state.request_ack(token);
                        false
                    } else if state.seen(Kind::WillSleep, token) {
                        false
                    } else {
                        state.remember(state.cycle, Kind::WillSleep, token);
                        true
                    }
                });
                if immediate {
                    context.acknowledge(
                        Ack {
                            token,
                            deadline: false,
                        },
                        true,
                    );
                }
            }
        }
    }

    struct Registration {
        context: Box<Context>,
        port: NotificationPort,
        notifier: u32,
        run_loop: RunLoop,
        source: RunLoopSource,
    }

    impl Registration {
        fn open(shared: Arc<Shared>) -> Result<Self> {
            let mut scale = Timebase { numer: 0, denom: 0 };
            let status = unsafe { mach_timebase_info(&mut scale) };
            if status != 0 || scale.numer == 0 || scale.denom == 0 {
                return Err(Error::Io(format!(
                    "power clock timebase unavailable: {status}"
                )));
            }
            let mut context = Box::new(Context {
                shared,
                connection: 0,
                scale,
            });
            let mut port = ptr::null_mut();
            let mut notifier = 0;
            let connection = unsafe {
                IORegisterForSystemPower(
                    ptr::from_mut(context.as_mut()).cast(),
                    &mut port,
                    callback,
                    &mut notifier,
                )
            };
            if connection == 0 {
                // IOPMLib owns partial-registration cleanup on its failure path.
                return Err(Error::Io("IORegisterForSystemPower failed".into()));
            }
            context.connection = connection;
            let mut registration = Self {
                context,
                port,
                notifier,
                run_loop: unsafe { CFRunLoopGetCurrent() },
                source: ptr::null_mut(),
            };
            if registration.port.is_null() || registration.notifier == 0 {
                return Err(Error::Io(
                    "incomplete system power notification registration".into(),
                ));
            }
            registration.source = unsafe { IONotificationPortGetRunLoopSource(registration.port) };
            if registration.source.is_null() || registration.run_loop.is_null() {
                return Err(Error::Io(
                    "system power notification run-loop source unavailable".into(),
                ));
            }
            unsafe {
                CFRunLoopAddSource(
                    registration.run_loop,
                    registration.source,
                    kCFRunLoopDefaultMode,
                )
            };
            Ok(registration)
        }

        fn run(&self) -> Result<()> {
            while !self.context.shared.stopping.load(Ordering::Acquire) {
                self.context.service_acks(false);
                let now = self.context.now();
                let wait = self.context.shared.update(|state| {
                    state
                        .pending
                        .iter()
                        .map(|p| {
                            if p.requested {
                                Duration::ZERO
                            } else {
                                p.deadline.saturating_sub(now)
                            }
                        })
                        .min()
                        .unwrap_or(POLL)
                        .min(POLL)
                });
                let result =
                    unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, wait.as_secs_f64(), 1) };
                if !matches!(result, 3 | 4) {
                    return Err(Error::Io(format!(
                        "system power notification run loop exited: {result}"
                    )));
                }
            }
            Ok(())
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            self.context.service_acks(true);
            if !self.source.is_null() && !self.run_loop.is_null() {
                unsafe { CFRunLoopRemoveSource(self.run_loop, self.source, kCFRunLoopDefaultMode) };
            }
            let deregister = unsafe { IODeregisterForSystemPower(&mut self.notifier) };
            let close = unsafe { IOServiceClose(self.context.connection) };
            if !self.port.is_null() {
                unsafe { IONotificationPortDestroy(self.port) };
            }
            if deregister != 0 || close != 0 {
                self.context.shared.update(|state| {
                    state.fail(format!(
                    "power notification cleanup failed: deregister={deregister:#x} close={close:#x}"
                ))
                });
            }
            // context drops after deregistration/port destruction; source was borrowed.
        }
    }

    pub(super) fn open() -> Result<NativePower> {
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let (sender, receiver) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("m1fan-power".into())
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    let registration = match Registration::open(Arc::clone(&worker_shared)) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            return;
                        }
                    };
                    if sender.send(Ok(())).is_err() {
                        return;
                    }
                    if let Err(error) = registration.run() {
                        worker_shared.update(|state| state.fail(error.to_string()));
                    }
                }));
                if outcome.is_err() {
                    worker_shared
                        .update(|state| state.fail("power notification thread panicked".into()));
                }
            })
            .map_err(|error| {
                Error::Io(format!("create system power notification thread: {error}"))
            })?;
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(NativePower {
                shared,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                shared.stopping.store(true, Ordering::Release);
                // Detach a stalled registration; all native resources remain thread-owned.
                Err(Error::Io(format!(
                    "system power notification startup: {error}"
                )))
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::*;
    pub(super) fn open() -> Result<NativePower> {
        Err(Error::Invalid(
            "system power notifications require macOS".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_sleep_does_not_extend_deadline_or_generation() {
        let mut state = State::default();
        assert!(state.will_sleep(7, Duration::ZERO).is_none());
        assert!(state.will_sleep(7, Duration::from_secs(2)).is_none());
        assert!(state.gate);
        assert_eq!(state.generation, 1);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.events.len(), 1);
        assert!(state.due(Duration::from_millis(2999), false).is_empty());
        assert_eq!(
            state.due(Duration::from_secs(3), false),
            vec![Ack {
                token: 7,
                deadline: true
            }]
        );
        assert!(state.due(Duration::from_secs(4), false).is_empty());
    }

    #[test]
    fn explicit_ack_is_asynchronous_and_issued_once() {
        let mut state = State::default();
        state.will_sleep(8, Duration::ZERO);
        state.request_ack(8).unwrap();
        state.request_ack(8).unwrap();
        assert_eq!(state.stats.acknowledgement_attempts, 0);
        let due = state.due(Duration::from_secs(1), false);
        assert_eq!(
            due,
            vec![Ack {
                token: 8,
                deadline: false
            }]
        );
        state.ack_result(due[0], 0, true);
        state.request_ack(8).unwrap();
        assert!(state.due(Duration::from_secs(4), false).is_empty());
        assert_eq!(state.stats.acknowledgement_attempts, 1);
        assert!(state.gate); // ACK alone does not mean the system has woken.
        assert!(state.events.contains(&PowerEvent::SleepAcknowledged {
            token: 8,
            deadline: false,
            success: true
        }));
    }

    #[test]
    fn completed_wake_allows_same_sleep_token_in_next_cycle() {
        let mut state = State::default();
        state.will_sleep(8, Duration::ZERO);
        state.request_ack(8).unwrap();
        let first = state.due(Duration::from_secs(1), false);
        assert_eq!(first.len(), 1);
        state.ack_result(first[0], 0, true);
        state.transition(Kind::WillPowerOn);
        state.transition(Kind::DidWake);

        let generation = state.generation;
        state.will_sleep(8, Duration::from_secs(4));
        state.will_sleep(8, Duration::from_secs(5));
        assert_eq!(state.generation, generation + 1);
        assert_eq!(state.pending.len(), 1);
        assert!(state.gate);
        assert!(state.due(Duration::from_secs(6), false).is_empty());
        state.request_ack(8).unwrap();
        let second = state.due(Duration::from_secs(6), false);
        assert_eq!(second.len(), 1);
        assert!(!second[0].deadline);
        state.ack_result(second[0], 0, true);
        assert!(state.due(Duration::from_secs(8), false).is_empty());
        assert_eq!(state.stats.acknowledgement_attempts, 2);
        assert_eq!(
            state
                .events
                .iter()
                .filter(|event| matches!(event, PowerEvent::WillSleep { token: 8 }))
                .count(),
            2
        );
    }

    #[test]
    fn cancellation_retains_old_pending_ack_without_suppressing_reused_token() {
        let mut state = State::default();
        state.will_sleep(18, Duration::ZERO);
        state.transition(Kind::CancelledSleep);
        let cycle = state.cycle;
        state.transition(Kind::CancelledSleep);
        assert_eq!(state.cycle, cycle);
        state.will_sleep(18, Duration::from_secs(1));
        assert_eq!(state.pending.len(), 2);
        assert_ne!(state.pending[0].cycle, state.pending[1].cycle);

        // Only the old cycle was released by cancellation. Acknowledging it
        // must not consume or postpone the new cycle's independent deadline.
        let old = state.due(Duration::from_secs(1), false);
        assert_eq!(old.len(), 1);
        state.ack_result(old[0], 0, true);
        assert_eq!(state.pending.len(), 1);
        assert!(!state.pending[0].requested);
        assert!(state.due(Duration::from_secs(3), false).is_empty());
        state.request_ack(18).unwrap();
        let new = state.due(Duration::from_secs(3), false);
        assert_eq!(new.len(), 1);
        assert!(!new[0].deadline);
        state.ack_result(new[0], 0, true);
        assert_eq!(state.stats.acknowledgement_attempts, 2);
        assert!(state.due(Duration::from_secs(5), false).is_empty());
    }

    #[test]
    fn controller_request_covers_reused_token_while_both_cycles_are_pending() {
        let mut state = State::default();
        state.will_sleep(19, Duration::ZERO);
        state.transition(Kind::CancelledSleep);
        state.will_sleep(19, Duration::from_secs(1));
        state.request_ack(19).unwrap();
        let due = state.due(Duration::from_secs(1), false);
        assert_eq!(due.len(), 2);
        for ack in due {
            state.ack_result(ack, 0, true);
        }
        state.request_ack(19).unwrap();
        assert!(state.due(Duration::from_secs(4), false).is_empty());
        assert_eq!(state.stats.acknowledgement_attempts, 2);
    }

    #[test]
    fn cancelled_idle_cycle_allows_reuse_of_can_sleep_token() {
        let mut state = State::default();
        assert!(state.can_sleep(20));
        assert!(!state.can_sleep(20));
        state.transition(Kind::CancelledSleep);
        assert!(state.can_sleep(20));
        assert!(!state.can_sleep(20));
        // CanSystemSleep and WillSleep are distinct notifications even when
        // their token words happen to match within this cycle.
        state.will_sleep(20, Duration::ZERO);
        assert_eq!(state.pending.len(), 1);
        assert!(state.gate);
    }

    #[test]
    fn early_wake_keeps_gate_closed_and_completed_wake_is_idempotent() {
        let mut state = State::default();
        state.will_sleep(9, Duration::ZERO);
        state.transition(Kind::WillPowerOn);
        assert!(state.gate);
        let generation = state.generation;
        state.transition(Kind::WillPowerOn);
        assert_eq!(state.generation, generation);
        state.transition(Kind::DidWake);
        assert!(!state.gate);
        assert!(state.generation > generation);
        let generation = state.generation;
        state.transition(Kind::DidWake);
        assert_eq!(state.generation, generation);
        assert_eq!(state.due(Duration::from_secs(1), false).len(), 1);
    }

    #[test]
    fn cancelled_idle_sleep_opens_gate_without_manufacturing_sleep_ack() {
        let mut state = State::default();
        assert!(state.can_sleep(10));
        assert!(!state.can_sleep(10));
        state.transition(Kind::CancelledSleep);
        assert!(!state.gate);
        assert_eq!(state.generation, 1);
        state.transition(Kind::CancelledSleep);
        assert_eq!(state.generation, 1);
        assert!(state.due(Duration::from_secs(4), false).is_empty());
        assert_eq!(state.drain(), vec![PowerEvent::CancelledSleep]);
    }

    #[test]
    fn failed_ack_has_actual_result_and_persistent_error_count() {
        let mut state = State::default();
        state.will_sleep(11, Duration::ZERO);
        let ack = state.due(Duration::from_secs(3), false)[0];
        state.ack_result(ack, -1, true);
        let events = state.drain();
        assert!(events.contains(&PowerEvent::SleepAcknowledged {
            token: 11,
            deadline: true,
            success: false
        }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PowerEvent::Error(_)))
        );
        assert_eq!(state.stats.acknowledgement_failures, 1);
        assert!(state.gate);
        assert!(state.due(Duration::from_secs(4), false).is_empty());
    }

    #[test]
    fn event_overflow_is_bounded_reported_and_invalidates_writes() {
        let mut state = State::default();
        for index in 0..MAX_EVENTS + 2 {
            state.transition(if index % 2 == 0 {
                Kind::DidWake
            } else {
                Kind::CancelledSleep
            });
        }
        assert_eq!(state.events.len(), MAX_EVENTS);
        assert_eq!(state.stats.event_overflows, 2);
        assert!(state.gate);
        let events = state.drain();
        assert_eq!(events.len(), MAX_EVENTS + 1);
        assert!(matches!(&events[0], PowerEvent::Error(message) if message.contains("dropped=2")));
        assert!(state.drain().is_empty());
        assert_eq!(state.stats.event_overflows, 2);
    }

    #[test]
    fn pending_overflow_returns_one_immediate_ack_and_keeps_existing_deadlines() {
        let mut state = State::default();
        for token in 0..MAX_PENDING as isize {
            assert!(state.will_sleep(token, Duration::ZERO).is_none());
        }
        let generation = state.generation;
        let overflow = state.will_sleep(99, Duration::from_secs(1)).unwrap();
        assert_eq!(
            overflow,
            Ack {
                token: 99,
                deadline: false
            }
        );
        state.ack_result(overflow, -1, true);
        assert!(state.will_sleep(99, Duration::from_secs(2)).is_none());
        assert_eq!(state.pending.len(), MAX_PENDING);
        assert!(state.gate);
        assert!(state.generation > generation);
        assert_eq!(state.stats.pending_overflows, 1);
        assert_eq!(state.stats.acknowledgement_failures, 1);
        assert_eq!(state.due(Duration::from_secs(3), false).len(), MAX_PENDING);
    }

    #[test]
    fn shutdown_flushes_pending_and_unknown_tokens_cannot_ack_anything() {
        let mut state = State::default();
        state.will_sleep(12, Duration::ZERO);
        assert!(state.request_ack(999).is_err());
        assert_eq!(
            state.due(Duration::from_secs(1), true),
            vec![Ack {
                token: 12,
                deadline: false
            }]
        );
        assert!(state.due(Duration::from_secs(1), true).is_empty());
    }

    #[test]
    fn shared_gate_and_generation_follow_event_state() {
        let shared = Shared::default();
        shared.update(|state| state.will_sleep(13, Duration::ZERO));
        assert!(shared.gate.load(Ordering::Acquire));
        assert_eq!(shared.generation.load(Ordering::Acquire), 1);
        shared.update(|state| state.transition(Kind::DidWake));
        assert!(!shared.gate.load(Ordering::Acquire));
        assert!(shared.generation.load(Ordering::Acquire) > 1);
    }
}
