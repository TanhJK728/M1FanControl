//! End-to-end orchestration uses only fake SMC/temperature/clock capabilities.
use m1fan::adaptive::{self, Completion, Frame, Observer};
use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

fn byte(n: u8) -> Value {
    Value {
        kind: *b"ui8 ",
        bytes: vec![n],
    }
}
fn float(n: f32) -> Value {
    Value {
        kind: *b"flt ",
        bytes: n.to_le_bytes().to_vec(),
    }
}
struct ReadRule {
    activate_target: f32,
    key: String,
    skip: usize,
    remaining: usize,
    result: Result<Value>,
    active: bool,
}
struct Smc {
    data: BTreeMap<String, Value>,
    writes: Vec<(String, Value)>,
    rules: Vec<ReadRule>,
}
impl Smc {
    fn new() -> Self {
        Self {
            data: [
                ("FNum", byte(1)),
                ("F0Md", byte(0)),
                ("F0Mn", float(1000.)),
                ("F0Mx", float(7000.)),
                ("F0Ac", float(2000.)),
                ("F0Tg", float(1900.)),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect(),
            writes: Vec::new(),
            rules: Vec::new(),
        }
    }
    fn after_target(&mut self, target: f32, key: &str, skip: usize, result: Result<Value>) {
        self.rules.push(ReadRule {
            activate_target: target,
            key: key.into(),
            skip,
            remaining: 1,
            result,
            active: false,
        });
    }
}
impl ReadSmc for Smc {
    fn read(&mut self, key: &str) -> Result<Value> {
        for rule in &mut self.rules {
            if !rule.active || rule.remaining == 0 || rule.key != key {
                continue;
            }
            if rule.skip > 0 {
                rule.skip -= 1;
                continue;
            }
            rule.remaining -= 1;
            if let Ok(value) = &rule.result {
                self.data.insert(key.into(), value.clone());
            }
            return rule.result.clone();
        }
        self.data
            .get(key)
            .cloned()
            .ok_or_else(|| Error::Missing(key.into()))
    }
    fn keys(&mut self) -> Result<Vec<String>> {
        Ok(self.data.keys().cloned().collect())
    }
}
impl WriteSmc for Smc {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        self.writes.push((key.into(), value.clone()));
        self.data.insert(key.into(), value.clone());
        if key == "F0Tg" {
            self.data.insert("F0Ac".into(), value.clone());
            for rule in &mut self.rules {
                if value.number()? == rule.activate_target {
                    rule.active = true;
                }
            }
        }
        Ok(())
    }
}
#[derive(Default)]
struct TestClock {
    now: Cell<Duration>,
    stopped: Cell<bool>,
}
impl Clock for TestClock {
    fn wait(&self, d: Duration) -> bool {
        self.now.set(self.now.get() + d);
        !self.stopped.get()
    }
    fn settle(&self, d: Duration) {
        self.now.set(self.now.get() + d);
    }
    fn now(&self) -> Duration {
        self.now.get()
    }
}
struct Source<'a> {
    samples: VecDeque<Result<f32>>,
    calls: usize,
    pause_at: Option<usize>,
    clock: &'a TestClock,
}
impl<'a> Source<'a> {
    fn new(clock: &'a TestClock, samples: Vec<Result<f32>>) -> Self {
        Self {
            samples: samples.into(),
            calls: 0,
            pause_at: None,
            clock,
        }
    }
}
impl ChipTemperatureSource for Source<'_> {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        self.calls += 1;
        if self.pause_at == Some(self.calls) {
            self.clock
                .now
                .set(self.clock.now.get() + Duration::from_secs(10));
        }
        let temp = self.samples.pop_front().unwrap_or(Ok(50.))?;
        Ok(vec![ChipReading {
            name: "pACC MTR Temp Sensor2".into(),
            group: SensorGroup::Performance,
            celsius: temp,
        }])
    }
}
struct Output<'a> {
    frames: usize,
    limit: usize,
    ready_failure: bool,
    signal_ready: bool,
    clock: &'a TestClock,
    events: Vec<String>,
}
impl<'a> Output<'a> {
    fn new(clock: &'a TestClock) -> Self {
        Self {
            frames: 0,
            limit: 4,
            ready_failure: false,
            signal_ready: false,
            clock,
            events: Vec::new(),
        }
    }
}
impl Observer for Output<'_> {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(self.frames >= self.limit)
    }
    fn ready(&mut self, f: &Frame) -> Result<()> {
        if self.signal_ready {
            self.clock.stopped.set(true);
        }
        if self.ready_failure {
            return Err(Error::Io("startup parent disconnected".into()));
        }
        self.frame(f)
    }
    fn frame(&mut self, _: &Frame) -> Result<()> {
        self.frames += 1;
        Ok(())
    }
    fn event(&mut self, s: &str) -> Result<()> {
        self.events.push(s.into());
        Ok(())
    }
}
fn restored(smc: &Smc) {
    assert_eq!(smc.data["F0Md"], byte(0));
    assert_eq!(smc.writes.last().unwrap(), &("F0Md".into(), byte(0)));
}

