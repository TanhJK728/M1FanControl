//! End-to-end orchestration uses only fake SMC/temperature/clock capabilities.
use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::supervisor::{self, Completion, Frame, Observer};
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
    fail_target: bool,
    fail_restore: bool,
}
impl Smc {
    fn new() -> Self {
        Self {
            data: [
                ("FNum", byte(1)),
                ("TB1T", float(30.)),
                ("TB2T", float(29.)),
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
            fail_target: false,
            fail_restore: false,
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
        if key == "F0Md" && value == &byte(0) && self.fail_restore {
            return Err(Error::Io("injected restore failure".into()));
        }
        if key == "F0Tg" && self.fail_target {
            return Err(Error::Io("injected write error".into()));
        }
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
    states: Vec<String>,
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
            states: Vec::new(),
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
    fn frame(&mut self, f: &Frame) -> Result<()> {
        self.states.push(f.line());
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
fn idle_stop_verifies_automatic_without_any_writes() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(40.); 5]);
    let mut out = Output::new(&c);
    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap(),
        Completion::AutomaticVerified
    );
    assert!(smc.writes.is_empty());
    assert!(
        out.states
            .iter()
            .all(|s| s.contains("automatic; monitoring"))
    );
}
#[test]
fn battery_independently_requests_max_at_cool_chip() {
    let mut smc = Smc::new();
    smc.data.insert("TB2T".into(), float(40.));
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(40.); 5]);
    let mut out = Output::new(&c);
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert!(smc.writes.contains(&("F0Tg".into(), float(7000.))));
    assert!(out.states[0].contains("chip=false battery=true"));
}
#[test]
fn chip_cools_releases_then_reengages_on_new_heat() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut sequence = vec![Ok(70.)];
    sequence.extend(vec![Ok(45.); 33]);
    sequence.push(Ok(70.));
    let mut src = Source::new(&c, sequence);
    let mut out = Output::new(&c);
    out.limit = 35;
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert_eq!(
        smc.writes
            .iter()
            .filter(|(k, v)| k == "F0Md" && v == &byte(1))
            .count(),
        2
    );
    assert!(
        out.states
            .iter()
            .any(|s| s.contains("automatic; monitoring"))
    );
}
#[test]
fn three_sensor_failures_restore_and_never_reacquire_even_when_hot() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(
        &c,
        vec![
            Ok(70.),
            Err(Error::Io("lost".into())),
            Err(Error::Io("lost".into())),
            Err(Error::Io("lost".into())),
            Ok(90.),
            Ok(90.),
        ],
    );
    let mut out = Output::new(&c);
    out.limit = 6;
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert_eq!(smc.writes.iter().filter(|(k, _)| k == "F0Tg").count(), 1);
    assert!(out.states.last().unwrap().contains("FAULT LATCHED"));
}
#[test]
fn ownership_overwrite_faults_on_first_cycle_and_never_rewrites_target() {
    let mut smc = Smc::new();
    smc.after_target(7000., "TB1T", 0, Ok(float(30.)));
    smc.after_target(7000., "F0Md", 4, Ok(byte(0)));
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(80.); 5]);
    let mut out = Output::new(&c);
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert_eq!(smc.writes.iter().filter(|(k, _)| k == "F0Tg").count(), 1);
    assert!(out.states.iter().any(|s| s.contains("FAULT LATCHED")));
}
#[test]
fn failed_target_after_mode_write_restores_immediately() {
    let mut smc = Smc::new();
    smc.fail_target = true;
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.); 5]);
    let mut out = Output::new(&c);
    assert!(supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).is_err());
    restored(&smc);
    assert!(out.states[0].contains("FAULT LATCHED"));
    assert_eq!(smc.writes.len(), 3);
}
#[test]
fn signal_after_ready_restores_and_stops() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.)]);
    let mut out = Output::new(&c);
    out.signal_ready = true;
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert_eq!(out.frames, 1);
}
#[test]
fn failed_startup_handshake_restores_before_returning_error() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.)]);
    let mut out = Output::new(&c);
    out.ready_failure = true;
    assert!(supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).is_err());
    restored(&smc);
}
#[test]
fn sleep_gap_releases_and_latches() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.); 5]);
    src.pause_at = Some(2);
    let mut out = Output::new(&c);
    supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap();
    restored(&smc);
    assert!(out.states[1].contains("FAULT LATCHED"));
    assert_eq!(smc.writes.iter().filter(|(k, _)| k == "F0Tg").count(), 1);
}
#[test]
fn absent_battery_never_enters_manual_and_latches_after_three_reads() {
    let mut smc = Smc::new();
    smc.data.remove("TB2T");
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(80.); 5]);
    let mut out = Output::new(&c);
    assert!(supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).is_err());
    assert!(smc.writes.is_empty());
    assert!(out.states[2].contains("FAULT LATCHED"));
}
#[test]
fn readonly_preview_shows_battery_demand_without_writing() {
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
    smc.0.data.insert("TB1T".into(), float(38.));
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(40.); 2]);
    let mut out = Output::new(&c);
    supervisor::preview(&mut smc, &plan, &mut src, &c, Some(2), &mut out).unwrap();
    assert!(smc.0.writes.is_empty());
    assert!(out.states[0].contains("requested 5200"));
}

#[test]
fn restoration_failure_is_not_reported_as_success_and_is_bounded() {
    let mut smc = Smc::new();
    smc.fail_restore = true;
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.); 5]);
    let mut out = Output::new(&c);
    out.signal_ready = true;
    assert!(supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).is_err());
    assert!(out.events.last().unwrap().contains("FAILED"));
    assert_eq!(
        smc.writes
            .iter()
            .filter(|(k, v)| k == "F0Md" && v == &byte(0))
            .count(),
        6
    );
}
#[test]
fn cancelled_before_control_performs_no_writes() {
    let mut smc = Smc::new();
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    c.stopped.set(true);
    let mut src = Source::new(&c, vec![Ok(70.)]);
    let mut out = Output::new(&c);
    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).unwrap(),
        Completion::AutomaticVerified
    );
    assert!(smc.writes.is_empty());
}
#[test]
fn preexisting_other_manual_controller_is_never_overwritten() {
    let mut smc = Smc::new();
    smc.data.insert("F0Md".into(), byte(1));
    let plan = fan::plan(&mut smc).unwrap();
    let c = TestClock::default();
    let mut src = Source::new(&c, vec![Ok(70.); 5]);
    let mut out = Output::new(&c);
    assert!(supervisor::run(&mut smc, &plan, &mut src, &c, &mut out).is_err());
    assert!(smc.writes.is_empty());
    assert!(out.states[0].contains("FAULT LATCHED"));
}
