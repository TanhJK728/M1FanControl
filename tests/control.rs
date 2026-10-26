use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use m1fan::control::{Clock, FailureWindow, Session, restore_all};
use m1fan::fan;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc, encode_rpm, validate_write};
use m1fan::temps::Temperatures;

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

#[derive(Clone)]
enum Action {
    Fail { error: Error, mutate_first: bool },
    Ignore,
    ReadSequences(Vec<(String, Vec<Result<Value>>)>),
}

#[derive(Clone, Debug)]
enum Operation {
    Read(String, Result<Value>),
    Write(String, Value),
}

struct Rule {
    key: String,
    value: Value,
    remaining: usize,
    action: Action,
}

#[derive(Default)]
struct MockSmc {
    data: BTreeMap<String, Value>,
    reads: Vec<String>,
    writes: Vec<(String, Value)>,
    rules: Vec<Rule>,
    read_sequences: BTreeMap<String, VecDeque<Result<Value>>>,
    operations: Vec<Operation>,
}

impl MockSmc {
    fn fans(maxima: &[f32], ftst: bool) -> Self {
        let mut smc = Self::default();
        smc.data.insert("FNum".into(), byte(maxima.len() as u8));
        for (index, &maximum) in maxima.iter().enumerate() {
            for (suffix, value) in [
                ("Mn", float(1200.0)),
                ("Mx", float(maximum)),
                ("Ac", float(1300.0)),
                ("Tg", float(0.0)),
                ("md", byte(0)),
            ] {
                smc.data.insert(format!("F{index}{suffix}"), value);
            }
        }
        if ftst {
            smc.data.insert("Ftst".into(), byte(0));
        }
        smc.data.insert("Tp0P".into(), float(43.5));
        smc.data.insert(
            "Te0S".into(),
            Value {
                kind: *b"sp78",
                bytes: vec![45, 128],
            },
        );
        smc
    }

    fn script(&mut self, key: &str, value: Value, remaining: usize, action: Action) {
        self.rules.push(Rule {
            key: key.into(),
            value,
            remaining,
            action,
        });
    }

    fn fail(
        &mut self,
        key: &str,
        value: Value,
        remaining: usize,
        error: Error,
        mutate_first: bool,
    ) {
        self.script(
            key,
            value,
            remaining,
            Action::Fail {
                error,
                mutate_first,
            },
        );
    }

    fn read_after_write(
        &mut self,
        write_key: &str,
        value: Value,
        read_key: &str,
        samples: Vec<Result<Value>>,
    ) {
        self.script(
            write_key,
            value,
            1,
            Action::ReadSequences(vec![(read_key.into(), samples)]),
        );
    }

    fn reads_until_next_write(&self, key: &str, value: &Value) -> Vec<(&str, &Result<Value>)> {
        let start = self
            .operations
            .iter()
            .position(
                |operation| matches!(operation, Operation::Write(k, v) if k == key && v == value),
            )
            .expect("scripted write was not attempted");
        self.operations[start + 1..]
            .iter()
            .take_while(|operation| matches!(operation, Operation::Read(..)))
            .map(|operation| match operation {
                Operation::Read(key, result) => (key.as_str(), result),
                Operation::Write(..) => unreachable!(),
            })
            .collect()
    }

    fn assert_target_writes_in_bounds(&self) {
        for (key, value) in &self.writes {
            if let Some(prefix) = key.strip_suffix("Tg") {
                let rpm = value.number().unwrap();
                let min = self.number(&format!("{prefix}Mn"));
                let max = self.number(&format!("{prefix}Mx"));
                assert!(
                    (min..=max).contains(&rpm),
                    "{key}={rpm} outside {min}..={max}"
                );
            }
        }
    }

    fn number(&self, key: &str) -> f32 {
        self.data[key].number().unwrap()
    }

    fn wrote(&self, key: &str, value: &Value) -> bool {
        self.writes.iter().any(|(k, v)| k == key && v == value)
    }

    fn automatic(&self) {
        for (key, value) in &self.data {
            if key.ends_with("md") || key.ends_with("Md") {
                assert!(
                    matches!(value.integer().unwrap(), 0 | 3),
                    "{key} remains manual"
                );
            }
        }
        if self.data.contains_key("Ftst") {
            assert_eq!(self.number("Ftst"), 0.0);
        }
    }
}

impl ReadSmc for MockSmc {
    fn read(&mut self, key: &str) -> Result<Value> {
        self.reads.push(key.into());
        let result = if let Some(result) = self
            .read_sequences
            .get_mut(key)
            .and_then(VecDeque::pop_front)
        {
            if let Ok(value) = &result {
                self.data.insert(key.into(), value.clone());
            }
            result
        } else {
            self.data
                .get(key)
                .cloned()
                .ok_or_else(|| Error::Missing(key.into()))
        };
        self.operations
            .push(Operation::Read(key.into(), result.clone()));
        result
    }

    fn keys(&mut self) -> Result<Vec<String>> {
        Ok(self.data.keys().cloned().collect())
    }
}