#[test]
fn preview_has_no_write_capability_and_never_changes_mode() {
    struct OnlyRead(Smc);
    impl ReadSmc for OnlyRead {
        fn read(&mut self, k: &str) -> Result<Value> {
            self.0.read(k)
        }
        fn keys(&mut self) -> Result<Vec<String>> {
            self.0.keys()
        }
    }
    let mut smc = OnlyRead(Smc::new());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.), Ok(70.)]);
    let mut out = Output::new(&clock);
    adaptive::preview(&mut smc, &plan, &mut source, &clock, Some(2), &mut out).unwrap();
    assert_eq!(out.frames, 2);
    assert!(smc.0.writes.is_empty());
    assert_eq!(smc.0.data["F0Md"], byte(0));
}
#[test]
fn dynamic_temperature_control_stops_and_restores() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.), Ok(70.)]);
    let mut out = Output::new(&clock);
    adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap();
    restored(&smc);
    let targets: Vec<_> = smc
        .writes
        .iter()
        .filter(|(k, _)| k == "F0Tg")
        .map(|(_, v)| v.number().unwrap())
        .collect();
    assert_eq!(targets, vec![4000., 7000.]);
    assert!(
        out.events
            .iter()
            .any(|s| s == "automatic restoration verified")
    );
}
#[test]
fn three_sensor_failures_restore_without_reasserting_target() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let error = Err(Error::Missing("HID event".into()));
    let mut source = Source::new(&clock, vec![Ok(50.), error.clone(), error.clone(), error]);
    let mut out = Output::new(&clock);
    assert!(adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    restored(&smc);
    assert_eq!(source.calls, 4);
    assert_eq!(smc.writes.iter().filter(|(k, _)| k == "F0Tg").count(), 1);
}
#[test]
fn invalid_initial_temperature_does_not_write() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(f32::NAN)]);
    let mut out = Output::new(&clock);
    assert!(adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    assert!(smc.writes.is_empty());
}
#[test]
fn startup_handshake_failure_restores() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![]);
    let mut out = Output::new(&clock);
    out.ready_failure = true;
    assert!(adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    restored(&smc);
}
#[test]
fn signal_after_engagement_restores_even_when_clock_interrupts() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![]);
    let mut out = Output::new(&clock);
    out.signal_ready = true;
    adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap();
    restored(&smc);
}
#[test]
fn sleep_or_stalled_sampling_exits_and_restores() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![]);
    source.pause_at = Some(2);
    let mut out = Output::new(&clock);
    let err = adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap_err();
    assert!(err.to_string().contains("sampling gap"));
    restored(&smc);
    assert_eq!(smc.writes.iter().filter(|(k, _)| k == "F0Tg").count(), 1);
}
#[test]
fn delayed_initial_temperature_has_no_writes() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![]);
    source.pause_at = Some(1);
    let mut out = Output::new(&clock);
    assert!(adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    assert!(smc.writes.is_empty());
}

