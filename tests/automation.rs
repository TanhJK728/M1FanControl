//! Reconnect, sleep and recovery scenarios use memory-only SMC/HID/power mocks.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use m1fan::automation::{self, Backoff, Factory, Observer, Status};
use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::power::{PowerEvent, PowerSource};
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::supervisor::Completion;

fn byte(value: u8) -> Value {
    Value {
        kind: *b"ui8 ",
        bytes: vec![value],
    }
}
fn float(value: f32) -> Value {
    Value {
        kind: *b"flt ",
        bytes: value.to_le_bytes().to_vec(),
    }
}
fn seconds(value: u64) -> Duration {
    Duration::from_secs(value)
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    SmcOpen,
    SourceOpen,
    Sample(bool),
    ModeRead(u8),
    ModeReadFailed,
    ModeWrite(u8),
    Journal(bool),
    TargetWrite,
    Ack(isize, u8),
    Power(PowerEvent),
}
#[derive(Clone, Debug)]
struct Record {
    at: Duration,
    connection: usize,
    source: usize,
    streak: usize,
    kind: Kind,
}
#[derive(Clone)]
enum Action {
    Sleep(isize),
    Wake,
    Cancel,
    Ftst(u8),
    Jump(Duration),
    Stop,
}

#[derive(Clone)]
struct SourceSpec {
    samples: VecDeque<bool>,
    fallback: bool,
    name: String,
    celsius: f32,
}
impl Default for SourceSpec {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            fallback: true,
            name: "pACC MTR Temp Sensor2".into(),
            celsius: 60.,
        }
    }
}
impl SourceSpec {
    fn script(samples: &[bool]) -> Self {
        Self {
            samples: samples.iter().copied().collect(),
            ..Self::default()
        }
    }
    fn failing() -> Self {
        Self {
            fallback: false,
            ..Self::default()
        }
    }
}

struct World {
    now: Duration,
    stop: bool,
    waits: usize,
    values: BTreeMap<String, Value>,
    records: Vec<Record>,
    smc_opens: usize,
    source_opens: usize,
    streak: usize,
    source_specs: VecDeque<SourceSpec>,
    fail_source_opens: usize,
    fail_restore_connections: BTreeSet<usize>,
    fail_target_once: bool,
    fail_mode_reads_while_asleep: bool,
    sleep_on_manual: Option<isize>,
    after_sleep: Vec<(Duration, Action)>,
    on_sample: BTreeMap<(usize, usize), Action>,
    scheduled: Vec<(Duration, Action)>,
    gate: bool,
    generation: u64,
    events: VecDeque<PowerEvent>,
    last_power: Option<PowerEvent>,
    acked: BTreeSet<isize>,
}
impl Default for World {
    fn default() -> Self {
        Self {
            now: Duration::ZERO,
            stop: false,
            waits: 0,
            values: [
                ("FNum", byte(1)),
                ("F0Md", byte(0)),
                ("F0Mn", float(1000.)),
                ("F0Mx", float(7000.)),
                ("F0Ac", float(2000.)),
                ("F0Tg", float(1900.)),
                ("TB1T", float(30.)),
                ("TB2T", float(29.)),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect(),
            records: vec![],
            smc_opens: 0,
            source_opens: 0,
            streak: 0,
            source_specs: VecDeque::new(),
            fail_source_opens: 0,
            fail_restore_connections: BTreeSet::new(),
            fail_target_once: false,
            fail_mode_reads_while_asleep: false,
            sleep_on_manual: None,
            after_sleep: vec![],
            on_sample: BTreeMap::new(),
            scheduled: vec![],
            gate: false,
            generation: 0,
            events: VecDeque::new(),
            last_power: None,
            acked: BTreeSet::new(),
        }
    }
}
impl World {
    fn record(&mut self, connection: usize, kind: Kind) {
        self.records.push(Record {
            at: self.now,
            connection,
            source: self.source_opens,
            streak: self.streak,
            kind,
        });
    }
    fn mode(&self) -> u8 {
        self.values["F0Md"].bytes[0]
    }
    fn schedule(&mut self, at: Duration, action: Action) {
        self.scheduled.push((at, action));
        self.scheduled.sort_by_key(|(at, _)| *at);
    }
    fn apply(&mut self, action: Action) {
        let event = match action {
            Action::Sleep(token) => PowerEvent::WillSleep { token },
            Action::Wake => PowerEvent::DidWake,
            Action::Cancel => PowerEvent::CancelledSleep,
            Action::Ftst(value) => {
                self.values.insert("Ftst".into(), byte(value));
                return;
            }
            Action::Jump(amount) => {
                self.now += amount;
                return;
            }
            Action::Stop => {
                self.stop = true;
                return;
            }
        };
        // Match the native PowerSource contract: duplicate notifications do not
        // increment generation or extend an existing sleep acknowledgement.
        if self.last_power.as_ref() == Some(&event) {
            return;
        }
        self.gate = matches!(event, PowerEvent::WillSleep { .. });
        self.generation += 1;
        self.last_power = Some(event.clone());
        self.record(0, Kind::Power(event.clone()));
        self.events.push_back(event);
    }
    fn advance(&mut self, duration: Duration) {
        self.now += duration;
        while self
            .scheduled
            .first()
            .is_some_and(|(at, _)| *at <= self.now)
        {
            let (_, action) = self.scheduled.remove(0);
            self.apply(action);
        }
        assert!(
            self.now < seconds(600),
            "mock did not terminate: {:?}",
            self.records.last()
        );
    }
    fn manual_writes(&self) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|r| r.kind == Kind::ModeWrite(1))
            .collect()
    }
    fn all_writes(&self) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|r| matches!(r.kind, Kind::ModeWrite(_) | Kind::TargetWrite))
            .collect()
    }
}
type Shared = Rc<RefCell<World>>;