impl WriteSmc for MockSmc {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        self.writes.push((key.into(), value.clone()));
        self.operations
            .push(Operation::Write(key.into(), value.clone()));
        validate_write(key, value)?;
        if !self.data.contains_key(key) {
            return Err(Error::Missing(key.into()));
        }
        let action = self
            .rules
            .iter_mut()
            .find(|rule| rule.remaining > 0 && rule.key == key && rule.value == *value)
            .map(|rule| {
                rule.remaining -= 1;
                rule.action.clone()
            });
        match action {
            Some(Action::Fail {
                error,
                mutate_first,
            }) => {
                if mutate_first {
                    self.data.insert(key.into(), value.clone());
                }
                Err(error)
            }
            Some(Action::Ignore) => Ok(()),
            Some(Action::ReadSequences(sequences)) => {
                self.data.insert(key.into(), value.clone());
                for (read_key, samples) in sequences {
                    self.read_sequences.insert(read_key, samples.into());
                }
                Ok(())
            }
            None => {
                self.data.insert(key.into(), value.clone());
                Ok(())
            }
        }
    }
}

#[derive(Default)]
struct MockClock {
    waits: RefCell<Vec<Duration>>,
    settles: RefCell<Vec<Duration>>,
    interrupt_at: Option<Duration>,
    elapsed: Cell<Duration>,
}

impl MockClock {
    fn interrupt_at(duration: Duration) -> Self {
        Self {
            interrupt_at: Some(duration),
            ..Self::default()
        }
    }
}

impl Clock for MockClock {
    fn wait(&self, duration: Duration) -> bool {
        self.waits.borrow_mut().push(duration);
        if self.interrupt_at == Some(duration) {
            return false;
        }
        self.elapsed.set(self.elapsed.get() + duration);
        true
    }

    fn settle(&self, duration: Duration) {
        self.settles.borrow_mut().push(duration);
        self.elapsed.set(self.elapsed.get() + duration);
    }

    fn now(&self) -> Duration {
        self.elapsed.get()
    }
}

fn refusal(key: &str) -> Error {
    Error::Smc {
        key: key.into(),
        code: 0x82,
    }
}

#[test]
fn finish_reports_the_final_successful_recovery_and_does_not_retry_in_drop() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let writes_after_finish;
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().fail(
            "F0md",
            byte(0),
            3,
            Error::Io("temporary failure".into()),
            false,
        );
        session.finish().unwrap();
        session.smc().automatic();
        writes_after_finish = session.smc().writes.len();
    }
    assert_eq!(smc.writes.len(), writes_after_finish);
}

#[test]
fn finish_reports_exhausted_recovery_without_a_later_hidden_outcome() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let writes_after_finish;
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().fail(
            "F0md",
            byte(0),
            6,
            Error::Io("persistent failure".into()),
            false,
        );
        assert!(session.finish().is_err());
        writes_after_finish = session.smc().writes.len();
    }
    assert_eq!(smc.writes.len(), writes_after_finish);
    assert_eq!(smc.number("F0md"), 1.0);
}

#[test]
fn max_uses_each_fans_hardware_limit_and_restores_all() {
    let mut smc = MockSmc::fans(&[6123.5, 5432.25], true);
    smc.data
        .insert("F1Tg".into(), encode_rpm(*b"fpe2", 0.0).unwrap());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        assert_eq!(session.smc().number("F0Tg"), 6123.5);
        assert_eq!(session.smc().number("F1Tg"), 5432.25);
        assert_eq!(session.smc().number("F0md"), 1.0);
        assert_eq!(session.smc().number("F1md"), 1.0);
        assert!(
            !session.smc().wrote("Ftst", &byte(1)),
            "direct mode must not unlock unnecessarily"
        );
        session.restore().unwrap();
    }
    smc.automatic();
    assert_eq!(smc.number("F0Tg"), 6123.5);
    assert_eq!(smc.number("F1Tg"), 5432.25);
    smc.assert_target_writes_in_bounds();
    assert!(
        smc.writes
            .iter()
            .all(|(key, value)| validate_write(key, value).is_ok())
    );
}

#[test]
fn max_samples_limit_at_engagement_after_discovery() {
    let mut smc = MockSmc::fans(&[5000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.data.insert("F0Mx".into(), float(6217.75));
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        assert_eq!(session.smc().number("F0Tg"), 6217.75);
        session.restore().unwrap();
    }
    smc.automatic();
}

#[test]
fn invalid_or_missing_maximum_prevents_every_write_even_with_multiple_fans() {
    for invalid in [
        None,
        Some(float(0.0)),
        Some(float(1000.0)),
        Some(float(-1.0)),
        Some(float(f32::NAN)),
        Some(float(f32::INFINITY)),
        Some(byte(255)),
    ] {
        let mut smc = MockSmc::fans(&[6000.0, 5500.0], true);
        match invalid {
            Some(value) => {
                smc.data.insert("F1Mx".into(), value);
            }
            None => {
                smc.data.remove("F1Mx");
            }
        }
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        {
            assert!(Session::new(&mut smc, plan, &clock).engage_max().is_err());
        }
        assert!(
            smc.writes.is_empty(),
            "preflight wrote hardware: {:?}",
            smc.writes
        );
    }
}

#[test]
fn unrepresentable_maximum_prevents_every_write() {
    let mut smc = MockSmc::fans(&[6000.1], false);
    smc.data
        .insert("F0Tg".into(), encode_rpm(*b"fpe2", 0.0).unwrap());
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    assert!(Session::new(&mut smc, plan, &clock).engage_max().is_err());
    assert!(smc.writes.is_empty());
}

#[test]
fn known_firmware_refusal_uses_ftst_unlock_then_releases_it() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail("F0md", byte(1), 1, refusal("F0md"), false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        assert_eq!(session.smc().number("Ftst"), 1.0);
        assert_eq!(session.smc().number("F0Tg"), 6000.0);
        session.restore().unwrap();
    }
    assert_eq!(
        &smc.writes[..4],
        &[
            ("F0md".into(), byte(1)),
            ("Ftst".into(), byte(1)),
            ("F0md".into(), byte(1)),
            ("F0Tg".into(), float(6000.0))
        ]
    );
    assert!(clock.waits.borrow().contains(&Duration::from_secs(3)));
    smc.automatic();
}

