//! Optional JSONL diagnostics. Logging is best effort so a failed log device can
//! never suppress an automatic-restoration write. The controller checks status
//! at safe boundaries and reports a sticky logging failure separately.

use crate::smc::{Error, Result};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

#[derive(Clone, Default)]
pub struct Trace {
    inner: Option<Rc<RefCell<State>>>,
}

struct State {
    writer: Box<dyn Write>,
    start: Instant,
    failure: Option<String>,
}

impl Trace {
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Create a new private file; existing paths, including symlinks, are refused.
    pub fn create(path: &Path) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path).map_err(|error| {
            Error::Diagnostic(format!("create trace {}: {error}", path.display()))
        })?;
        Ok(Self::with_writer(Box::new(file)))
    }

    fn with_writer(writer: Box<dyn Write>) -> Self {
        Self {
            inner: Some(Rc::new(RefCell::new(State {
                writer,
                start: Instant::now(),
                failure: None,
            }))),
        }
    }

    pub fn event(&self, stage: &str, detail: &str) {
        if self.inner.is_some() {
            self.record(&format!(
                "\"event\":{},\"detail\":{}",
                quote(stage),
                quote(detail)
            ));
        }
    }

    pub fn status(&self) -> Result<()> {
        match &self.inner {
            Some(inner) => match &inner.borrow().failure {
                Some(error) => Err(Error::Diagnostic(error.clone())),
                None => Ok(()),
            },
            None => Ok(()),
        }
    }

    fn record(&self, fields: &str) {
        let Some(inner) = &self.inner else {
            return;
        };
        let mut state = inner.borrow_mut();
        if state.failure.is_some() {
            return;
        }
        let line = format!(
            "{{\"t_us\":{},{} }}\n",
            state.start.elapsed().as_micros(),
            fields
        );
        let result = state
            .writer
            .write_all(line.as_bytes())
            .and_then(|()| state.writer.flush());
        if let Err(error) = result {
            state.failure = Some(format!("trace write failed: {error}"));
        }
    }
}

fn quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{00}'..='\u{1f}' => {
                let _ = write!(output, "\\u{:04x}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

#[cfg(any(test, all(target_os = "macos", target_arch = "aarch64")))]
mod wire {
    use super::{Trace, quote};
    use crate::smc::Value;
    use std::fmt::Write as _;

    #[derive(Clone, Copy)]
    pub(crate) struct Metadata {
        pub size: u32,
        pub kind: [u8; 4],
    }

    pub(crate) struct WireOperation<'a> {
        pub label: &'a str,
        pub key: Option<&'a str>,
        pub key_id: u32,
        pub command: u8,
        pub index: Option<u32>,
        pub request_size: u32,
        pub metadata: Option<Metadata>,
        pub payload: &'a [u8],
    }

    pub(crate) struct WireReply<'a> {
        pub iokit: i32,
        pub length: usize,
        pub result: Option<u8>,
        pub status: Option<u8>,
        pub key_id: Option<u32>,
        pub metadata: Option<Metadata>,
        pub payload: Option<&'a [u8]>,
        pub error: Option<&'a str>,
    }

    fn hex(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(output, "{byte:02x}");
        }
        output
    }

    fn optional_number<T: ToString>(value: Option<T>) -> String {
        value.map_or_else(|| "null".into(), |value| value.to_string())
    }

    fn decode(metadata: Option<Metadata>, bytes: Option<&[u8]>) -> String {
        let Some((metadata, bytes)) = metadata.zip(bytes) else {
            return "null".into();
        };
        let Some(bytes) = bytes.get(..metadata.size as usize) else {
            return "null".into();
        };
        let value = Value {
            kind: metadata.kind,
            bytes: bytes.to_vec(),
        };
        if let Ok(integer) = value.integer() {
            return integer.to_string();
        }
        value
            .number()
            .map_or_else(|_| "null".into(), |n| n.to_string())
    }

    impl Trace {
        pub(crate) fn wire(&self, operation: &WireOperation<'_>, reply: Option<&WireReply<'_>>) {
            if self.inner.is_none() {
                return;
            }
            let metadata = if operation.command == 9 {
                reply
                    .filter(|r| r.result == Some(0))
                    .and_then(|r| r.metadata)
            } else {
                operation.metadata
            };
            let input_value = if operation.command == 6 {
                decode(metadata, Some(operation.payload))
            } else {
                "null".into()
            };
            let output_value = if operation.command == 5 {
                decode(
                    metadata,
                    reply
                        .filter(|r| r.result == Some(0))
                        .and_then(|r| r.payload),
                )
            } else {
                "null".into()
            };
            let fields = format!(
                concat!(
                    "\"event\":{},\"operation\":{},\"key\":{},\"request_key_id\":{},",
                    "\"command\":{},\"index\":{},\"request_size\":{},",
                    "\"data_type\":{},\"data_type_hex\":{},\"data_size\":{},",
                    "\"input_payload_hex\":{},\"input_decoded\":{},",
                    "\"returned_payload_hex\":{},\"returned_decoded\":{},",
                    "\"returned_key_id\":{},\"returned_info_size\":{},\"returned_info_type_hex\":{},",
                    "\"iokit\":{},\"iokit_hex\":{},\"smc_result\":{},\"smc_status\":{},",
                    "\"response_length\":{},\"success\":{},\"error\":{}"
                ),
                quote(if reply.is_some() {
                    "smc.end"
                } else {
                    "smc.begin"
                }),
                quote(operation.label),
                operation.key.map_or_else(|| "null".into(), quote),
                operation.key_id,
                operation.command,
                optional_number(operation.index),
                operation.request_size,
                metadata.map_or_else(
                    || "null".into(),
                    |m| quote(&String::from_utf8_lossy(&m.kind))
                ),
                metadata.map_or_else(|| "null".into(), |m| quote(&hex(&m.kind))),
                optional_number(metadata.map(|m| m.size)),
                quote(&hex(operation.payload)),
                input_value,
                reply
                    .and_then(|r| r.payload)
                    .map_or_else(|| "null".into(), |p| quote(&hex(p))),
                output_value,
                optional_number(reply.and_then(|r| r.key_id)),
                optional_number(reply.and_then(|r| r.metadata).map(|m| m.size)),
                reply
                    .and_then(|r| r.metadata)
                    .map_or_else(|| "null".into(), |m| quote(&hex(&m.kind))),
                optional_number(reply.map(|r| r.iokit)),
                reply.map_or_else(
                    || "null".into(),
                    |r| quote(&format!("{:#010x}", r.iokit as u32))
                ),
                optional_number(reply.and_then(|r| r.result)),
                optional_number(reply.and_then(|r| r.status)),
                optional_number(reply.map(|r| r.length)),
                reply.map_or_else(|| "null".into(), |r| r.error.is_none().to_string()),
                reply
                    .and_then(|r| r.error)
                    .map_or_else(|| "null".into(), quote),
            );
            self.record(&fields);
        }
    }
}