struct MockSmc {
    world: Shared,
    connection: usize,
}
impl ReadSmc for MockSmc {
    fn read(&mut self, key: &str) -> Result<Value> {
        let mut world = self.world.borrow_mut();
        if key == "F0Md" && world.gate && world.fail_mode_reads_while_asleep {
            world.record(self.connection, Kind::ModeReadFailed);
            return Err(Error::Io(
                "mode verification unavailable during sleep".into(),
            ));
        }
        let value = world
            .values
            .get(key)
            .cloned()
            .ok_or_else(|| Error::Missing(key.into()))?;
        if key == "F0Md" {
            world.record(self.connection, Kind::ModeRead(value.bytes[0]));
        }
        Ok(value)
    }
    fn keys(&mut self) -> Result<Vec<String>> {
        Ok(self.world.borrow().values.keys().cloned().collect())
    }
}
impl WriteSmc for MockSmc {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        let mut world = self.world.borrow_mut();
        if key == "F0Md" {
            world.record(self.connection, Kind::ModeWrite(value.bytes[0]));
            if value == &byte(0) && world.fail_restore_connections.contains(&self.connection) {
                return Err(Error::Io("mock old connection rejects restoration".into()));
            }
        } else if key == "F0Tg" {
            world.record(self.connection, Kind::TargetWrite);
            if std::mem::take(&mut world.fail_target_once) {
                return Err(Error::Control(
                    "mock target write failed after manual mode".into(),
                ));
            }
        } else {
            panic!("unexpected mock write key {key}");
        }
        world.values.insert(key.into(), value.clone());
        if key == "F0Tg" {
            world.values.insert("F0Ac".into(), value.clone());
        }
        if key == "F0Md"
            && value == &byte(1)
            && let Some(token) = world.sleep_on_manual.take()
        {
            world.apply(Action::Sleep(token));
            for (delay, action) in std::mem::take(&mut world.after_sleep) {
                let at = world.now + delay;
                world.schedule(at, action);
            }
        }
        Ok(())
    }
}
struct MockSource {
    world: Shared,
    id: usize,
    calls: usize,
    spec: SourceSpec,
}
impl ChipTemperatureSource for MockSource {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        self.calls += 1;
        let good = self.spec.samples.pop_front().unwrap_or(self.spec.fallback);
        let mut world = self.world.borrow_mut();
        world.streak = if good { world.streak + 1 } else { 0 };
        world.record(0, Kind::Sample(good));
        if let Some(action) = world.on_sample.remove(&(self.id, self.calls)) {
            world.apply(action);
        }
        drop(world);
        if !good {
            return Err(Error::Io("mock fresh sensor sample unavailable".into()));
        }
        Ok(vec![ChipReading {
            name: self.spec.name.clone(),
            group: SensorGroup::Performance,
            celsius: self.spec.celsius,
        }])
    }
}
struct MockFactory(Shared);
impl Factory for MockFactory {
    type Smc = MockSmc;
    type Source = MockSource;
    fn smc(&mut self) -> Result<MockSmc> {
        let mut world = self.0.borrow_mut();
        world.smc_opens += 1;
        let connection = world.smc_opens;
        world.record(connection, Kind::SmcOpen);
        Ok(MockSmc {
            world: self.0.clone(),
            connection,
        })
    }
    fn source(&mut self) -> Result<MockSource> {
        let mut world = self.0.borrow_mut();
        world.source_opens += 1;
        world.streak = 0;
        let id = world.source_opens;
        world.record(0, Kind::SourceOpen);
        if world.fail_source_opens > 0 {
            world.fail_source_opens -= 1;
            return Err(Error::Io("mock source open failed".into()));
        }
        let spec = world.source_specs.pop_front().unwrap_or_default();
        Ok(MockSource {
            world: self.0.clone(),
            id,
            calls: 0,
            spec,
        })
    }
}
struct FakeClock(Shared);
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.borrow().now
    }
    fn wait(&self, duration: Duration) -> bool {
        let mut world = self.0.borrow_mut();
        world.waits += 1;
        assert!(world.waits < 200_000, "mock loop did not progress");
        world.advance(duration);
        !world.stop
    }
    fn settle(&self, duration: Duration) {
        self.0.borrow_mut().advance(duration);
    }
}
struct FakePower(Shared);
impl PowerSource for FakePower {
    fn drain(&self) -> Vec<PowerEvent> {
        self.0.borrow_mut().events.drain(..).collect()
    }
    fn sleep_pending(&self) -> bool {
        self.0.borrow().gate
    }
    fn generation(&self) -> u64 {
        self.0.borrow().generation
    }
    fn acknowledge_sleep(&self, token: isize) -> Result<()> {
        let mut world = self.0.borrow_mut();
        assert!(world.acked.insert(token), "sleep token acknowledged twice");
        let mode = world.mode();
        world.record(0, Kind::Ack(token, mode));
        world.events.push_back(PowerEvent::SleepAcknowledged {
            token,
            deadline: false,
            success: true,
        });
        Ok(())
    }
}
struct Output {
    world: Shared,
    statuses: Vec<Status>,
    events: Vec<String>,
    manual_frames: usize,
    stop_after_manual: usize,
    stop_after_ack: bool,
    stop_after_backoffs: Option<usize>,
    ready_calls: usize,
    fail_journal_arming: bool,
    sleep_and_stop_on_automatic: bool,
}
impl Observer for Output {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(self.world.borrow().stop
            || self.manual_frames >= self.stop_after_manual
            || (self.stop_after_ack && !self.world.borrow().acked.is_empty())
            || self.stop_after_backoffs.is_some_and(|limit| {
                self.events
                    .iter()
                    .filter(|s| s.contains("retry in"))
                    .count()
                    >= limit
            }))
    }
    fn ready(&mut self, _: &Status) -> Result<()> {
        self.ready_calls += 1;
        Ok(())
    }
    fn update(&mut self, status: &Status) -> Result<()> {
        if status.phase == "manual" {
            self.manual_frames += 1;
        }
        self.statuses.push(status.clone());
        if status.phase == "monitoring-automatic" && self.sleep_and_stop_on_automatic {
            self.sleep_and_stop_on_automatic = false;
            let mut world = self.world.borrow_mut();
            world.apply(Action::Sleep(75));
            world.stop = true;
        }
        Ok(())
    }
    fn event(&mut self, message: &str) -> Result<()> {
        self.events.push(message.into());
        Ok(())
    }
    fn recovery_pending(&mut self, pending: bool) -> Result<()> {
        self.world.borrow_mut().record(0, Kind::Journal(pending));
        if pending && self.fail_journal_arming {
            return Err(Error::Io("mock durable recovery journal failed".into()));
        }
        Ok(())
    }
}
struct Rig {
    world: Shared,
    output: Output,
}
impl Rig {
    fn new() -> Self {
        let world = Rc::new(RefCell::new(World::default()));
        Self {
            output: Output {
                world: world.clone(),
                statuses: vec![],
                events: vec![],
                manual_frames: 0,
                stop_after_manual: 1,
                stop_after_ack: false,
                stop_after_backoffs: None,
                ready_calls: 0,
                fail_journal_arming: false,
                sleep_and_stop_on_automatic: false,
            },
            world,
        }
    }
    fn run(&mut self, was_unclean: bool) -> Result<Completion> {
        self.run_with_delay(Duration::ZERO, was_unclean)
    }
    fn run_with_delay(&mut self, delay: Duration, was_unclean: bool) -> Result<Completion> {
        automation::run(
            &mut MockFactory(self.world.clone()),
            &FakePower(self.world.clone()),
            &FakeClock(self.world.clone()),
            &mut self.output,
            delay,
            was_unclean,
        )
    }
    fn success(&mut self) {
        assert_eq!(self.run(false).unwrap(), Completion::AutomaticVerified);
    }
}