#[test]
fn unknown_firmware_error_does_not_activate_unlock() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail(
        "F0md",
        byte(1),
        1,
        Error::Smc {
            key: "F0md".into(),
            code: 0x85,
        },
        false,
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        assert!(Session::new(&mut smc, plan, &clock).engage_max().is_err());
    }
    assert!(!smc.wrote("Ftst", &byte(1)));
    smc.automatic();
}

#[test]
fn permission_failure_after_possible_mutation_still_arms_cleanup() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail(
        "F0md",
        byte(1),
        1,
        Error::Permission("scripted permission error".into()),
        true,
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(matches!(session.engage_max(), Err(Error::Permission(_))));
        assert_eq!(session.smc().number("F0md"), 1.0);
    }
    assert!(smc.wrote("F0md", &byte(0)));
    assert!(!smc.wrote("Ftst", &byte(1)));
    smc.automatic();
}

#[test]
fn target_failure_after_mutation_restores_mode_without_an_out_of_range_target_reset() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail(
        "F0Tg",
        float(6000.0),
        5,
        Error::Io("scripted target failure".into()),
        true,
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(session.engage_max().is_err());
        assert_eq!(session.smc().number("F0Tg"), 6000.0);
    }
    smc.automatic();
    assert_eq!(smc.number("F0Tg"), 6000.0);
    smc.assert_target_writes_in_bounds();
}

#[test]
fn later_fan_failure_restores_previously_engaged_fans() {
    let mut smc = MockSmc::fans(&[6000.0, 5500.0], true);
    smc.fail(
        "F1md",
        byte(1),
        1,
        Error::Permission("second fan".into()),
        true,
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(session.engage_max().is_err());
        assert_eq!(session.smc().number("F0Tg"), 6000.0);
    }
    assert!(smc.wrote("F0md", &byte(0)));
    assert!(smc.wrote("F1md", &byte(0)));
    smc.automatic();
}

#[test]
fn auto_requires_no_rpm_or_temperature_readings() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    for key in ["F0Mn", "F0Mx", "F0Ac", "Tp0P", "Te0S"] {
        smc.data.remove(key);
    }
    smc.data.insert("F0md".into(), byte(1));
    smc.data.insert("Ftst".into(), byte(1));
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    restore_all(&mut smc, &plan, &clock).unwrap();
    smc.automatic();
    assert!(
        !smc.reads
            .iter()
            .any(|key| ["F0Mn", "F0Mx", "Tp0P", "Te0S"].contains(&key.as_str()))
    );
    assert!(smc.reads.iter().any(|key| key == "F0Ac"));
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
}

#[test]
fn missing_target_metadata_still_restores_mode_and_ftst() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.data.remove("F0Tg");
    smc.data.insert("F0md".into(), byte(1));
    smc.data.insert("Ftst".into(), byte(1));
    let plan = fan::plan(&mut smc).unwrap();
    assert_eq!(plan.fans.len(), 1);
    assert_eq!(plan.fans[0].target_kind, None);
    let clock = MockClock::default();
    assert!(
        restore_all(&mut smc, &plan, &clock).is_err(),
        "incomplete cleanup must remain visible"
    );
    smc.automatic();
    assert!(smc.wrote("F0md", &byte(0)));
    assert!(smc.wrote("Ftst", &byte(0)));
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
}

#[test]
fn missing_fan_count_still_releases_ftst_and_reports_incomplete_restoration() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.data.remove("FNum");
    smc.data.insert("Ftst".into(), byte(1));
    let plan = fan::plan(&mut smc).unwrap();
    assert!(plan.fans.is_empty());
    assert!(plan.ftst);
    assert!(!plan.problems.is_empty());
    let clock = MockClock::default();
    assert!(restore_all(&mut smc, &plan, &clock).is_err());
    assert!(smc.wrote("Ftst", &byte(0)));
    assert_eq!(smc.number("Ftst"), 0.0);
}

#[test]
fn three_consecutive_failures_trigger_exit_and_healthy_sample_resets_window() {
    let mut window = FailureWindow::default();
    assert!(window.record(false).is_ok());
    assert!(window.record(false).is_ok());
    assert!(window.record(true).is_ok());
    assert!(window.record(false).is_ok());
    assert!(window.record(false).is_ok());
    assert!(window.record(false).is_err());
}

#[test]
fn sustained_temperature_failures_cause_session_cleanup() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    let plan = fan::plan(&mut smc).unwrap();
    let temperatures = Temperatures::discover(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().data.insert("Tp0P".into(), float(f32::NAN));
        let mut window = FailureWindow::default();
        for attempt in 0..3 {
            let healthy = temperatures
                .sample(session.smc())
                .iter()
                .all(|(_, sample)| sample.is_ok());
            assert!(!healthy);
            assert_eq!(window.record(healthy).is_err(), attempt == 2);
        }
    }
    smc.automatic();
}

#[test]
fn unwinding_a_panic_restores_automatic_mode() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        panic!("scripted foreground failure");
    }));
    assert!(outcome.is_err());
    smc.automatic();
}