#[cfg(any(test, all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) use wire::{Metadata, WireOperation, WireReply};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
    struct Buffer(Rc<RefCell<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn buffer_trace() -> (Trace, Buffer) {
        let buffer = Buffer(Rc::new(RefCell::new(Vec::new())));
        (Trace::with_writer(Box::new(buffer.clone())), buffer)
    }

    fn text(buffer: &Buffer) -> String {
        String::from_utf8(buffer.0.borrow().clone()).unwrap()
    }

    #[test]
    fn escapes_json_and_uses_monotonic_microseconds() {
        assert_eq!(
            quote("\"\\\n\r\t\0\u{001f}温度"),
            "\"\\\"\\\\\\n\\r\\t\\u0000\\u001f温度\""
        );
        let (trace, buffer) = buffer_trace();
        trace.event("one\n", "quotes: \"here\"");
        trace.clone().event("two", "next");
        let log = text(&buffer);
        let times: Vec<u128> = log
            .lines()
            .map(|line| {
                line.strip_prefix("{\"t_us\":")
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert_eq!(times.len(), 2);
        assert!(times[1] >= times[0]);
        assert!(log.contains("\"event\":\"one\\n\""));
        assert!(log.contains(r#"quotes: \"here\""#));
        assert!(trace.status().is_ok());
    }

    #[test]
    fn file_creation_refuses_overwrite_and_symlinks() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "m1fan-trace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("trace.jsonl");
        let trace = Trace::create(&path).unwrap();
        trace.event("test", "preserve");
        let before = std::fs::read(&path).unwrap();
        assert!(Trace::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let link = directory.join("symlink.jsonl");
            symlink(&path, &link).unwrap();
            assert!(Trace::create(&link).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            std::fs::remove_file(link).unwrap();
        }
        drop(trace);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn writer_failure_is_sticky_and_never_panics_or_blocks_later_calls() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected full disk"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let trace = Trace::with_writer(Box::new(Broken));
        trace.event("write.begin", "F0Md");
        assert!(
            matches!(trace.status(), Err(Error::Diagnostic(message)) if message.contains("injected full disk"))
        );
        trace.clone().event("restore.begin", "F0Md");
        assert!(trace.status().is_err());
        assert!(Trace::disabled().status().is_ok());
    }

    #[test]
    fn failed_logger_cannot_block_verified_automatic_restoration() {
        use crate::control::{Clock, restore_all};
        use crate::fan::{FanSpec, Plan};
        use crate::smc::{ReadSmc, Value, WriteSmc, encode_rpm};
        use std::time::Duration;

        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("injected full disk before cleanup"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        struct NoWait;
        impl Clock for NoWait {
            fn wait(&self, _: Duration) -> bool {
                true
            }
            fn settle(&self, _: Duration) {}
            fn now(&self) -> Duration {
                Duration::ZERO
            }
        }
        struct Mock {
            trace: Trace,
            mode: u8,
            writes: Vec<(String, Value)>,
        }
        impl ReadSmc for Mock {
            fn read(&mut self, key: &str) -> Result<Value> {
                match key {
                    "F0Md" => Ok(Value {
                        kind: *b"ui8 ",
                        bytes: vec![self.mode],
                    }),
                    "F0Tg" | "F0Ac" => encode_rpm(*b"flt ", 7000.0),
                    _ => Err(Error::Missing(key.into())),
                }
            }
            fn keys(&mut self) -> Result<Vec<String>> {
                Ok(Vec::new())
            }
            fn trace(&self) -> Trace {
                self.trace.clone()
            }
        }
        impl WriteSmc for Mock {
            fn write(&mut self, key: &str, value: &Value) -> Result<()> {
                self.writes.push((key.into(), value.clone()));
                assert_eq!(key, "F0Md");
                assert_eq!(
                    value,
                    &Value {
                        kind: *b"ui8 ",
                        bytes: vec![0]
                    }
                );
                self.mode = 0;
                Ok(())
            }
        }
        let trace = Trace::with_writer(Box::new(Broken));
        trace.event("before_cleanup", "force logging failure");
        assert!(matches!(trace.status(), Err(Error::Diagnostic(_))));
        let mut smc = Mock {
            trace: trace.clone(),
            mode: 1,
            writes: Vec::new(),
        };
        assert_eq!(smc.read("F0Md").unwrap().integer(), Ok(1));
        let plan = Plan {
            fans: vec![FanSpec {
                index: 0,
                mode_key: "F0Md".into(),
                target_kind: Some(*b"flt "),
            }],
            ftst: false,
            problems: Vec::new(),
        };
        assert_eq!(restore_all(&mut smc, &plan, &NoWait), Ok(()));
        assert_eq!(smc.mode, 0);
        assert_eq!(smc.writes.len(), 1);
        assert!(matches!(trace.status(), Err(Error::Diagnostic(_))));
    }

    #[test]
    fn wire_records_exact_bytes_separate_write_input_and_read_output() {
        let (trace, buffer) = buffer_trace();
        let payload = 7000_f32.to_le_bytes();
        let operation = WireOperation {
            label: "F0Tg",
            key: Some("F0Tg"),
            key_id: 0x46305467,
            command: 6,
            index: None,
            request_size: 4,
            metadata: Some(Metadata {
                size: 4,
                kind: *b"flt ",
            }),
            payload: &payload,
        };
        trace.wire(&operation, None);
        let reply = WireReply {
            iokit: 0,
            length: 80,
            result: Some(0),
            status: Some(0),
            key_id: Some(0),
            metadata: Some(Metadata {
                size: 0,
                kind: [0; 4],
            }),
            payload: Some(&[0; 32]),
            error: None,
        };
        trace.wire(&operation, Some(&reply));
        let log = text(&buffer);
        assert!(log.contains("\"data_type_hex\":\"666c7420\""));
        assert!(log.contains("\"input_payload_hex\":\"00c0da45\""));
        assert!(log.contains("\"input_decoded\":7000"));
        assert!(log.contains("\"returned_decoded\":null"));
        assert!(log.contains("\"iokit_hex\":\"0x00000000\""));
        let read = WireOperation {
            command: 5,
            ..operation
        };
        let read_reply = WireReply {
            payload: Some(&payload),
            ..reply
        };
        trace.wire(&read, Some(&read_reply));
        let log = text(&buffer);
        assert!(
            log.lines()
                .last()
                .unwrap()
                .contains("\"input_decoded\":null")
        );
        assert!(
            log.lines()
                .last()
                .unwrap()
                .contains("\"returned_decoded\":7000")
        );
    }

    #[test]
    fn unavailable_response_fields_stay_null_and_integer_values_stay_exact() {
        let (trace, buffer) = buffer_trace();
        let operation = WireOperation {
            label: "#KEY",
            key: Some("#KEY"),
            key_id: 0x234b4559,
            command: 5,
            index: None,
            request_size: 4,
            metadata: Some(Metadata {
                size: 4,
                kind: *b"ui32",
            }),
            payload: &[0; 32],
        };
        let reply = WireReply {
            iokit: 0xE000_02C1_u32 as i32,
            length: 80,
            result: None,
            status: None,
            key_id: None,
            metadata: None,
            payload: None,
            error: Some("IOKit failed"),
        };
        trace.wire(&operation, Some(&reply));
        let log = text(&buffer);
        for field in [
            "smc_result",
            "smc_status",
            "returned_payload_hex",
            "returned_decoded",
            "returned_key_id",
            "returned_info_size",
            "returned_info_type_hex",
        ] {
            assert!(log.contains(&format!("\"{field}\":null")));
        }
        assert!(log.contains("\"iokit_hex\":\"0xe00002c1\""));
        assert!(log.contains("\"success\":false"));
        let payload = u32::MAX.to_be_bytes();
        let reply = WireReply {
            iokit: 0,
            result: Some(0),
            status: Some(0),
            payload: Some(&payload),
            error: None,
            ..reply
        };
        trace.wire(&operation, Some(&reply));
        assert!(
            text(&buffer)
                .lines()
                .last()
                .unwrap()
                .contains("\"returned_decoded\":4294967295")
        );
    }
}
