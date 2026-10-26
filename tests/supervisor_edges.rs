//! Focused regressions for restoration, release-time watchdogs and lost ownership.
//! All device operations run against an in-memory SMC; no native APIs are opened.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::supervisor::{self, Completion, Frame, Observer};

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

#[derive(Default)]
struct ClockMock {
    now: Cell<Duration>,
}
impl Clock for ClockMock {
    fn wait(&self, duration: Duration) -> bool {
        self.now.set(self.now.get() + duration);
        true
    }
    fn settle(&self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }
    fn now(&self) -> Duration {
        self.now.get()
    }
}

#[derive(Default)]
struct Faults {
    tachometer_failed: bool,
    mode_reads: VecDeque<Value>,
}

struct SmcMock {
    values: BTreeMap<String, Value>,
    writes: Vec<(String, Value)>,
    faults: Rc<RefCell<Faults>>,
    stalled: bool,
}
impl SmcMock {
    fn new(faults: Rc<RefCell<Faults>>) -> Self {
        Self {
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
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
            writes: Vec::new(),
            faults,
            stalled: false,
        }
    }
    fn engagements(&self) -> usize {
        self.writes
            .iter()
            .filter(|(key, value)| key == "F0Md" && value == &byte(1))
            .count()
    }
    fn assert_restored(&self) {
        assert_eq!(self.values["F0Md"], byte(0));
        assert_eq!(self.writes.last(), Some(&("F0Md".into(), byte(0))));
    }
}
impl ReadSmc for SmcMock {
    fn read(&mut self, key: &str) -> Result<Value> {
        let mut faults = self.faults.borrow_mut();
        if key == "F0Ac" && faults.tachometer_failed {
            return Err(Error::Io("persistent tachometer failure".into()));
        }
        if key == "F0Md"
            && let Some(value) = faults.mode_reads.pop_front()
        {
            return Ok(value);
        }
        self.values
            .get(key)
            .cloned()
            .ok_or_else(|| Error::Missing(key.into()))
    }
    fn keys(&mut self) -> Result<Vec<String>> {
        Ok(self.values.keys().cloned().collect())
    }
}
impl WriteSmc for SmcMock {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        self.writes.push((key.into(), value.clone()));
        self.values.insert(key.into(), value.clone());
        if key == "F0Tg" {
            self.values.insert(
                "F0Ac".into(),
                if self.stalled {
                    float(0.)
                } else {
                    value.clone()
                },
            );
        }
        if key == "F0Md" && value == &byte(0) {
            self.faults.borrow_mut().mode_reads.clear();
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    TachometerFailure,
    ReleaseAtWatchdogDeadline,
    LostModeDuringTemperatureFailure,
}
struct SourceMock<'a> {
    scenario: Scenario,
    calls: usize,
    clock: &'a ClockMock,
    faults: Rc<RefCell<Faults>>,
}
impl ChipTemperatureSource for SourceMock<'_> {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        self.calls += 1;
        let celsius = match self.scenario {
            Scenario::TachometerFailure => {
                if self.calls == 2 {
                    self.faults.borrow_mut().tachometer_failed = true;
                }
                70.
            }
            Scenario::ReleaseAtWatchdogDeadline => {
                if self.calls == 1 || self.clock.now() >= Duration::from_secs(32) {
                    70.
                } else {
                    45.
                }
            }
            Scenario::LostModeDuringTemperatureFailure => {
                if self.calls == 2 {
                    self.faults.borrow_mut().mode_reads = VecDeque::from([byte(0), byte(1)]);
                    return Err(Error::Io("temporary chip event failure".into()));
                }
                70.
            }
        };
        Ok(vec![ChipReading {
            name: "pACC MTR Temp Sensor2".into(),
            group: SensorGroup::Performance,
            celsius,
        }])
    }
}