#[test]
fn startup_mode_and_target_readback_mismatches_fail_then_restore() {
    for (key, value) in [("F0md", byte(1)), ("F0Tg", float(6000.0))] {
        let mut smc = MockSmc::fans(&[6000.0], true);
        smc.script(key, value, 1, Action::Ignore);
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            assert!(
                session
                    .engage_max()
                    .unwrap_err()
                    .to_string()
                    .contains("observation timed out")
            );
        }
        smc.automatic();
    }
}

#[test]
fn unlock_readback_mismatch_fails_and_releases_global_control() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail("F0md", byte(1), 1, refusal("F0md"), false);
    smc.script("Ftst", byte(1), 1, Action::Ignore);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(
            session
                .engage_max()
                .unwrap_err()
                .to_string()
                .contains("Ftst unlock readback mismatch")
        );
    }
    assert!(smc.wrote("Ftst", &byte(0)));
    smc.automatic();
}

#[test]
fn restoration_failure_still_releases_ftst_and_restores_other_fans() {
    let mut smc = MockSmc::fans(&[6000.0, 5500.0], true);
    for key in ["F0md", "F1md", "Ftst"] {
        smc.data.insert(key.into(), byte(1));
    }
    smc.fail(
        "F0md",
        byte(0),
        1,
        Error::Permission("mode restore denied".into()),
        false,
    );
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let error = restore_all(&mut smc, &plan, &clock).unwrap_err();
    assert!(error.to_string().contains("F0md"));
    assert_eq!(smc.number("F0md"), 1.0);
    assert_eq!(smc.number("F1md"), 0.0);
    assert_eq!(smc.number("Ftst"), 0.0);
    assert!(smc.wrote("Ftst", &byte(0)));
    assert!(
        !clock.settles.borrow().is_empty(),
        "restoration must settle independently of signals"
    );
}

#[test]
fn explicit_failed_restore_is_retried_by_drop() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().fail(
            "F0md",
            byte(0),
            3,
            Error::Io("temporary restore failure".into()),
            false,
        );
        assert!(session.restore().is_err());
        assert_eq!(session.smc().number("F0md"), 1.0);
    }
    smc.automatic();
    assert_eq!(
        smc.writes
            .iter()
            .filter(|(key, value)| key == "F0md" && value == &byte(0))
            .count(),
        4
    );
}

#[test]
fn read_only_discovery_status_and_sampling_issue_no_writes() {
    let mut smc = MockSmc::fans(&[6000.0, 5500.0], true);
    assert_eq!(fan::count(&mut smc).unwrap(), 2);
    let plan = fan::plan(&mut smc).unwrap();
    for spec in &plan.fans {
        fan::state(&mut smc, spec).unwrap();
    }
    let temperatures = Temperatures::discover(&mut smc).unwrap();
    for _ in 0..3 {
        assert!(
            temperatures
                .sample(&mut smc)
                .iter()
                .all(|(_, sample)| sample.is_ok())
        );
    }
    assert!(smc.writes.is_empty());
}

#[test]
fn interrupt_before_first_write_leaves_hardware_untouched() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::interrupt_at(Duration::ZERO);
    assert_eq!(
        Session::new(&mut smc, plan, &clock).engage_max(),
        Err(Error::Interrupted)
    );
    assert!(smc.writes.is_empty());
}

#[test]
fn interrupt_during_unlock_wait_restores_mode_and_ftst() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.fail("F0md", byte(1), 1, refusal("F0md"), false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::interrupt_at(Duration::from_secs(3));
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert_eq!(session.engage_max(), Err(Error::Interrupted));
        assert_eq!(session.smc().number("Ftst"), 1.0);
    }
    smc.automatic();
}

#[test]
fn lowercase_and_uppercase_mode_keys_are_discovered_and_restored() {
    for mode_key in ["F0md", "F0Md"] {
        let mut smc = MockSmc::fans(&[6000.0], false);
        smc.data.remove("F0md");
        smc.data.insert(mode_key.into(), byte(3));
        let plan = fan::plan(&mut smc).unwrap();
        assert_eq!(plan.fans[0].mode_key, mode_key);
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            session.engage_max().unwrap();
            session.restore().unwrap();
        }
        assert!(smc.wrote(mode_key, &byte(1)));
        assert!(smc.wrote(mode_key, &byte(0)));
        smc.automatic();
    }
}

#[test]
fn active_manual_or_global_control_prevents_new_session_writes() {
    for active_key in ["F0md", "Ftst"] {
        let mut smc = MockSmc::fans(&[6000.0], true);
        smc.data.insert(active_key.into(), byte(1));
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        assert!(Session::new(&mut smc, plan, &clock).engage_max().is_err());
        assert!(smc.writes.is_empty());
    }
}

#[test]
fn hold_detects_changed_mode_target_limit_and_failed_spinup() {
    for (key, changed, check_spinup) in [
        ("F0md", byte(0), false),
        ("F0Tg", float(5000.0), false),
        ("F0Mx", float(6100.0), false),
        ("F0Ac", float(4000.0), true),
    ] {
        let mut smc = MockSmc::fans(&[6000.0], true);
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            session.engage_max().unwrap();
            session.check_hold(false).unwrap();
            session.smc().data.insert(key.into(), changed);
            assert!(matches!(
                session.check_hold(check_spinup),
                Err(Error::Control(_))
            ));
        }
        smc.automatic();
    }
}

