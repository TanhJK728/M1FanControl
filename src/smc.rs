//! Minimal AppleSMC transport and typed values.
//!
//! Transport, ABI representation and codecs are adapted from MacFanControl
//! (de9af6f), with Stats (ffdf637) protocol cross-checks. The synthetic fpe2
//! test vector also follows MacFanControl. See docs/PROVENANCE.md and
//! THIRD_PARTY_NOTICES.md. All request bytes, including padding, are initialized.

use crate::trace::Trace;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Missing(String),
    Permission(String),
    Invalid(String),
    Io(String),
    Control(String),
    Diagnostic(String),
    Smc { key: String, code: u8 },
    Interrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(message) => write!(f, "unavailable: {message}"),
            Self::Permission(message) => write!(f, "permission denied: {message}"),
            Self::Invalid(message) => write!(f, "invalid SMC data: {message}"),
            Self::Io(message) => write!(f, "SMC transport error: {message}"),
            Self::Control(message) => write!(f, "fan control error: {message}"),
            Self::Diagnostic(message) => write!(f, "diagnostic error: {message}"),
            Self::Smc { key, code } => write!(f, "SMC rejected {key}: result {code:#04x}"),
            Self::Interrupted => f.write_str("interrupted"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub kind: [u8; 4],
    pub bytes: Vec<u8>,
}

impl Value {
    fn exact<const N: usize>(&self) -> Result<[u8; N]> {
        self.bytes.as_slice().try_into().map_err(|_| {
            Error::Invalid(format!(
                "type {:?} requires {N} bytes; received {}",
                String::from_utf8_lossy(&self.kind),
                self.bytes.len()
            ))
        })
    }

    /// Decode only known, exact-size numeric encodings. Float payloads are LE;
    /// integer and fixed-point payloads are BE independently of the host ABI.
    pub fn number(&self) -> Result<f32> {
        let number = match &self.kind {
            b"flt " => f32::from_le_bytes(self.exact()?),
            b"fpe2" => f32::from(u16::from_be_bytes(self.exact()?)) / 4.0,
            b"sp78" => f32::from(i16::from_be_bytes(self.exact()?)) / 256.0,
            b"ui8 " | b"ui16" | b"ui32" => self.integer()? as f32,
            _ => return Err(self.unsupported_type()),
        };
        if !number.is_finite() {
            return Err(Error::Invalid("non-finite numeric payload".into()));
        }
        Ok(number)
    }

    pub fn integer(&self) -> Result<u32> {
        match &self.kind {
            b"ui8 " => Ok(u32::from(self.exact::<1>()?[0])),
            b"ui16" => Ok(u32::from(u16::from_be_bytes(self.exact()?))),
            b"ui32" => Ok(u32::from_be_bytes(self.exact()?)),
            _ => Err(self.unsupported_type()),
        }
    }

    fn unsupported_type(&self) -> Error {
        Error::Invalid(format!(
            "unsupported numeric type {:?}",
            String::from_utf8_lossy(&self.kind)
        ))
    }
}

pub fn encode_rpm(kind: [u8; 4], rpm: f32) -> Result<Value> {
    if !rpm.is_finite() || rpm < 0.0 {
        return Err(Error::Invalid(format!("invalid RPM {rpm}")));
    }
    let bytes = match &kind {
        b"flt " => rpm.to_le_bytes().to_vec(),
        b"fpe2" => {
            let scaled = rpm * 4.0;
            if scaled > f32::from(u16::MAX) || scaled.fract() != 0.0 {
                return Err(Error::Invalid(format!(
                    "RPM {rpm} cannot be represented exactly as fpe2"
                )));
            }
            (scaled as u16).to_be_bytes().to_vec()
        }
        _ => {
            return Err(Error::Invalid(format!(
                "unsupported RPM encoding {:?}",
                String::from_utf8_lossy(&kind)
            )));
        }
    };
    Ok(Value { kind, bytes })
}