#[test]
fn ownership_lost_after_hold_read_is_detected_before_a_new_target_or_frame() {
    let mut smc = Smc::new();
    // Target verification reads mode three times, then startup and hold once.
    smc.after_target(4000., "F0Md", 5, Ok(byte(0)));
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.), Ok(70.)]);
    let mut out = Output::new(&clock);
    let error = adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap_err();
    assert!(matches!(error, Error::Control(_)));
    restored(&smc);
    assert_eq!(out.frames, 1);
    assert_eq!(
        smc.writes.iter().filter(|(key, _)| key == "F0Tg").count(),
        1
    );
}

#[test]
fn startup_snapshot_detects_target_takeover_before_reporting_ready() {
    let mut smc = Smc::new();
    smc.after_target(4000., "F0Tg", 3, Ok(float(2000.)));
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.)]);
    let mut out = Output::new(&clock);
    assert!(matches!(
        adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out),
        Err(Error::Control(_))
    ));
    restored(&smc);
    assert_eq!(out.frames, 0);
}

#[test]
fn one_post_update_rpm_failure_holds_target_then_recovers_on_healthy_sample() {
    let mut smc = Smc::new();
    smc.after_target(
        7000.,
        "F0Ac",
        3,
        Err(Error::Io("temporary tachometer error".into())),
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.), Ok(70.), Ok(70.), Ok(70.)]);
    let mut out = Output::new(&clock);
    out.limit = 3;
    assert_eq!(
        adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap(),
        Completion::Restored
    );
    restored(&smc);
    assert_eq!(source.calls, 4);
    assert_eq!(out.frames, 3);
    assert_eq!(
        out.events
            .iter()
            .filter(|e| e.contains("post-update telemetry unavailable"))
            .count(),
        1
    );
    assert_eq!(
        smc.writes.iter().filter(|(key, _)| key == "F0Tg").count(),
        2
    );
}

#[test]
fn three_post_update_failures_share_one_budget_despite_healthy_preliminary_reads() {
    let mut smc = Smc::new();
    for rpm in [5500., 5650., 5800.] {
        smc.after_target(
            rpm,
            "F0Ac",
            3,
            Err(Error::Io("temporary tachometer error".into())),
        );
    }
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![Ok(50.), Ok(60.), Ok(61.), Ok(62.)]);
    let mut out = Output::new(&clock);
    let error = adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("three consecutive telemetry failures")
    );
    restored(&smc);
    assert_eq!(source.calls, 4);
    assert_eq!(out.frames, 1);
    assert_eq!(
        out.events
            .iter()
            .filter(|e| e.contains("post-update telemetry unavailable"))
            .count(),
        3
    );
}

#[test]
fn stop_before_engagement_returns_no_writes_without_claiming_restoration() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    let mut source = Source::new(&clock, vec![]);
    let mut out = Output::new(&clock);
    out.limit = 0;
    assert_eq!(
        adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap(),
        Completion::NoWrites
    );
    assert!(smc.writes.is_empty());
    assert_eq!(out.frames, 0);
    assert!(
        !out.events
            .iter()
            .any(|e| e.contains("restoration verified"))
    );
}

#[test]
fn already_pending_signal_returns_no_writes_without_claiming_restoration() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let clock = TestClock::default();
    clock.stopped.set(true);
    let mut source = Source::new(&clock, vec![]);
    let mut out = Output::new(&clock);
    assert_eq!(
        adaptive::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap(),
        Completion::NoWrites
    );
    assert!(smc.writes.is_empty());
    assert_eq!(out.frames, 0);
    assert!(
        !out.events
            .iter()
            .any(|e| e.contains("restoration verified"))
    );
}