#[test]
fn three_consecutive_healthy_samples_precede_first_manual_write() {
    let mut rig = Rig::new();
    rig.success();
    let world = rig.world.borrow();
    let manual = world.manual_writes();
    assert_eq!(manual.len(), 1);
    assert!(manual[0].streak >= 3);
    assert!(manual[0].at >= seconds(2));
    assert_eq!(world.mode(), 0);
    assert_eq!(rig.output.ready_calls, 1);
}

#[test]
fn qualification_failure_restarts_consecutive_health_count_without_reopening() {
    let mut rig = Rig::new();
    rig.world
        .borrow_mut()
        .source_specs
        .push_back(SourceSpec::script(&[true, true, false, true, true, true]));
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 1);
    assert!(world.manual_writes()[0].at >= seconds(5));
    assert!(world.manual_writes()[0].streak >= 3);
    assert!(rig.output.statuses.iter().any(|s| s.errors > 0));
}

#[test]
fn sleep_between_mode_and_target_restores_and_verifies_before_ack() {
    let mut rig = Rig::new();
    rig.world.borrow_mut().sleep_on_manual = Some(71);
    rig.output.stop_after_ack = true;
    rig.success();
    let world = rig.world.borrow();
    assert!(!world.records.iter().any(|r| r.kind == Kind::TargetWrite));
    let restored = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let ack = world
        .records
        .iter()
        .position(|r| r.kind == Kind::Ack(71, 0))
        .unwrap();
    assert!(restored < ack);
    assert!(
        world.records[restored + 1..ack]
            .iter()
            .filter(|r| r.kind == Kind::ModeRead(0))
            .count()
            >= 3
    );
    assert_eq!(world.manual_writes().len(), 1);
}