pub trait ReadSmc {
    fn read(&mut self, key: &str) -> Result<Value>;
    fn keys(&mut self) -> Result<Vec<String>>;
    fn trace(&self) -> Trace {
        Trace::disabled()
    }
}

pub trait WriteSmc: ReadSmc {
    fn write(&mut self, key: &str, value: &Value) -> Result<()>;
}

/// Defense at the transport boundary: no general-purpose SMC write API.
/// The controller additionally verifies discovered fan indices and RPM bounds.
pub fn validate_write(key: &str, value: &Value) -> Result<()> {
    let name = key.as_bytes();
    let fan_key = name.len() == 4 && name[0] == b'F' && name[1].is_ascii_digit();
    let mode_key = key == "Ftst" || (fan_key && (&name[2..] == b"md" || &name[2..] == b"Md"));
    if mode_key && value.kind == *b"ui8 " && value.bytes.len() == 1 && value.bytes[0] <= 1 {
        return Ok(());
    }
    if fan_key && &name[2..] == b"Tg" && matches!(&value.kind, b"flt " | b"fpe2") {
        let rpm = value.number()?;
        if rpm >= 0.0 {
            return Ok(());
        }
    }
    Err(Error::Invalid(format!(
        "write to {key:?} rejected by the fan-control key/payload allowlist"
    )))
}

#[cfg(any(test, all(target_os = "macos", target_arch = "aarch64")))]
mod protocol {
    use super::{Error, Result};
    use crate::trace::{Metadata, WireReply};
    use std::mem::{offset_of, size_of};

    /// Layout of the AppleSMC selector-2 structure. Explicit padding ensures
    /// that Rust never passes uninitialized padding bytes to IOKit.
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct Packet {
        pub key: u32,
        pub version: [u8; 6],
        pub version_padding: [u8; 2],
        pub limits: [u32; 4],
        pub info_size: u32,
        pub info_type: u32,
        pub info_attributes: u8,
        pub info_padding: [u8; 3],
        pub result: u8,
        pub status: u8,
        pub command: u8,
        pub data_padding: u8,
        pub data: u32,
        pub bytes: [u8; 32],
    }

    const _: () = {
        assert!(size_of::<Packet>() == 80);
        assert!(offset_of!(Packet, version) == 4);
        assert!(offset_of!(Packet, limits) == 12);
        assert!(offset_of!(Packet, info_size) == 28);
        assert!(offset_of!(Packet, info_type) == 32);
        assert!(offset_of!(Packet, info_attributes) == 36);
        assert!(offset_of!(Packet, result) == 40);
        assert!(offset_of!(Packet, status) == 41);
        assert!(offset_of!(Packet, command) == 42);
        assert!(offset_of!(Packet, data) == 44);
        assert!(offset_of!(Packet, bytes) == 48);
    };

    pub(super) fn fourcc(key: &str) -> Result<u32> {
        let bytes: [u8; 4] = key
            .as_bytes()
            .try_into()
            .map_err(|_| Error::Invalid(format!("key {key:?} must contain four ASCII bytes")))?;
        if !bytes.iter().all(|b| (b' '..=b'~').contains(b)) {
            return Err(Error::Invalid(format!(
                "key {key:?} is not printable ASCII"
            )));
        }
        Ok(u32::from_be_bytes(bytes))
    }

    pub(super) fn response(key: &str, kern: i32, length: usize, packet: Packet) -> Result<Packet> {
        if kern == 0xE000_02C1_u32 as i32 || kern == 0xE000_02E2_u32 as i32 {
            return Err(Error::Permission(format!(
                "AppleSMC access for {key} (IOKit {:#010x})",
                kern as u32
            )));
        }
        if kern != 0 {
            return Err(Error::Io(format!(
                "{key}: IOKit returned {:#010x}",
                kern as u32
            )));
        }
        if length != size_of::<Packet>() {
            return Err(Error::Invalid(format!(
                "{key}: AppleSMC returned {length} bytes; expected {}",
                size_of::<Packet>()
            )));
        }
        match packet.result {
            0 => Ok(packet),
            0x84 => Err(Error::Missing(format!("SMC key {key}"))),
            code => Err(Error::Smc {
                key: key.into(),
                code,
            }),
        }
    }

