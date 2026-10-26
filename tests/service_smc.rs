//! Intent checks and write payloads use memory-only mocks; no AppleSMC access.
use m1fan::service_smc::IntentSmc;
use m1fan::smc::{Error, ReadSmc, Result, Value, WriteSmc};
use m1fan::trace::Trace;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Default)]
struct Calls {
    reads: Vec<String>,
    keys: usize,
    traces: usize,
    writes: Vec<(String, Value)>,
}

#[derive(Default)]
struct MockSmc {
    calls: Rc<RefCell<Calls>>,
    write_error: Option<Error>,
}

impl ReadSmc for MockSmc {
    fn read(&mut self, key: &str) -> Result<Value> {
        self.calls.borrow_mut().reads.push(key.into());
        Ok(byte(0))
    }
    fn keys(&mut self) -> Result<Vec<String>> {
        self.calls.borrow_mut().keys += 1;
        Ok(vec!["F0Md".into()])
    }
    fn trace(&self) -> Trace {
        self.calls.borrow_mut().traces += 1;
        Trace::disabled()
    }
}
impl WriteSmc for MockSmc {
    fn write(&mut self, key: &str, value: &Value) -> Result<()> {
        self.calls
            .borrow_mut()
            .writes
            .push((key.into(), value.clone()));
        self.write_error.clone().map_or(Ok(()), Err)
    }
}

fn byte(value: u8) -> Value {
    Value {
        kind: *b"ui8 ",
        bytes: vec![value],
    }
}
fn target(value: f32) -> Value {
    Value {
        kind: *b"flt ",
        bytes: value.to_le_bytes().to_vec(),
    }
}

#[test]
fn disabled_intent_does_not_block_read_keys_or_trace() {
    let mock = MockSmc::default();
    let calls = Rc::clone(&mock.calls);
    let mut smc = IntentSmc::new(mock, || panic!("read-only access must not check intent"));
    assert_eq!(smc.read("F0Md").unwrap(), byte(0));
    assert_eq!(smc.keys().unwrap(), vec!["F0Md"]);
    let _ = smc.trace();
    let calls = calls.borrow();
    assert_eq!(calls.reads, vec!["F0Md"]);
    assert_eq!(calls.keys, 1);
    assert_eq!(calls.traces, 1);
    assert!(calls.writes.is_empty());
}

#[test]
fn disabling_between_mode_and_target_blocks_target_and_allows_restoration() {
    let mock = MockSmc::default();
    let calls = Rc::clone(&mock.calls);
    let enabled = Cell::new(true);
    let checks = Cell::new(0);
    let mut smc = IntentSmc::new(mock, || {
        checks.set(checks.get() + 1);
        Ok(enabled.get())
    });
    smc.write("F0Md", &byte(1)).unwrap();
    enabled.set(false);
    let error = smc.write("F0Tg", &target(7000.0)).unwrap_err();
    assert!(error.to_string().contains("disabled"));
    assert_eq!(calls.borrow().writes.len(), 1);
    smc.write("F0Md", &byte(0)).unwrap();
    smc.write("Ftst", &byte(0)).unwrap();
    assert_eq!(checks.get(), 2);
    assert_eq!(
        calls.borrow().writes,
        vec![
            ("F0Md".into(), byte(1)),
            ("F0Md".into(), byte(0)),
            ("Ftst".into(), byte(0))
        ]
    );
}

#[test]
fn missing_marker_blocks_normal_write_without_blocking_automatic_release() {
    let mock = MockSmc::default();
    let calls = Rc::clone(&mock.calls);
    let mut smc = IntentSmc::new(mock, || Err(Error::Missing("service.enabled".into())));
    for (key, value) in [
        ("F0Md", byte(1)),
        ("F0Tg", target(7000.0)),
        ("Ftst", byte(1)),
    ] {
        let error = smc.write(key, &value).unwrap_err();
        assert!(error.to_string().contains("service.enabled"));
    }
    assert!(calls.borrow().writes.is_empty());
    smc.write("F0Md", &byte(0)).unwrap();
    smc.write("Ftst", &byte(0)).unwrap();
    assert_eq!(calls.borrow().writes.len(), 2);
}

#[test]
fn only_exact_automatic_release_payloads_bypass_intent() {
    let mock = MockSmc::default();
    let calls = Rc::clone(&mock.calls);
    let mut smc = IntentSmc::new(mock, || Ok(false));
    for (key, value) in [
        ("F0Tg", target(0.0)),
        ("F0Tg", byte(0)),
        ("F0Md", byte(1)),
        ("F0Md", byte(3)),
        (
            "F0Md",
            Value {
                kind: *b"ui16",
                bytes: vec![0, 0],
            },
        ),
        (
            "F0Md",
            Value {
                kind: *b"ui8 ",
                bytes: vec![],
            },
        ),
        (
            "F0Md",
            Value {
                kind: *b"ui8 ",
                bytes: vec![0, 0],
            },
        ),
        ("F0md", byte(0)),
        ("F00Md", byte(0)),
        ("FGMd", byte(0)),
        ("F0Ac", byte(0)),
        ("ftst", byte(0)),
        ("Ftst", byte(1)),
    ] {
        assert!(
            smc.write(key, &value).is_err(),
            "unexpected intent bypass for {key} {value:?}"
        );
    }
    assert!(calls.borrow().writes.is_empty());
    for key in ["F0Md", "F9Md", "FAMd", "FfMd", "Ftst"] {
        smc.write(key, &byte(0)).unwrap();
    }
    assert_eq!(calls.borrow().writes.len(), 5);
}

#[test]
fn underlying_transport_rejection_is_preserved_for_restore_and_normal_write() {
    let error = Error::Smc {
        key: "F0Md".into(),
        code: 0x82,
    };
    let mock = MockSmc {
        write_error: Some(error.clone()),
        ..Default::default()
    };
    let calls = Rc::clone(&mock.calls);
    let mut smc = IntentSmc::new(mock, || Ok(true));
    assert_eq!(smc.write("F0Md", &byte(0)), Err(error.clone()));
    assert_eq!(smc.write("F0Md", &byte(1)), Err(error));
    assert_eq!(calls.borrow().writes.len(), 2);
}

#[test]
fn enabled_check_occurs_before_each_normal_write() {
    let mock = MockSmc::default();
    let calls = Rc::clone(&mock.calls);
    let checks = Cell::new(0);
    let mut smc = IntentSmc::new(mock, || {
        assert_eq!(checks.get(), calls.borrow().writes.len());
        checks.set(checks.get() + 1);
        Ok(true)
    });
    smc.write("F0Md", &byte(1)).unwrap();
    smc.write("F0Tg", &target(5000.0)).unwrap();
    smc.write("F0Tg", &target(7000.0)).unwrap();
    assert_eq!(checks.get(), 3);
}