#[test]
fn wake_reopens_both_connections_and_qualifies_again_before_takeover() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.sleep_on_manual = Some(72);
        world.after_sleep.push((seconds(2), Action::Wake));
    }
    rig.success();
    let world = rig.world.borrow();
    let manual = world.manual_writes();
    assert_eq!(manual.len(), 2);
    assert_eq!(world.source_opens, 2);
    assert_ne!(manual[0].connection, manual[1].connection);
    assert_eq!(manual[1].source, 2);
    assert!(manual[1].streak >= 3);
    let wake = world
        .records
        .iter()
        .find(|r| r.kind == Kind::Power(PowerEvent::DidWake))
        .unwrap();
    assert!(manual[1].at >= wake.at + seconds(2));
    assert_eq!(world.mode(), 0);
}

#[test]
fn duplicate_sleep_and_cancel_notifications_do_not_duplicate_ack_or_retakeover() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.sleep_on_manual = Some(73);
        world.after_sleep = vec![
            (Duration::from_millis(100), Action::Sleep(73)),
            (seconds(2), Action::Cancel),
            (seconds(2), Action::Cancel),
        ];
    }
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.acked.len(), 1);
    assert_eq!(world.source_opens, 2);
    assert_eq!(world.manual_writes().len(), 2);
    assert_eq!(world.generation, 2);
    assert!(
        rig.output
            .statuses
            .iter()
            .any(|s| s.last_power.contains("sleep-cancelled"))
    );
}

#[test]
fn power_change_during_qualification_discards_previous_healthy_samples() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.on_sample.insert((1, 2), Action::Sleep(74));
        world.schedule(seconds(2), Action::Wake);
    }
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 2);
    assert_eq!(world.manual_writes().len(), 1);
    assert_eq!(world.manual_writes()[0].source, 2);
    assert!(world.manual_writes()[0].streak >= 3);
}