    /// Only a successful, complete transport response contains trustworthy SMC
    /// fields. Failed or truncated calls leave those fields unknown in traces.
    pub(super) fn trace_reply<'a>(
        kern: i32,
        length: usize,
        packet: &'a Packet,
        error: Option<&'a str>,
    ) -> WireReply<'a> {
        let complete = kern == 0 && length == size_of::<Packet>();
        WireReply {
            iokit: kern,
            length,
            result: complete.then_some(packet.result),
            status: complete.then_some(packet.status),
            key_id: complete.then_some(packet.key),
            metadata: complete.then_some(Metadata {
                size: packet.info_size,
                kind: packet.info_type.to_be_bytes(),
            }),
            payload: complete.then_some(packet.bytes.as_slice()),
            error,
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native {
    use super::protocol::{Packet, fourcc, response, trace_reply};
    use super::{Error, ReadSmc, Result, Value, WriteSmc, validate_write};
    use crate::trace::{Metadata, Trace, WireOperation};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::mem::size_of;

    type MachPort = u32;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: MachPort, matching: *mut c_void) -> MachPort;
        fn IOServiceOpen(service: MachPort, task: MachPort, kind: u32, conn: *mut MachPort) -> i32;
        fn IOServiceClose(conn: MachPort) -> i32;
        fn IOObjectRelease(object: MachPort) -> i32;
        fn IOConnectCallStructMethod(
            conn: MachPort,
            selector: u32,
            input: *const c_void,
            input_len: usize,
            output: *mut c_void,
            output_len: *mut usize,
        ) -> i32;
    }

    unsafe extern "C" {
        static mach_task_self_: MachPort;
    }

    #[derive(Clone, Copy)]
    struct Info {
        size: u32,
        kind: [u8; 4],
    }

    pub struct NativeSmc {
        conn: MachPort,
        control: bool,
        metadata: HashMap<u32, Info>,
        trace: Trace,
    }

    impl NativeSmc {
        pub fn set_trace(&mut self, trace: Trace) {
            self.trace = trace;
        }

        pub fn open_read_only() -> Result<Self> {
            Self::open(false)
        }

        pub fn open_control() -> Result<Self> {
            if unsafe { libc::geteuid() } != 0 {
                return Err(Error::Permission("fan control requires sudo".into()));
            }
            Self::open(true)
        }

        fn open(control: bool) -> Result<Self> {
            // IOServiceGetMatchingService consumes the matching dictionary.
            let matching = unsafe { IOServiceMatching(c"AppleSMC".as_ptr()) };
            if matching.is_null() {
                return Err(Error::Missing("AppleSMC matching dictionary".into()));
            }
            let service = unsafe { IOServiceGetMatchingService(0, matching) };
            if service == 0 {
                return Err(Error::Missing("AppleSMC service".into()));
            }
            let mut conn = 0;
            let kern = unsafe { IOServiceOpen(service, mach_task_self_, 0, &mut conn) };
            // The service send right is released on both success and failure.
            unsafe { IOObjectRelease(service) };
            if kern != 0 {
                response(
                    "open AppleSMC",
                    kern,
                    size_of::<Packet>(),
                    Packet::default(),
                )?;
            }
            if conn == 0 {
                return Err(Error::Io("AppleSMC returned an invalid connection".into()));
            }
            Ok(Self {
                conn,
                control,
                metadata: HashMap::new(),
                trace: Trace::disabled(),
            })
        }

        fn call(&self, key: &str, input: &Packet) -> Result<Packet> {
            let operation = WireOperation {
                label: key,
                key: (input.command != 8).then_some(key),
                key_id: input.key,
                command: input.command,
                index: (input.command == 8).then_some(input.data),
                request_size: input.info_size,
                metadata: self.metadata.get(&input.key).map(|info| Metadata {
                    size: info.size,
                    kind: info.kind,
                }),
                payload: &input.bytes,
            };
            // This record is flushed before entering IOKit so an interrupted
            // write can still be identified. Log failure never prevents I/O.
            self.trace.wire(&operation, None);
            let mut output = Packet::default();
            let mut length = size_of::<Packet>();
            // Both structures have asserted layout and initialized padding;
            // IOKit receives pointers valid for the duration of this call.
            let kern = unsafe {
                IOConnectCallStructMethod(
                    self.conn,
                    2,
                    std::ptr::from_ref(input).cast(),
                    size_of::<Packet>(),
                    std::ptr::from_mut(&mut output).cast(),
                    &mut length,
                )
            };
            let result = response(key, kern, length, output);
            let error = result.as_ref().err().map(ToString::to_string);
            self.trace.wire(
                &operation,
                Some(&trace_reply(kern, length, &output, error.as_deref())),
            );
            result
        }

        fn info(&mut self, key: &str, id: u32) -> Result<Info> {
            if let Some(info) = self.metadata.get(&id) {
                return Ok(*info);
            }
            let output = self.call(
                key,
                &Packet {
                    key: id,
                    command: 9,
                    ..Packet::default()
                },
            )?;
            if !(1..=32).contains(&output.info_size) || output.info_type == 0 {
                return Err(Error::Invalid(format!(
                    "{key}: invalid metadata size {} / type {:#010x}",
                    output.info_size, output.info_type
                )));
            }
            let info = Info {
                size: output.info_size,
                kind: output.info_type.to_be_bytes(),
            };
            self.metadata.insert(id, info);
            Ok(info)
        }
    }

    impl ReadSmc for NativeSmc {
        fn trace(&self) -> Trace {
            self.trace.clone()
        }

        fn read(&mut self, key: &str) -> Result<Value> {
            let id = fourcc(key)?;
            let info = self.info(key, id)?;
            let result = self.call(
                key,
                &Packet {
                    key: id,
                    info_size: info.size,
                    command: 5,
                    ..Packet::default()
                },
            );
            match result {
                Ok(output) => Ok(Value {
                    kind: info.kind,
                    bytes: output.bytes[..info.size as usize].to_vec(),
                }),
                Err(error) => {
                    self.metadata.remove(&id);
                    Err(error)
                }
            }
        }

        fn keys(&mut self) -> Result<Vec<String>> {
            let count = self.read("#KEY")?.integer()?;
            if count == 0 || count > 65_536 {
                return Err(Error::Invalid(format!(
                    "#KEY count {count} outside 1..=65536"
                )));
            }
            let mut keys = Vec::with_capacity(count as usize);
            for index in 0..count {
                let output = self.call(
                    &format!("key index {index}"),
                    &Packet {
                        command: 8,
                        data: index,
                        ..Packet::default()
                    },
                )?;
                let key = String::from_utf8(output.key.to_be_bytes().to_vec())
                    .map_err(|_| Error::Invalid(format!("key index {index} is not ASCII")))?;
                fourcc(&key)?;
                keys.push(key);
            }
            Ok(keys)
        }
    }

    impl WriteSmc for NativeSmc {
        fn write(&mut self, key: &str, value: &Value) -> Result<()> {
            if !self.control {
                return Err(Error::Permission("SMC connection is read-only".into()));
            }
            if unsafe { libc::geteuid() } != 0 {
                return Err(Error::Permission("fan control requires sudo".into()));
            }
            validate_write(key, value)?;
            let id = fourcc(key)?;
            let info = self.info(key, id)?;
            if info.kind != value.kind || info.size as usize != value.bytes.len() {
                return Err(Error::Invalid(format!(
                    "{key}: write type/length does not match hardware metadata"
                )));
            }
            let mut input = Packet {
                key: id,
                info_size: info.size,
                command: 6,
                ..Packet::default()
            };
            input.bytes[..value.bytes.len()].copy_from_slice(&value.bytes);
            match self.call(key, &input) {
                Ok(_) => Ok(()),
                Err(error) => {
                    self.metadata.remove(&id);
                    Err(error)
                }
            }
        }
    }

    impl Drop for NativeSmc {
        fn drop(&mut self) {
            unsafe { IOServiceClose(self.conn) };
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use native::NativeSmc;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub struct NativeSmc;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl NativeSmc {
    pub fn set_trace(&mut self, _: Trace) {}

    pub fn open_read_only() -> Result<Self> {
        Err(Error::Io(
            "native SMC access requires Apple Silicon macOS".into(),
        ))
    }

    pub fn open_control() -> Result<Self> {
        Self::open_read_only()
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl ReadSmc for NativeSmc {
    fn read(&mut self, _: &str) -> Result<Value> {
        Err(Error::Io(
            "native SMC access requires Apple Silicon macOS".into(),
        ))
    }

    fn keys(&mut self) -> Result<Vec<String>> {
        Err(Error::Io(
            "native SMC access requires Apple Silicon macOS".into(),
        ))
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
impl WriteSmc for NativeSmc {
    fn write(&mut self, _: &str, _: &Value) -> Result<()> {
        Err(Error::Io(
            "native SMC access requires Apple Silicon macOS".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::protocol::{Packet, fourcc, response};
    use super::*;

    fn value(kind: &[u8; 4], bytes: &[u8]) -> Value {
        Value {
            kind: *kind,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn numeric_codecs_use_explicit_endianness() {
        assert_eq!(
            value(b"flt ", &2150.5_f32.to_le_bytes()).number(),
            Ok(2150.5)
        );
        assert_eq!(value(b"fpe2", &[0x13, 0x88]).number(), Ok(1250.0));
        assert_eq!(value(b"sp78", &[0xfe, 0x80]).number(), Ok(-1.5));
        assert_eq!(value(b"ui8 ", &[7]).integer(), Ok(7));
        assert_eq!(value(b"ui16", &[0x12, 0x34]).integer(), Ok(0x1234));
        assert_eq!(
            value(b"ui32", &[0x12, 0x34, 0x56, 0x78]).integer(),
            Ok(0x12345678)
        );
    }

    #[test]
    fn codecs_reject_wrong_lengths_and_nonfinite_data() {
        for (kind, length) in [
            (*b"flt ", 4),
            (*b"fpe2", 2),
            (*b"sp78", 2),
            (*b"ui8 ", 1),
            (*b"ui16", 2),
            (*b"ui32", 4),
        ] {
            for bad_length in [0, length - 1, length + 1, 32] {
                assert!(value(&kind, &vec![0; bad_length]).number().is_err());
            }
        }
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(value(b"flt ", &bad.to_le_bytes()).number().is_err());
        }
        assert!(value(b"flag", &[1]).number().is_err());
        assert!(value(b"flt ", &1_f32.to_le_bytes()).integer().is_err());
    }

    #[test]
    fn rpm_encoding_preserves_values_and_rejects_rounding() {
        for rpm in [0.0, 1200.25, 16383.75] {
            for kind in [*b"flt ", *b"fpe2"] {
                assert_eq!(encode_rpm(kind, rpm).unwrap().number(), Ok(rpm));
            }
        }
        for rpm in [-1.0, f32::NAN, f32::INFINITY] {
            assert!(encode_rpm(*b"flt ", rpm).is_err());
            assert!(encode_rpm(*b"fpe2", rpm).is_err());
        }
        for rpm in [1200.1, 16384.0, f32::MAX] {
            assert!(encode_rpm(*b"fpe2", rpm).is_err());
        }
        assert!(encode_rpm(*b"ui16", 1200.0).is_err());
    }

    #[test]
    fn allowlist_permits_only_narrow_fan_payloads() {
        for key in ["F0md", "F0Md", "F9md", "Ftst"] {
            for mode in [0, 1] {
                assert!(validate_write(key, &value(b"ui8 ", &[mode])).is_ok());
            }
            assert!(validate_write(key, &value(b"ui8 ", &[3])).is_err());
            assert!(validate_write(key, &value(b"ui8 ", &[0, 0])).is_err());
            assert!(validate_write(key, &value(b"ui16", &[0, 1])).is_err());
        }
        for key in ["F0Tg", "F9Tg"] {
            for kind in [*b"flt ", *b"fpe2"] {
                assert!(validate_write(key, &encode_rpm(kind, 5700.0).unwrap()).is_ok());
            }
            assert!(validate_write(key, &value(b"flt ", &(-1_f32).to_le_bytes())).is_err());
            assert!(validate_write(key, &value(b"flt ", &f32::NAN.to_le_bytes())).is_err());
            assert!(validate_write(key, &value(b"flt ", &[0])).is_err());
        }
        for key in [
            "FS! ", "F0Mx", "F0Ac", "TC0P", "FAMD", "FAMD", "F:Tg", "F10Tg", "F", "", "F0md\0",
        ] {
            assert!(validate_write(key, &value(b"ui8 ", &[1])).is_err());
            assert!(validate_write(key, &encode_rpm(*b"flt ", 5700.0).unwrap()).is_err());
        }
    }

    #[test]
    fn packet_layout_and_fourcc_match_wire_abi() {
        assert_eq!(std::mem::size_of::<Packet>(), 80);
        assert_eq!(std::mem::offset_of!(Packet, result), 40);
        assert_eq!(std::mem::offset_of!(Packet, bytes), 48);
        assert_eq!(fourcc("#KEY"), Ok(0x234b4559));
        assert_eq!(fourcc("F0Ac").unwrap().to_be_bytes(), *b"F0Ac");
        for key in ["", "abc", "abcde", "\0abc", "éab"] {
            assert!(fourcc(key).is_err());
        }
    }

    #[test]
    fn response_checks_kernel_length_and_firmware_result() {
        assert!(response("F0Ac", 0, 80, Packet::default()).is_ok());
        for length in [0, 79, 81] {
            assert!(matches!(
                response("F0Ac", 0, length, Packet::default()),
                Err(Error::Invalid(_))
            ));
        }
        assert!(matches!(
            response("F0Ac", 0xE000_02C1_u32 as i32, 80, Packet::default()),
            Err(Error::Permission(_))
        ));
        assert!(matches!(
            response("F0Ac", 0xE000_02E2_u32 as i32, 80, Packet::default()),
            Err(Error::Permission(_))
        ));
        assert!(matches!(
            response("F0Ac", 42, 80, Packet::default()),
            Err(Error::Io(_))
        ));
        let missing = Packet {
            result: 0x84,
            ..Packet::default()
        };
        assert!(matches!(
            response("F0Ac", 0, 80, missing),
            Err(Error::Missing(_))
        ));
        let rejected = Packet {
            result: 0x85,
            ..Packet::default()
        };
        assert!(matches!(
            response("F0Ac", 0, 80, rejected),
            Err(Error::Smc { code: 0x85, .. })
        ));
    }

    #[test]
    fn trace_packet_fields_are_unknown_after_transport_failure_or_truncation() {
        let packet = Packet {
            result: 0x85,
            status: 3,
            info_size: 4,
            info_type: u32::from_be_bytes(*b"flt "),
            ..Packet::default()
        };
        for (kern, length) in [(42, 80), (0, 79), (0, 81)] {
            let trace = protocol::trace_reply(kern, length, &packet, Some("failed"));
            assert_eq!(trace.iokit, kern);
            assert_eq!(trace.length, length);
            assert!(trace.result.is_none());
            assert!(trace.status.is_none());
            assert!(trace.metadata.is_none());
            assert!(trace.payload.is_none());
        }
        let trace = protocol::trace_reply(0, 80, &packet, Some("firmware rejected"));
        assert_eq!(trace.result, Some(0x85));
        assert_eq!(trace.status, Some(3));
        assert_eq!(trace.metadata.unwrap().kind, *b"flt ");
        assert_eq!(trace.metadata.unwrap().size, 4);
        assert_eq!(trace.payload.unwrap().len(), 32);
    }
}
