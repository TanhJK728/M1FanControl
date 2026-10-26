//! Observer lifecycle hooks execute around mocked hardware only.
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::supervisor::{self, Frame, Observer, Transition};

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
struct Smc {
    data: BTreeMap<String, Value>,
    writes: Rc<RefCell<Vec<(String, Value)>>>,
    fail_restore: bool,
}
impl Smc {
    fn new(writes: Rc<RefCell<Vec<(String, Value)>>>) -> Self {
        Self {
            data: [
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
            writes,
            fail_restore: false,
        }
    }
}
impl ReadSmc for Smc {
    fn read(&mut self, key: &str) -> Result<Value> {
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
        self.writes.borrow_mut().push((key.into(), value.clone()));
        if self.fail_restore && key == "F0Md" && *value == byte(0) {
            return Err(Error::Io("restore transport failed".into()));
        }
        self.data.insert(key.into(), value.clone());
        if key == "F0Tg" {
            self.data.insert("F0Ac".into(), value.clone());
        }
        Ok(())
    }
}
#[derive(Default)]
struct Tick(Cell<Duration>);
impl Clock for Tick {
    fn wait(&self, d: Duration) -> bool {
        self.0.set(self.0.get() + d);
        true
    }
    fn settle(&self, d: Duration) {
        self.0.set(self.0.get() + d);
    }
    fn now(&self) -> Duration {
        self.0.get()
    }
}
struct Source(VecDeque<Result<f32>>);
impl ChipTemperatureSource for Source {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        let celsius = self.0.pop_front().unwrap_or(Ok(45.))?;
        Ok(vec![ChipReading {
            name: "pACC MTR Temp Sensor2".into(),
            group: SensorGroup::Performance,
            celsius,
        }])
    }
}
struct Output {
    writes: Rc<RefCell<Vec<(String, Value)>>>,
    frames: usize,
    limit: usize,
    transitions: Vec<Transition>,
    faults: Vec<(Error, Result<()>, usize)>,
    stop_on_fault: bool,
    fail_engaged_hook: bool,
    fault_frames: usize,
    finished: Vec<Result<()>>,
    fail_finished: bool,
    arming_writes: Vec<usize>,
    fail_arming: bool,
}
impl Output {
    fn new(writes: Rc<RefCell<Vec<(String, Value)>>>, limit: usize) -> Self {
        Self {
            writes,
            frames: 0,
            limit,
            transitions: vec![],
            faults: vec![],
            stop_on_fault: false,
            fail_engaged_hook: false,
            fault_frames: 0,
            finished: Vec::new(),
            fail_finished: false,
            arming_writes: vec![],
            fail_arming: false,
        }
    }
}
impl Observer for Output {
    fn arming(&mut self) -> Result<()> {
        self.arming_writes.push(self.writes.borrow().len());
        if self.fail_arming {
            return Err(Error::Io("recovery journal could not be persisted".into()));
        }
        Ok(())
    }
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(self.frames >= self.limit || (self.stop_on_fault && !self.faults.is_empty()))
    }
    fn transition(&mut self, kind: Transition) -> Result<()> {
        let writes = self.writes.borrow();
        match kind {
            Transition::Engaged => assert!(writes.last().is_some_and(|(key, _)| key == "F0Tg")),
            Transition::Released => assert_eq!(writes.last(), Some(&("F0Md".into(), byte(0)))),
        }
        self.transitions.push(kind);
        if kind == Transition::Engaged && self.fail_engaged_hook {
            return Err(Error::Io("hook failed".into()));
        }
        Ok(())
    }
    fn fault(&mut self, error: &Error, recovery: &Result<()>) -> Result<()> {
        let writes = self.writes.borrow();
        if self.fail_arming {
            assert!(
                writes.is_empty(),
                "arming error prevents every hardware write"
            );
        } else {
            assert_eq!(
                writes.last(),
                Some(&("F0Md".into(), byte(0))),
                "recovery attempt precedes hook"
            );
        }
        assert_eq!(self.fault_frames, 0, "hook precedes first fault frame");
        self.faults
            .push((error.clone(), recovery.clone(), writes.len()));
        Ok(())
    }
    fn finished(&mut self, recovery: &Result<()>) -> Result<()> {
        self.finished.push(recovery.clone());
        if self.fail_finished {
            return Err(Error::Io("finished hook failed".into()));
        }
        Ok(())
    }
    fn frame(&mut self, frame: &Frame) -> Result<()> {
        if frame.state.contains("FAULT LATCHED") {
            self.fault_frames += 1;
        }
        self.frames += 1;
        Ok(())
    }
    fn event(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn transitions_follow_verified_engagement_and_cooling_release() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let mut samples = vec![Ok(70.)];
    samples.extend(vec![Ok(45.); 34]);
    let mut source = Source(samples.into());
    let mut out = Output::new(writes, 35);
    supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap();
    assert_eq!(
        out.transitions,
        vec![Transition::Engaged, Transition::Released]
    );
    assert!(out.faults.is_empty());
    assert_eq!(out.arming_writes, vec![0]);
}