#[test]
fn sampling_gap_restores_then_rebuilds_and_requalifies_automatically() {
    let mut rig = Rig::new();
    rig.output.stop_after_manual = 2;
    rig.world
        .borrow_mut()
        .on_sample
        .insert((1, 5), Action::Jump(seconds(6)));
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 2);
    let manual = world.manual_writes();
    assert_eq!(manual.len(), 2);
    assert_ne!(manual[0].connection, manual[1].connection);
    assert!(manual[1].streak >= 3);
    let first_restore = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let second_open = world
        .records
        .iter()
        .position(|r| r.kind == Kind::SourceOpen && r.source == 2)
        .unwrap();
    assert!(first_restore < second_open);
    assert!(rig.output.events.iter().any(|e| e.contains("gap")));
}

#[test]
fn failed_qualification_has_ten_second_window_then_five_second_backoff() {
    let mut rig = Rig::new();
    rig.world
        .borrow_mut()
        .source_specs
        .push_back(SourceSpec::failing());
    rig.success();
    let world = rig.world.borrow();
    let opens: Vec<_> = world
        .records
        .iter()
        .filter(|r| r.kind == Kind::SourceOpen)
        .collect();
    assert_eq!(opens.len(), 2);
    assert!(opens[1].at - opens[0].at >= seconds(15));
    assert!(opens[1].at - opens[0].at < seconds(16));
    let first_samples = world
        .records
        .iter()
        .filter(|r| matches!(r.kind, Kind::Sample(_)) && r.source == 1)
        .count();
    assert_eq!(
        first_samples, 11,
        "t=0 through t=10 inclusive, bounded qualification"
    );
    assert!(world.manual_writes().iter().all(|r| r.source == 2));
}

#[test]
fn slow_third_healthy_sample_cannot_extend_ten_second_qualification_deadline() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world
            .source_specs
            .push_back(SourceSpec::script(&[false; 8]));
        // Good samples start at t=8, 9 and 10, but the last one returns at t=12.
        // This stays under the separate five-second I/O-gap limit.
        world.on_sample.insert((1, 11), Action::Jump(seconds(2)));
    }
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(
        world.source_opens, 2,
        "qualification must expire before takeover"
    );
    assert!(world.manual_writes().iter().all(|r| r.source == 2));
}

#[test]
fn three_running_sensor_failures_restore_then_reconnect_without_manual_restart() {
    let mut rig = Rig::new();
    rig.output.stop_after_manual = 2;
    rig.world
        .borrow_mut()
        .source_specs
        .push_back(SourceSpec::script(&[
            true, true, true, true, false, false, false,
        ]));
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 2);
    assert_eq!(world.manual_writes().len(), 2);
    let restore = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let reopened = world
        .records
        .iter()
        .position(|r| r.kind == Kind::SourceOpen && r.source == 2)
        .unwrap();
    assert!(restore < reopened);
    assert_eq!(
        world.records[..restore]
            .iter()
            .filter(|r| r.kind == Kind::Sample(false))
            .count(),
        3
    );
    assert!(world.manual_writes()[1].streak >= 3);
    assert_eq!(world.mode(), 0);
}

#[test]
fn source_open_failures_follow_five_fifteen_thirty_sixty_second_backoff() {
    let mut rig = Rig::new();
    rig.world.borrow_mut().fail_source_opens = 4;
    rig.success();
    let world = rig.world.borrow();
    let opens: Vec<_> = world
        .records
        .iter()
        .filter(|r| r.kind == Kind::SourceOpen)
        .collect();
    assert_eq!(opens.len(), 5);
    for (pair, delay) in opens.windows(2).zip([5, 15, 30, 60]) {
        let elapsed = pair[1].at - pair[0].at;
        assert!(
            elapsed >= seconds(delay) && elapsed < seconds(delay + 1),
            "delay {delay}, elapsed {elapsed:?}"
        );
    }
    assert_eq!(world.manual_writes().len(), 1);
    assert_eq!(world.manual_writes()[0].source, 5);
}