struct RecordedFrame {
    elapsed: Duration,
    state: String,
    problem: Option<String>,
}
struct OutputMock {
    limit: usize,
    frames: Vec<RecordedFrame>,
    events: Vec<String>,
}
impl OutputMock {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            frames: Vec::new(),
            events: Vec::new(),
        }
    }
}
impl Observer for OutputMock {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(self.frames.len() >= self.limit)
    }
    fn frame(&mut self, frame: &Frame) -> Result<()> {
        self.frames.push(RecordedFrame {
            elapsed: frame.elapsed,
            state: frame.state.clone(),
            problem: frame.problem.clone(),
        });
        Ok(())
    }
    fn event(&mut self, message: &str) -> Result<()> {
        self.events.push(message.into());
        Ok(())
    }
}

#[test]
fn persistent_tachometer_fault_does_not_prevent_verified_automatic_stop() {
    let faults = Rc::new(RefCell::new(Faults::default()));
    let mut smc = SmcMock::new(faults.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = ClockMock::default();
    let mut source = SourceMock {
        scenario: Scenario::TachometerFailure,
        calls: 0,
        clock: &clock,
        faults: faults.clone(),
    };
    let mut output = OutputMock::new(5);
    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap(),
        Completion::AutomaticVerified
    );
    assert!(
        faults.borrow().tachometer_failed,
        "fault must persist through final verification"
    );
    smc.assert_restored();
    assert_eq!(smc.engagements(), 1);
    assert!(output.frames[3].state.contains("FAULT LATCHED"));
    assert!(
        output
            .events
            .last()
            .unwrap()
            .contains("automatic mode verified")
    );
}

#[test]
fn first_spinup_deadline_on_release_cycle_latches_fault_and_blocks_reengagement() {
    let faults = Rc::new(RefCell::new(Faults::default()));
    let mut smc = SmcMock::new(faults.clone());
    smc.stalled = true;
    smc.values.insert("F0Ac".into(), float(0.));
    let plan = fan::plan(&mut smc).unwrap();
    let clock = ClockMock::default();
    let mut source = SourceMock {
        scenario: Scenario::ReleaseAtWatchdogDeadline,
        calls: 0,
        clock: &clock,
        faults,
    };
    let mut output = OutputMock::new(36);
    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap(),
        Completion::AutomaticVerified
    );
    smc.assert_restored();
    let first_fault = output
        .frames
        .iter()
        .find(|frame| frame.state.contains("FAULT LATCHED"))
        .expect("stall at the first 30-second watchdog check must latch");
    assert!(
        first_fault.elapsed >= Duration::from_secs(31)
            && first_fault.elapsed < Duration::from_secs(32)
    );
    assert!(first_fault.problem.as_ref().unwrap().contains("below 80%"));
    assert_eq!(
        smc.engagements(),
        1,
        "new heat after t=32 must not reacquire a faulted fan"
    );
    assert!(
        output
            .frames
            .last()
            .unwrap()
            .state
            .contains("FAULT LATCHED")
    );
}

#[test]
fn same_cycle_temperature_error_cannot_hide_observed_mode_loss() {
    let faults = Rc::new(RefCell::new(Faults::default()));
    let mut smc = SmcMock::new(faults.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = ClockMock::default();
    let mut source = SourceMock {
        scenario: Scenario::LostModeDuringTemperatureFailure,
        calls: 0,
        clock: &clock,
        faults,
    };
    let mut output = OutputMock::new(4);
    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap(),
        Completion::AutomaticVerified
    );
    smc.assert_restored();
    assert!(
        output.frames[1].state.contains("FAULT LATCHED"),
        "observed mode=0 must fault in the first failing-temperature cycle"
    );
    assert!(
        output.frames[1]
            .problem
            .as_ref()
            .unwrap()
            .contains("ownership changed")
    );
    assert_eq!(smc.engagements(), 1);
    assert_eq!(
        smc.writes.iter().filter(|(key, _)| key == "F0Tg").count(),
        1
    );
}