#[test]
fn successful_write_without_restoration_readback_is_reported() {
    let mut smc = MockSmc::fans(&[6000.0], true);
    smc.data.insert("F0md".into(), byte(1));
    smc.script("F0md", byte(0), 1, Action::Ignore);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let error = restore_all(&mut smc, &plan, &clock).unwrap_err();
    assert!(matches!(error, Error::Control(_)));
    assert!(error.to_string().contains("restore_observation timed out"));
    assert_eq!(smc.number("Ftst"), 0.0);
}

#[test]
fn delayed_mode_requires_three_stable_samples_before_any_target_write() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write(
        "F0md",
        byte(1),
        "F0md",
        [0, 0, 1, 1, 1].into_iter().map(|n| Ok(byte(n))).collect(),
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        let mode_reads: Vec<u32> = session
            .smc()
            .reads_until_next_write("F0md", &byte(1))
            .iter()
            .filter(|(key, _)| *key == "F0md")
            .map(|(_, result)| result.as_ref().unwrap().integer().unwrap())
            .collect();
        // Five observation samples, then the immediate pre-target mode check.
        assert_eq!(mode_reads, vec![0, 0, 1, 1, 1, 1]);
        assert_eq!(clock.now(), Duration::from_millis(600));
        assert_eq!(session.smc().writes.len(), 2);
        session.finish().unwrap();
    }
    smc.automatic();
    smc.assert_target_writes_in_bounds();
}

#[test]
fn delayed_target_converges_within_window_without_rewriting() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write(
        "F0Tg",
        float(7000.0),
        "F0Tg",
        [1600.0, 1600.0, 7000.0, 7000.0, 7000.0]
            .into_iter()
            .map(|n| Ok(float(n)))
            .collect(),
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        let target_reads: Vec<f32> = session
            .smc()
            .reads_until_next_write("F0Tg", &float(7000.0))
            .iter()
            .filter(|(key, _)| *key == "F0Tg")
            .map(|(_, result)| result.as_ref().unwrap().number().unwrap())
            .collect();
        assert_eq!(target_reads, vec![1600.0, 1600.0, 7000.0, 7000.0, 7000.0]);
        assert_eq!(clock.now(), Duration::from_millis(600));
        assert_eq!(session.smc().writes.len(), 2);
        session.finish().unwrap();
    }
    smc.automatic();
    smc.assert_target_writes_in_bounds();
}

#[test]
fn never_settling_mode_and_target_have_a_two_second_twenty_one_sample_bound() {
    for (key, value, stage, total_duration) in [
        ("F0md", byte(1), "mode_observation", Duration::from_secs(2)),
        (
            "F0Tg",
            float(6000.0),
            "target_observation",
            Duration::from_millis(2200),
        ),
    ] {
        let mut smc = MockSmc::fans(&[6000.0], false);
        let plan = fan::plan(&mut smc).unwrap();
        smc.script(key, value.clone(), 1, Action::Ignore);
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            let error = session.engage_max().unwrap_err();
            assert!(matches!(error, Error::Control(_)));
            assert!(error.to_string().contains(&format!("{stage} timed out")));
            assert_eq!(clock.now(), total_duration);
            let reads = session.smc().reads_until_next_write(key, &value);
            assert_eq!(reads.len(), 21 * 3);
            for sample in reads.chunks_exact(3) {
                assert_eq!(
                    sample.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
                    ["F0md", "F0Tg", "F0Ac"]
                );
            }
            assert_eq!(
                session
                    .smc()
                    .writes
                    .iter()
                    .filter(|(k, v)| k == key && *v == value)
                    .count(),
                1
            );
            session.finish().unwrap();
        }
        smc.automatic();
        smc.assert_target_writes_in_bounds();
    }
}

#[test]
fn target_observed_then_overwritten_fails_immediately_and_does_not_reassert() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write(
        "F0Tg",
        float(6000.0),
        "F0Tg",
        vec![Ok(float(6000.0)), Ok(float(5000.0))],
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("target changed after observing requested RPM")
        );
        assert_eq!(clock.now(), Duration::from_millis(300));
        assert_eq!(
            session
                .smc()
                .reads_until_next_write("F0Tg", &float(6000.0))
                .len(),
            6
        );
        assert_eq!(session.smc().writes.len(), 2);
        session.finish().unwrap();
    }
    assert_eq!(
        smc.writes.iter().filter(|(key, _)| key == "F0Tg").count(),
        1
    );
    smc.automatic();
}

#[test]
fn mode_reverted_during_initial_observation_is_detected_before_target_write() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write("F0md", byte(1), "F0md", vec![Ok(byte(1)), Ok(byte(0))]);
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("mode changed after observing manual mode")
        );
        assert_eq!(clock.now(), Duration::from_millis(100));
        assert_eq!(
            session.smc().reads_until_next_write("F0md", &byte(1)).len(),
            6
        );
        session.finish().unwrap();
    }
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
    assert_eq!(
        smc.writes
            .iter()
            .filter(|(key, value)| key == "F0md" && value == &byte(1))
            .count(),
        1
    );
    smc.automatic();
}

#[test]
fn mode_stolen_after_three_samples_prevents_target_write() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write(
        "F0md",
        byte(1),
        "F0md",
        [1, 1, 1, 0].into_iter().map(|n| Ok(byte(n))).collect(),
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("manual mode lost before target write")
        );
        assert_eq!(clock.now(), Duration::from_millis(200));
        session.finish().unwrap();
    }
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
    smc.automatic();
}