#[test]
fn backoff_is_capped_and_can_be_reset_after_healthy_operation() {
    let mut backoff = Backoff::default();
    let delays: Vec<_> = (0..6).map(|_| backoff.next_delay()).collect();
    assert_eq!(delays, [5, 15, 30, 60, 60, 60].map(seconds));
    backoff.reset();
    assert_eq!(backoff.next_delay(), seconds(5));
}

#[test]
fn failed_restore_responsibility_crosses_reconnect_and_precedes_next_manual_write() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.fail_target_once = true;
        world.fail_restore_connections.insert(1);
    }
    rig.success();
    let world = rig.world.borrow();
    let second_restore = world
        .records
        .iter()
        .position(|r| r.connection == 2 && r.kind == Kind::ModeWrite(0))
        .unwrap();
    let second_manual = world
        .records
        .iter()
        .position(|r| r.connection == 2 && r.kind == Kind::ModeWrite(1))
        .unwrap();
    assert!(second_restore < second_manual);
    assert!(
        rig.output
            .statuses
            .iter()
            .any(|s| s.phase == "recovery-backoff" && s.recovery_pending)
    );
    assert!(
        rig.output
            .statuses
            .iter()
            .any(|s| s.phase == "qualifying-fresh-sensors"
                && !s.recovery_pending
                && s.retry_level == 1)
    );
    assert_eq!(world.mode(), 0);
}

#[test]
fn startup_foreign_manual_control_is_never_overwritten() {
    let mut rig = Rig::new();
    rig.world.borrow_mut().values.insert("F0Md".into(), byte(1));
    rig.output.stop_after_backoffs = Some(1);
    let error = rig.run(false).unwrap_err();
    assert!(error.to_string().contains("foreign manual"));
    let world = rig.world.borrow();
    assert!(world.all_writes().is_empty());
    assert_eq!(world.source_opens, 0);
    assert_eq!(world.mode(), 1);
}

#[test]
fn unclean_restart_restores_existing_manual_mode_before_fresh_qualification() {
    let mut rig = Rig::new();
    rig.world.borrow_mut().values.insert("F0Md".into(), byte(1));
    assert_eq!(rig.run(true).unwrap(), Completion::AutomaticVerified);
    let world = rig.world.borrow();
    assert_eq!(world.all_writes()[0].kind, Kind::ModeWrite(0));
    let restored = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let source = world
        .records
        .iter()
        .position(|r| r.kind == Kind::SourceOpen)
        .unwrap();
    assert!(restored < source);
    assert!(world.manual_writes()[0].streak >= 3);
    assert_eq!(world.mode(), 0);
}

#[test]
fn stop_during_backoff_is_prompt_and_does_not_open_another_temperature_source() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.fail_source_opens = 4;
        world.schedule(seconds(2), Action::Stop);
    }
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 1);
    assert!(world.all_writes().is_empty());
    assert!(world.now < seconds(3));
    assert_eq!(world.mode(), 0);
}

#[test]
fn stop_during_process_restart_delay_performs_only_final_automatic_verification() {
    let mut rig = Rig::new();
    rig.world.borrow_mut().schedule(seconds(1), Action::Stop);
    assert_eq!(
        rig.run_with_delay(seconds(60), false).unwrap(),
        Completion::AutomaticVerified
    );
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 0);
    assert!(world.all_writes().is_empty());
    assert_eq!(world.smc_opens, 1);
    assert!(world.now < seconds(2));
}

#[test]
fn durable_responsibility_is_set_before_writes_and_cleared_after_verified_restore() {
    let mut rig = Rig::new();
    rig.success();
    let world = rig.world.borrow();
    let armed = world
        .records
        .iter()
        .position(|r| r.kind == Kind::Journal(true))
        .unwrap();
    let manual = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(1))
        .unwrap();
    let restore = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let cleared = world
        .records
        .iter()
        .enumerate()
        .find(|(i, r)| *i > armed && r.kind == Kind::Journal(false))
        .map(|(i, _)| i)
        .unwrap();
    assert!(armed < manual && manual < restore && restore < cleared);
    assert!(
        world.records[restore + 1..cleared]
            .iter()
            .filter(|r| r.kind == Kind::ModeRead(0))
            .count()
            >= 3
    );
    assert!(
        !world.records[armed + 1..restore]
            .iter()
            .any(|r| r.kind == Kind::Journal(false))
    );
}

