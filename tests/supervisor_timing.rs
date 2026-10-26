//! Regression tests for cancellation between compound writes and startup readiness.
//! All I/O and elapsed time are simulated; no native hardware is opened.
use m1fan::chip_temps::{ChipReading, ChipTemperatureSource, SensorGroup};
use m1fan::control::Clock;
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::supervisor::{self, Completion, Frame, Observer};
use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

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
struct FakeClock(Cell<Duration>);

impl FakeClock {
    fn advance(&self, duration: Duration) {
        self.0.set(self.0.get() + duration);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.get()
    }

    fn wait(&self, duration: Duration) -> bool {
        self.advance(duration);
        true
    }

    fn settle(&self, duration: Duration) {
        self.advance(duration);
    }
}

struct FakeSmc<'a> {
    data: BTreeMap<String, Value>,
    clock: &'a FakeClock,
    mode: &'a Cell<u32>,
    writes: Vec<(String, Value)>,
    target_writes: usize,
    slow_manual_write: bool,
    fail_target_write: bool,
    slow_failed_post_update_read: bool,
    delayed_return_at: Option<Duration>,
    restored_at: Option<Duration>,
}

impl<'a> FakeSmc<'a> {
    fn new(clock: &'a FakeClock, mode: &'a Cell<u32>) -> Self {
        Self {
            data: [
                ("FNum", byte(1)),
                ("F0Md", byte(0)),
                ("F0Mn", float(1000.0)),
                ("F0Mx", float(7000.0)),
                ("F0Ac", float(2000.0)),
                ("F0Tg", float(1900.0)),
                ("TB1T", float(30.0)),
                ("TB2T", float(29.0)),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
            clock,
            mode,
            writes: Vec::new(),
            target_writes: 0,
            slow_manual_write: false,
            fail_target_write: false,
            slow_failed_post_update_read: false,
            delayed_return_at: None,
            restored_at: None,
        }
    }
}

impl ReadSmc for FakeSmc<'_> {
    fn read(&mut self, key: &str) -> Result<Value> {
        // Target observation reads Md/Tg/Ac. The first Mn read after the second
        // target write is the supervisor's post-update fan snapshot.
        if self.slow_failed_post_update_read && self.target_writes == 2 && key == "F0Mn" {
            self.slow_failed_post_update_read = false;
            self.clock.advance(Duration::from_secs(6));
            self.delayed_return_at = Some(self.clock.now());
            return Err(Error::Io("late post-update read failed".into()));
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

impl WriteSmc for FakeSmc<'_> {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        self.writes.push((key.into(), value.clone()));
        if key == "F0Tg" {
            self.target_writes += 1;
            if self.fail_target_write {
                return Err(Error::Io("initial target write failed".into()));
            }
            self.data.insert("F0Ac".into(), value.clone());
        }
        self.data.insert(key.into(), value.clone());
        if key == "F0Md" {
            let mode = value.integer()?;
            self.mode.set(mode);
            if mode == 1 && self.slow_manual_write {
                self.slow_manual_write = false;
                self.clock.advance(Duration::from_secs(6));
                self.delayed_return_at = Some(self.clock.now());
            } else if mode == 0 {
                self.restored_at = Some(self.clock.now());
            }
        }
        Ok(())
    }
}

struct Source(VecDeque<f32>);

impl ChipTemperatureSource for Source {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        Ok(vec![ChipReading {
            name: "pACC MTR Temp Sensor2".into(),
            group: SensorGroup::Performance,
            celsius: self.0.pop_front().unwrap_or(90.0),
        }])
    }
}

struct Output<'a> {
    mode: &'a Cell<u32>,
    ready_calls: usize,
    frames: Vec<(String, u32)>,
}

impl<'a> Output<'a> {
    fn new(mode: &'a Cell<u32>) -> Self {
        Self {
            mode,
            ready_calls: 0,
            frames: Vec::new(),
        }
    }
}

impl Observer for Output<'_> {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(self.frames.len() >= 3)
    }

    fn ready(&mut self, frame: &Frame) -> Result<()> {
        self.ready_calls += 1;
        self.frame(frame)
    }

    fn frame(&mut self, frame: &Frame) -> Result<()> {
        self.frames.push((frame.line(), self.mode.get()));
        Ok(())
    }

    fn event(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}

#[test]
fn slow_manual_write_restores_before_target_write_and_never_announces_ready() {
    let clock = FakeClock::default();
    let mode = Cell::new(0);
    let mut smc = FakeSmc::new(&clock, &mode);
    smc.slow_manual_write = true;
    let plan = fan::plan(&mut smc).unwrap();
    let mut source = Source(VecDeque::from([70.0]));
    let mut output = Output::new(&mode);

    let error = supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap_err();

    assert!(error.to_string().contains("startup failed"));
    assert!(error.to_string().contains("sampling gap exceeded 5s"));
    assert_eq!(smc.target_writes, 0);
    assert_eq!(
        smc.writes,
        vec![("F0Md".into(), byte(1)), ("F0Md".into(), byte(0))]
    );
    assert_eq!(smc.delayed_return_at, Some(Duration::from_secs(6)));
    assert_eq!(smc.restored_at, smc.delayed_return_at);
    assert_eq!(output.ready_calls, 0);
    assert_eq!(output.frames.len(), 1);
    assert_eq!(output.frames[0].1, 0);
    assert!(output.frames[0].0.contains("FAULT LATCHED"));
}

#[test]
fn late_failed_post_update_read_restores_in_the_same_cycle_and_latches() {
    let clock = FakeClock::default();
    let mode = Cell::new(0);
    let mut smc = FakeSmc::new(&clock, &mode);
    smc.slow_failed_post_update_read = true;
    let plan = fan::plan(&mut smc).unwrap();
    let mut source = Source(VecDeque::from([55.0, 70.0, 90.0]));
    let mut output = Output::new(&mode);

    assert_eq!(
        supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap(),
        Completion::AutomaticVerified
    );

    assert!(smc.delayed_return_at.is_some());
    assert_eq!(smc.restored_at, smc.delayed_return_at);
    assert_eq!(smc.target_writes, 2);
    assert_eq!(smc.writes.last(), Some(&("F0Md".into(), byte(0))));
    assert_eq!(output.ready_calls, 1);
    assert_eq!(output.frames.len(), 3);
    assert_eq!(output.frames[0].1, 1);
    for (line, observed_mode) in &output.frames[1..] {
        assert!(line.contains("FAULT LATCHED"));
        assert!(line.contains("sampling gap exceeded 5s"));
        assert_eq!(*observed_mode, 0);
    }
}

#[test]
fn first_target_write_failure_restores_before_reporting_without_ready() {
    let clock = FakeClock::default();
    let mode = Cell::new(0);
    let mut smc = FakeSmc::new(&clock, &mode);
    smc.fail_target_write = true;
    let plan = fan::plan(&mut smc).unwrap();
    let mut source = Source(VecDeque::from([70.0]));
    let mut output = Output::new(&mode);

    let error = supervisor::run(&mut smc, &plan, &mut source, &clock, &mut output).unwrap_err();

    assert!(error.to_string().contains("startup failed"));
    assert!(error.to_string().contains("initial target write failed"));
    assert_eq!(smc.target_writes, 1);
    assert_eq!(smc.writes.len(), 3);
    assert_eq!(smc.writes.last(), Some(&("F0Md".into(), byte(0))));
    assert_eq!(output.ready_calls, 0);
    assert_eq!(output.frames.len(), 1);
    assert!(output.frames[0].0.contains("FAULT LATCHED"));
    assert_eq!(output.frames[0].1, 0);
}