#[test]
fn arming_hook_failure_prevents_all_writes_and_still_reports_final_verification() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let mut source = Source(vec![Ok(70.)].into());
    let mut out = Output::new(writes.clone(), 4);
    out.fail_arming = true;
    let error = supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("recovery journal could not be persisted")
    );
    assert_eq!(out.arming_writes, vec![0]);
    assert!(writes.borrow().is_empty());
    assert!(out.transitions.is_empty());
    assert_eq!(out.faults.len(), 1);
    assert_eq!(out.faults[0].1, Ok(()));
    assert_eq!(out.finished, vec![Ok(())]);
    assert_eq!(smc.data["F0Md"], byte(0));
}

#[test]
fn fault_hook_receives_original_read_error_after_restoration_and_can_end_epoch() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let error = Error::Io("temporary HID transport".into());
    let mut source = Source(
        vec![
            Ok(70.),
            Err(error.clone()),
            Err(error.clone()),
            Err(error.clone()),
            Ok(70.),
        ]
        .into(),
    );
    let mut out = Output::new(writes, 10);
    out.stop_on_fault = true;
    supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap();
    assert_eq!(out.faults.len(), 1);
    assert_eq!(out.faults[0].0, error);
    assert_eq!(out.faults[0].1, Ok(()));
    assert_eq!(out.frames, 4);
    assert_eq!(out.transitions, vec![Transition::Engaged]);
}

#[test]
fn failed_restore_is_exposed_to_outer_reconnect_owner() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    smc.fail_restore = true;
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let error = Error::Io("temporary HID transport".into());
    let mut source = Source(
        vec![
            Ok(70.),
            Err(error.clone()),
            Err(error.clone()),
            Err(error.clone()),
        ]
        .into(),
    );
    let mut out = Output::new(writes.clone(), 10);
    out.stop_on_fault = true;
    assert!(supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    assert_eq!(out.faults.len(), 1);
    assert_eq!(out.faults[0].0, error);
    assert!(out.faults[0].1.is_err());
    assert_eq!(out.finished.len(), 1);
    assert!(out.finished[0].is_err());
    assert_eq!(
        writes
            .borrow()
            .iter()
            .filter(|(key, v)| key == "F0Md" && *v == byte(0))
            .count(),
        6
    );
}

#[test]
fn hook_error_after_engagement_still_preserves_restoration_responsibility() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let mut source = Source(vec![Ok(70.)].into());
    let mut out = Output::new(writes, 4);
    out.fail_engaged_hook = true;
    assert!(supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).is_err());
    assert_eq!(smc.data["F0Md"], byte(0));
    assert_eq!(out.faults.len(), 1);
    assert_eq!(out.faults[0].1, Ok(()));
    assert_eq!(
        out.finished,
        vec![Ok(())],
        "operation error must not hide verified recovery"
    );
}

#[test]
fn finished_hook_error_is_returned_after_restoration_has_completed() {
    let writes = Rc::new(RefCell::new(vec![]));
    let mut smc = Smc::new(writes.clone());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = Tick::default();
    let mut source = Source(vec![Ok(70.)].into());
    let mut out = Output::new(writes, 2);
    out.fail_finished = true;
    let error = supervisor::run(&mut smc, &plan, &mut source, &clock, &mut out).unwrap_err();
    assert!(error.to_string().contains("finished hook failed"));
    assert_eq!(smc.data["F0Md"], byte(0));
    assert_eq!(out.finished, vec![Ok(())]);
}