#[test]
fn mode_stolen_during_target_observation_reads_target_and_actual_then_recovers() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write("F0Tg", float(6000.0), "F0md", vec![Ok(byte(0))]);
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("manual mode lost during target verification")
        );
        let reads = session.smc().reads_until_next_write("F0Tg", &float(6000.0));
        assert_eq!(
            reads.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            ["F0md", "F0Tg", "F0Ac"]
        );
        assert_eq!(clock.now(), Duration::from_millis(200));
        session.finish().unwrap();
    }
    assert_eq!(smc.writes.len(), 3);
    smc.automatic();
}

#[test]
fn hold_control_loss_is_immediate_and_never_reasserts_manual_or_target() {
    for (key, changed, message) in [
        ("F0md", byte(0), "previously verified manual mode lost"),
        (
            "F0Tg",
            float(5000.0),
            "previously verified target overwritten",
        ),
    ] {
        let mut smc = MockSmc::fans(&[6000.0], false);
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            session.engage_max().unwrap();
            let before = clock.now();
            let writes_before = session.smc().writes.len();
            session.smc().data.insert(key.into(), changed);
            let reads_before = session.smc().reads.len();
            let error = session.check_hold(false).unwrap_err();
            assert!(matches!(error, Error::Control(_)));
            assert!(error.to_string().contains(message));
            assert_eq!(
                &session.smc().reads[reads_before..],
                ["F0md", "F0Tg", "F0Ac"]
            );
            assert_eq!(clock.now(), before);
            assert_eq!(session.smc().writes.len(), writes_before);
            session.finish().unwrap();
        }
        assert_eq!(smc.writes.len(), 3);
        smc.automatic();
        smc.assert_target_writes_in_bounds();
    }
}

#[test]
fn target_permission_failure_after_verified_mode_restores_automatic() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.fail(
        "F0Tg",
        float(6000.0),
        1,
        Error::Permission("target write denied".into()),
        false,
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(matches!(session.engage_max(), Err(Error::Permission(_))));
        assert_eq!(session.smc().number("F0md"), 1.0);
        assert_eq!(session.smc().number("F0Tg"), 0.0);
        assert_eq!(clock.now(), Duration::from_millis(200));
        session.finish().unwrap();
    }
    assert_eq!(smc.writes.len(), 3);
    smc.automatic();
}

#[test]
fn interrupt_during_observation_runs_recovery_despite_interrupted_clock() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::interrupt_at(Duration::from_millis(100));
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert_eq!(session.engage_max(), Err(Error::Interrupted));
        assert_eq!(session.smc().number("F0md"), 1.0);
        session.finish().unwrap();
    }
    assert_eq!(
        clock.settles.borrow().as_slice(),
        [Duration::from_millis(100); 2]
    );
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
    smc.automatic();
}

#[test]
fn automatic_restoration_waits_for_three_stable_samples_after_delayed_mode_change() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().read_after_write(
            "F0md",
            byte(0),
            "F0md",
            [1, 1, 0, 0, 0].into_iter().map(|n| Ok(byte(n))).collect(),
        );
        let before = clock.now();
        session.finish().unwrap();
        assert_eq!(clock.now() - before, Duration::from_millis(400));
        let reads = session.smc().reads_until_next_write("F0md", &byte(0));
        assert_eq!(reads.len(), 5 * 3);
    }
    assert_eq!(smc.writes.len(), 3);
    smc.automatic();
    smc.assert_target_writes_in_bounds();
}

#[test]
fn target_tolerance_remains_a_quarter_rpm() {
    for (readback, accepted) in [(5999.75, true), (6000.25, true), (5999.5, false)] {
        let mut smc = MockSmc::fans(&[6000.0], false);
        let plan = fan::plan(&mut smc).unwrap();
        smc.read_after_write("F0Tg", float(6000.0), "F0Tg", vec![Ok(float(readback))]);
        let clock = MockClock::default();
        {
            let mut session = Session::new(&mut smc, plan, &clock);
            let result = session.engage_max();
            assert_eq!(result.is_ok(), accepted, "target {readback}");
            if !accepted {
                assert!(matches!(result, Err(Error::Control(_))));
            }
            session.finish().unwrap();
        }
        smc.automatic();
        smc.assert_target_writes_in_bounds();
    }
}

#[test]
fn telemetry_error_does_not_skip_other_observation_reads_or_claim_control_loss() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        session.smc().data.remove("F0md");
        let reads_before = session.smc().reads.len();
        assert!(matches!(session.check_hold(false), Err(Error::Missing(_))));
        assert_eq!(
            &session.smc().reads[reads_before..],
            ["F0md", "F0Tg", "F0Ac"]
        );
        session.smc().data.insert("F0md".into(), byte(1));
        session.finish().unwrap();
    }
    smc.automatic();
}

#[test]
fn unexpected_read_error_during_settling_is_bounded_and_recovery_still_runs() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.read_after_write(
        "F0md",
        byte(1),
        "F0md",
        vec![
            Err(Error::Io("temporary read failure".into())),
            Ok(byte(1)),
            Ok(byte(1)),
            Ok(byte(1)),
        ],
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_max().unwrap();
        let reads = session.smc().reads_until_next_write("F0md", &byte(1));
        assert!(reads[0].1.is_err());
        assert_eq!(reads[1].0, "F0Tg");
        assert_eq!(reads[2].0, "F0Ac");
        assert_eq!(clock.now(), Duration::from_millis(500));
        session.finish().unwrap();
    }
    smc.automatic();
}