#[test]
fn failed_durable_arming_callback_prevents_hardware_write() {
    let mut rig = Rig::new();
    rig.output.fail_journal_arming = true;
    let error = rig.run(false).unwrap_err();
    let world = rig.world.borrow();
    assert!(error.to_string().contains("output failed"));
    assert!(world.records.iter().any(|r| r.kind == Kind::Journal(true)));
    assert!(world.all_writes().is_empty());
    assert_eq!(world.mode(), 0);
    assert!(
        rig.output
            .events
            .iter()
            .any(|e| e.contains("automatic mode verified"))
    );
}

#[test]
fn changed_sensor_names_across_reconnect_prevent_new_takeover() {
    let mut rig = Rig::new();
    rig.output.stop_after_manual = usize::MAX;
    rig.output.stop_after_backoffs = Some(2);
    {
        let mut world = rig.world.borrow_mut();
        world.source_specs.push_back(SourceSpec::default());
        world.source_specs.push_back(SourceSpec {
            name: "pACC MTR Temp Sensor3".into(),
            ..SourceSpec::default()
        });
        world.on_sample.insert((1, 5), Action::Jump(seconds(6)));
    }
    rig.success();
    let world = rig.world.borrow();
    assert_eq!(world.source_opens, 2);
    assert_eq!(world.manual_writes().len(), 1);
    assert_eq!(world.manual_writes()[0].source, 1);
    assert!(rig.output.statuses.iter().any(|s| {
        s.last_error
            .contains("chip sensor set differs across reconnect")
    }));
    assert_eq!(world.mode(), 0);
}

#[test]
fn sleep_and_stop_with_failed_final_mode_verification_never_claims_success() {
    let mut rig = Rig::new();
    rig.output.sleep_and_stop_on_automatic = true;
    {
        let mut world = rig.world.borrow_mut();
        world.source_specs.push_back(SourceSpec {
            celsius: 45.,
            ..SourceSpec::default()
        });
        world.fail_mode_reads_while_asleep = true;
    }
    assert!(rig.run(false).is_err());
    let world = rig.world.borrow();
    assert!(
        world.all_writes().is_empty(),
        "automatic monitoring never acquired write responsibility"
    );
    assert!(world.records.iter().any(|r| r.kind == Kind::ModeReadFailed));
    assert!(world.records.iter().any(|r| r.kind == Kind::Ack(75, 0)));
    assert!(
        world.smc_opens >= 2,
        "final verification cannot use stale successful preflight"
    );
    assert!(
        rig.output
            .events
            .iter()
            .any(|e| e.contains("automatic restoration NOT verified"))
    );
    assert!(
        !rig.output
            .events
            .iter()
            .any(|e| e == "automatic mode verified; automatic supervisor stopped")
    );
}

#[test]
fn initial_sleep_ack_does_not_claim_automatic_verification_before_any_smc_read() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.apply(Action::Sleep(76));
        world.schedule(Duration::from_millis(500), Action::Stop);
    }
    rig.success();
    let sleep_statuses: Vec<_> = rig
        .output
        .statuses
        .iter()
        .filter(|s| s.phase == "sleeping")
        .collect();
    assert!(!sleep_statuses.is_empty());
    assert!(sleep_statuses.iter().all(|s| !s.automatic_verified));
    assert!(
        sleep_statuses
            .iter()
            .all(|s| s.last_release == "sleep: automatic_verified_at_handling=false")
    );
    let world = rig.world.borrow();
    let ack = world
        .records
        .iter()
        .position(|r| r.kind == Kind::Ack(76, 0))
        .unwrap();
    let first_smc = world
        .records
        .iter()
        .position(|r| r.kind == Kind::SmcOpen)
        .unwrap();
    assert!(ack < first_smc);
    assert!(world.all_writes().is_empty());
    assert_eq!(world.source_opens, 0);
}