#[test]
fn a_rpm_read_failure_does_not_erase_observed_manual_mode_before_takeover() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.script(
        "F0md",
        byte(1),
        1,
        Action::ReadSequences(vec![
            ("F0md".into(), vec![Ok(byte(1)), Ok(byte(0))]),
            (
                "F0Ac".into(),
                vec![Err(Error::Io("tachometer read unavailable".into()))],
            ),
        ]),
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("mode changed after observing manual mode")
        );
        assert_eq!(clock.now(), Duration::from_millis(100));
        let reads = session.smc().reads_until_next_write("F0md", &byte(1));
        assert_eq!(reads.len(), 6);
        assert!(reads[2].1.is_err());
        assert_eq!(reads[3].1.as_ref().unwrap().integer().unwrap(), 0);
        session.finish().unwrap();
    }
    assert!(!smc.writes.iter().any(|(key, _)| key == "F0Tg"));
    smc.automatic();
}

#[test]
fn a_rpm_read_failure_does_not_erase_observed_target_before_overwrite() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.script(
        "F0Tg",
        float(6000.0),
        1,
        Action::ReadSequences(vec![
            ("F0Tg".into(), vec![Ok(float(6000.0)), Ok(float(5000.0))]),
            (
                "F0Ac".into(),
                vec![Err(Error::Io("tachometer read unavailable".into()))],
            ),
        ]),
    );
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        let error = session.engage_max().unwrap_err();
        assert!(matches!(error, Error::Control(_)));
        assert!(
            error
                .to_string()
                .contains("target changed after observing requested RPM")
        );
        assert_eq!(clock.now(), Duration::from_millis(300));
        let reads = session.smc().reads_until_next_write("F0Tg", &float(6000.0));
        assert_eq!(reads.len(), 6);
        assert!(reads[2].1.is_err());
        assert_eq!(reads[4].1.as_ref().unwrap().number().unwrap(), 5000.0);
        session.finish().unwrap();
    }
    assert_eq!(
        smc.writes.iter().filter(|(key, _)| key == "F0Tg").count(),
        1
    );
    smc.automatic();
}

#[test]
fn an_ambiguous_target_write_failure_is_attempted_once_then_restored() {
    for error in [
        Error::Io("write transport status unknown".into()),
        Error::Smc {
            key: "F0Tg".into(),
            code: 0x85,
        },
    ] {
        for mutate_first in [false, true] {
            let mut smc = MockSmc::fans(&[6000.0], false);
            let plan = fan::plan(&mut smc).unwrap();
            // Permit further failed attempts in the script so an accidental retry
            // remains observable instead of becoming a mock success.
            smc.fail("F0Tg", float(6000.0), 5, error.clone(), mutate_first);
            let clock = MockClock::default();
            {
                let mut session = Session::new(&mut smc, plan, &clock);
                assert_eq!(session.engage_max(), Err(error.clone()));
                assert!(session.recovery_required());
                assert_eq!(clock.now(), Duration::from_millis(200));
                assert_eq!(session.smc().number("F0md"), 1.0);
                assert_eq!(
                    session.smc().number("F0Tg"),
                    if mutate_first { 6000.0 } else { 0.0 }
                );
                assert_eq!(
                    session
                        .smc()
                        .writes
                        .iter()
                        .filter(|(key, _)| key == "F0Tg")
                        .count(),
                    1
                );
                session.finish().unwrap();
                assert!(!session.recovery_required());
            }
            assert_eq!(smc.writes.len(), 3);
            smc.automatic();
            smc.assert_target_writes_in_bounds();
        }
    }
}

#[test]
fn recovery_responsibility_distinguishes_preflight_failure_and_possibly_mutating_write() {
    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.data.insert("F0Mx".into(), float(0.0));
    let clock = MockClock::default();
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(!session.recovery_required());
        assert!(matches!(session.engage_max(), Err(Error::Invalid(_))));
        assert!(!session.recovery_required());
        assert!(session.smc().writes.is_empty());
    }
    assert!(smc.writes.is_empty());

    let mut smc = MockSmc::fans(&[6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    smc.fail(
        "F0md",
        byte(1),
        1,
        Error::Io("mode write status unknown".into()),
        true,
    );
    {
        let mut session = Session::new(&mut smc, plan, &clock);
        assert!(!session.recovery_required());
        assert!(matches!(session.engage_max(), Err(Error::Io(_))));
        assert!(session.recovery_required());
        assert_eq!(session.smc().number("F0md"), 1.0);
        session.finish().unwrap();
        assert!(!session.recovery_required());
    }
    smc.automatic();
}

#[test]
fn custom_targets_hold_update_and_skip_unchanged_targets_without_mode_rewrites() {
    let mut smc = MockSmc::fans(&[7000.0, 6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0, 3500.0]).unwrap();
    session.check_hold(false).unwrap();
    let initial_writes = session.smc().writes.len();
    session.update_targets(&[3000.0, 3500.0]).unwrap();
    assert_eq!(session.smc().writes.len(), initial_writes);
    session.update_targets(&[4200.0, 3500.0]).unwrap();
    assert_eq!(
        &session.smc().writes[initial_writes..],
        &[("F0Tg".into(), float(4200.0))]
    );
    session.check_hold(false).unwrap();
    session.finish().unwrap();
    session.smc().automatic();
}

#[test]
fn every_custom_target_is_preflighted_before_any_initial_write() {
    for targets in [
        vec![3000.0],
        vec![3000.0, 6001.0],
        vec![3000.0, 1000.0],
        vec![3000.0, f32::NAN],
    ] {
        let mut smc = MockSmc::fans(&[7000.0, 6000.0], false);
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        assert!(
            Session::new(&mut smc, plan, &clock)
                .engage_targets(&targets)
                .is_err()
        );
        assert!(smc.writes.is_empty());
    }
}

#[test]
fn update_preflight_checks_all_targets_before_first_write_and_preserves_recovery() {
    let mut smc = MockSmc::fans(&[7000.0, 6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0, 3000.0]).unwrap();
    let before = session.smc().writes.len();
    assert!(session.update_targets(&[4000.0, 6001.0]).is_err());
    assert_eq!(session.smc().writes.len(), before);
    assert!(session.recovery_required());
    session.finish().unwrap();
}

#[test]
fn update_checks_other_fans_ownership_before_first_target_write() {
    let mut smc = MockSmc::fans(&[7000.0, 6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0, 3000.0]).unwrap();
    session.smc().data.insert("F1md".into(), byte(0));
    let before = session.smc().writes.len();
    assert!(matches!(
        session.update_targets(&[4000.0, 4000.0]),
        Err(Error::Control(_))
    ));
    assert_eq!(session.smc().writes.len(), before);
    session.finish().unwrap();
}

#[test]
fn updated_target_can_have_bounded_old_readback_without_extra_writes() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0]).unwrap();
    session.smc().read_after_write(
        "F0Tg",
        float(4200.0),
        "F0Tg",
        vec![
            Ok(float(3000.0)),
            Ok(float(3000.0)),
            Ok(float(4200.0)),
            Ok(float(4200.0)),
            Ok(float(4200.0)),
        ],
    );
    let before = session.smc().writes.len();
    session.update_targets(&[4200.0]).unwrap();
    assert_eq!(session.smc().writes.len(), before + 1);
    session.check_hold(false).unwrap();
    session.finish().unwrap();
}

#[test]
fn overwritten_update_fails_immediately_and_restores_without_reasserting_target() {
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0]).unwrap();
    session.smc().read_after_write(
        "F0Tg",
        float(4200.0),
        "F0Tg",
        vec![Ok(float(4200.0)), Ok(float(3000.0))],
    );
    assert!(matches!(
        session.update_targets(&[4200.0]),
        Err(Error::Control(_))
    ));
    assert!(session.recovery_required());
    session.finish().unwrap();
    session.smc().automatic();
    assert_eq!(
        session
            .smc()
            .writes
            .iter()
            .filter(|(key, value)| key == "F0Tg" && *value == float(4200.0))
            .count(),
        1
    );
}

#[test]
fn second_fan_update_failure_after_mutation_restores_both_fans() {
    let mut smc = MockSmc::fans(&[7000.0, 6000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let clock = MockClock::default();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0, 3000.0]).unwrap();
    session.smc().fail(
        "F1Tg",
        float(4000.0),
        1,
        Error::Io("ambiguous transport failure".into()),
        true,
    );
    assert!(session.update_targets(&[4200.0, 4000.0]).is_err());
    assert_eq!(session.smc().number("F0Tg"), 4200.0);
    assert_eq!(session.smc().number("F1Tg"), 4000.0);
    assert!(session.recovery_required());
    session.finish().unwrap();
    session.smc().automatic();
}

#[test]
fn range_change_or_target_loss_prevents_update_and_retains_cleanup() {
    for (key, value) in [
        ("F0Mn", float(1250.0)),
        ("F0Mx", float(6800.0)),
        ("F0Tg", float(3100.0)),
    ] {
        let mut smc = MockSmc::fans(&[7000.0], false);
        let plan = fan::plan(&mut smc).unwrap();
        let clock = MockClock::default();
        let mut session = Session::new(&mut smc, plan, &clock);
        session.engage_targets(&[3000.0]).unwrap();
        session.smc().data.insert(key.into(), value);
        let before = session.smc().writes.len();
        assert!(matches!(
            session.update_targets(&[4200.0]),
            Err(Error::Control(_))
        ));
        assert_eq!(session.smc().writes.len(), before);
        session.finish().unwrap();
    }
}

#[test]
fn interruption_during_update_observation_retains_automatic_restore() {
    struct InterruptClock {
        elapsed: Cell<Duration>,
        stop: Cell<bool>,
    }
    impl Clock for InterruptClock {
        fn wait(&self, duration: Duration) -> bool {
            self.elapsed.set(self.elapsed.get() + duration);
            !(self.stop.get() && !duration.is_zero())
        }
        fn settle(&self, duration: Duration) {
            self.elapsed.set(self.elapsed.get() + duration);
        }
        fn now(&self) -> Duration {
            self.elapsed.get()
        }
    }
    let clock = InterruptClock {
        elapsed: Cell::new(Duration::ZERO),
        stop: Cell::new(false),
    };
    let mut smc = MockSmc::fans(&[7000.0], false);
    let plan = fan::plan(&mut smc).unwrap();
    let mut session = Session::new(&mut smc, plan, &clock);
    session.engage_targets(&[3000.0]).unwrap();
    clock.stop.set(true);
    assert_eq!(session.update_targets(&[4200.0]), Err(Error::Interrupted));
    assert_eq!(session.smc().number("F0Tg"), 4200.0);
    assert!(session.recovery_required());
    session.finish().unwrap();
    session.smc().automatic();
}