#[test]
fn sleep_interrupting_restart_delay_recovers_durable_responsibility_before_ack() {
    let mut rig = Rig::new();
    rig.output.stop_after_ack = true;
    {
        let mut world = rig.world.borrow_mut();
        world.values.insert("F0Md".into(), byte(1));
        world.schedule(seconds(1), Action::Sleep(77));
    }
    assert_eq!(
        rig.run_with_delay(seconds(60), true).unwrap(),
        Completion::AutomaticVerified
    );
    let world = rig.world.borrow();
    let restored = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let ack = world
        .records
        .iter()
        .position(|r| matches!(r.kind, Kind::Ack(77, _)))
        .unwrap();
    assert!(
        restored < ack,
        "durable responsibility requires a recovery attempt before active ACK"
    );
    assert_eq!(world.records[ack].kind, Kind::Ack(77, 0));
    assert!(
        world.records[restored + 1..ack]
            .iter()
            .filter(|r| r.kind == Kind::ModeRead(0))
            .count()
            >= 3
    );
    assert!(
        world.records[restored + 1..ack]
            .iter()
            .any(|r| r.kind == Kind::Journal(false))
    );
    assert_eq!(
        world.source_opens, 0,
        "sleep recovery does not initialize temperature control"
    );
    assert!(world.manual_writes().is_empty());
}

#[test]
fn failed_sleep_recovery_during_restart_delay_retains_durable_pending_and_unverified_status() {
    let mut rig = Rig::new();
    {
        let mut world = rig.world.borrow_mut();
        world.values.insert("F0Md".into(), byte(1));
        world.fail_restore_connections.extend(1..=8);
        world.schedule(seconds(1), Action::Sleep(78));
        // Leave enough time for both bounded recovery rounds and publication
        // of the sleeping status before requesting termination.
        world.schedule(seconds(10), Action::Stop);
    }
    assert!(rig.run_with_delay(seconds(60), true).is_err());
    let world = rig.world.borrow();
    let first_restore = world
        .records
        .iter()
        .position(|r| r.kind == Kind::ModeWrite(0))
        .unwrap();
    let ack = world
        .records
        .iter()
        .position(|r| r.kind == Kind::Ack(78, 1))
        .unwrap();
    assert!(
        first_restore < ack,
        "bounded recovery attempts precede ACK even when they fail"
    );
    assert!(!world.records.iter().any(|r| r.kind == Kind::Journal(false)));
    assert!(rig.output.statuses.iter().any(|s| s.phase == "sleeping"
        && s.recovery_pending
        && !s.automatic_verified
        && s.last_release.contains("false")));
    assert!(
        !rig.output
            .events
            .iter()
            .any(|e| e == "automatic mode verified; automatic supervisor stopped")
    );
    assert_eq!(world.mode(), 1);
    assert_eq!(world.source_opens, 0);
}

#[test]
fn first_control_sample_waits_a_full_sensor_period_after_qualification() {
    let mut rig = Rig::new();
    rig.success();
    let world = rig.world.borrow();
    let samples: Vec<_> = world
        .records
        .iter()
        .filter(|r| r.source == 1 && r.kind == Kind::Sample(true))
        .collect();
    assert!(samples.len() >= 4);
    assert!(
        samples[3].at - samples[2].at >= seconds(1),
        "a fresh source must not immediately reread its qualification event"
    );
    assert!(world.manual_writes()[0].at >= samples[3].at);
}

#[test]
fn ftst_becoming_active_during_qualification_blocks_takeover_and_verified_claims() {
    let mut rig = Rig::new();
    rig.output.stop_after_backoffs = Some(1);
    {
        let mut world = rig.world.borrow_mut();
        world.values.insert("Ftst".into(), byte(0));
        world.on_sample.insert((1, 2), Action::Ftst(1));
    }
    assert!(rig.run(false).is_err());
    let world = rig.world.borrow();
    assert_eq!(
        world.mode(),
        0,
        "fan mode alone must not conceal active Ftst"
    );
    assert_eq!(world.values["Ftst"], byte(1));
    assert!(world.all_writes().is_empty());
    assert!(!world.records.iter().any(|r| r.kind == Kind::Journal(true)));
    let rejected: Vec<_> = rig
        .output
        .statuses
        .iter()
        .filter(|s| s.last_error.contains("Ftst"))
        .collect();
    assert!(!rejected.is_empty());
    assert!(rejected.iter().all(|s| !s.automatic_verified));
    assert_eq!(rig.output.ready_calls, 0);
    assert!(
        rig.output
            .events
            .iter()
            .any(|e| e.contains("automatic restoration NOT verified"))
    );
    assert!(
        !rig.output
            .events
            .iter()
            .any(|e| e == "automatic mode verified; automatic supervisor stopped")
    );
}
