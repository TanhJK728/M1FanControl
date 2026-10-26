//! Named, read-only Apple Silicon IOHID temperature measurements.
//!
//! Product-name meanings and matching criteria were checked against the pinned
//! Stats Sensors reader and Chromium's M1SensorsReader. These are engineering
//! mappings, not Apple's published calibration or per-core topology guarantees.
//! Client/service caching and periodic rediscovery adapt Stats (ffdf637).
//! IOHID interface prior art includes the BSD-licensed freedomtan/sensors.
//! Validation and ownership wrappers are project implementations. See
//! docs/PROVENANCE.md and THIRD_PARTY_NOTICES.md.
//! No SMC temperature key is interpreted as a CPU sensor here.

use crate::smc::{Error, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensorGroup {
    Performance,
    Efficiency,
    Gpu,
    Soc,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChipReading {
    /// Exact IOHID Product property, retained for diagnostic provenance.
    pub name: String,
    pub group: SensorGroup,
    pub celsius: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipTiming {
    pub name: String,
    pub event_ticks: u64,
    pub read_ticks: u64,
    pub timebase_numer: u32,
    pub timebase_denom: u32,
    pub age_ns: u64,
}

pub trait ChipTemperatureSource {
    /// Return a complete current sample or an error; never return cached values.
    fn sample(&mut self) -> Result<Vec<ChipReading>>;
}

const TEMPERATURE_EVENT: u32 = 15;
const MAX_EVENT_AGE_NS: u64 = 5_000_000_000;

fn group_for_product(name: &str) -> Option<SensorGroup> {
    [
        ("pACC MTR Temp Sensor", SensorGroup::Performance),
        ("eACC MTR Temp Sensor", SensorGroup::Efficiency),
        ("GPU MTR Temp Sensor", SensorGroup::Gpu),
        ("SOC MTR Temp Sensor", SensorGroup::Soc),
    ]
    .into_iter()
    .find_map(|(prefix, group)| {
        let suffix = name.strip_prefix(prefix)?;
        let index = suffix.parse::<u8>().ok()?;
        (index < 64 && suffix == index.to_string()).then_some(group)
    })
}

struct RawReading {
    name: String,
    event_type: u32,
    celsius: f64,
    event_ticks: u64,
    read_ticks: u64,
    timebase_numer: u32,
    timebase_denom: u32,
}

trait RawSource {
    fn read(&mut self) -> Result<Vec<RawReading>>;
}

#[derive(Debug)]
struct Sample {
    readings: Vec<ChipReading>,
    timings: Vec<ChipTiming>,
}

fn event_age_ns(raw: &RawReading) -> Result<u64> {
    if raw.event_ticks == 0
        || raw.event_ticks > raw.read_ticks
        || raw.timebase_numer == 0
        || raw.timebase_denom == 0
    {
        return Err(Error::Invalid(format!(
            "HID {:?}: invalid event timestamp {}, read timestamp {}, timebase {}/{}",
            raw.name, raw.event_ticks, raw.read_ticks, raw.timebase_numer, raw.timebase_denom
        )));
    }
    let elapsed = u128::from(raw.read_ticks - raw.event_ticks) * u128::from(raw.timebase_numer)
        / u128::from(raw.timebase_denom);
    if elapsed > u128::from(MAX_EVENT_AGE_NS) {
        return Err(Error::Invalid(format!(
            "HID {:?}: stale event age {elapsed} ns exceeds {MAX_EVENT_AGE_NS} ns; event={}, read={}, timebase={}/{}",
            raw.name, raw.event_ticks, raw.read_ticks, raw.timebase_numer, raw.timebase_denom
        )));
    }
    Ok(elapsed as u64)
}

fn decode(raw: Vec<RawReading>) -> Result<Sample> {
    let mut readings = Vec::with_capacity(raw.len());
    let mut timings = Vec::with_capacity(raw.len());
    let mut names = BTreeMap::new();
    for r in raw {
        let group = group_for_product(&r.name).ok_or_else(|| {
            Error::Invalid(format!(
                "unrecognized chip temperature Product {:?}",
                r.name
            ))
        })?;
        if names.insert(r.name.clone(), group).is_some() {
            return Err(Error::Invalid(format!(
                "duplicate chip temperature Product {:?}",
                r.name
            )));
        }
        if r.event_type != TEMPERATURE_EVENT {
            return Err(Error::Invalid(format!(
                "HID {:?}: expected temperature event 15, received {}",
                r.name, r.event_type
            )));
        }
        let celsius = r.celsius as f32;
        if !r.celsius.is_finite()
            || r.celsius <= 0.0
            || r.celsius > 130.0
            || !celsius.is_finite()
            || celsius <= 0.0
        {
            return Err(Error::Invalid(format!(
                "HID {:?}: abnormal temperature {} C",
                r.name, r.celsius
            )));
        }
        let age_ns = event_age_ns(&r)?;
        timings.push(ChipTiming {
            name: r.name.clone(),
            event_ticks: r.event_ticks,
            read_ticks: r.read_ticks,
            timebase_numer: r.timebase_numer,
            timebase_denom: r.timebase_denom,
            age_ns,
        });
        readings.push(ChipReading {
            name: r.name,
            group,
            celsius,
        });
    }
    for required in [
        SensorGroup::Performance,
        SensorGroup::Efficiency,
        SensorGroup::Gpu,
    ] {
        if !readings.iter().any(|r| r.group == required) {
            return Err(Error::Missing(format!(
                "HID chip temperature group {required:?}; available Products: {:?}",
                readings.iter().map(|r| &r.name).collect::<Vec<_>>()
            )));
        }
    }
    readings.sort_by(|a, b| a.name.cmp(&b.name));
    timings.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Sample { readings, timings })
}

struct CheckedSource<S> {
    source: S,
    selected: BTreeMap<String, SensorGroup>,
    timings: Vec<ChipTiming>,
    fresh_after: Option<u64>,
}

impl<S: RawSource> CheckedSource<S> {
    fn open(mut source: S) -> Result<Self> {
        let first = decode(source.read()?)?;
        Ok(Self {
            source,
            selected: first
                .readings
                .into_iter()
                .map(|r| (r.name, r.group))
                .collect(),
            timings: first.timings,
            fresh_after: None,
        })
    }

    fn open_fresh(source: S, fence: u64) -> Result<Self> {
        let mut checked = Self::open(source)?;
        checked.fresh_after = Some(fence);
        // Discovery may observe a cached event. It establishes the sensor set,
        // but is never counted as a post-fence accepted health sample.
        checked.timings.clear();
        Ok(checked)
    }

    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        let sample = decode(self.source.read()?)?;
        let readings = sample.readings;
        let current: BTreeMap<_, _> = readings.iter().map(|r| (r.name.clone(), r.group)).collect();
        if current != self.selected {
            let missing: Vec<_> = self
                .selected
                .keys()
                .filter(|name| !current.contains_key(*name))
                .collect();
            let added: Vec<_> = current
                .keys()
                .filter(|name| !self.selected.contains_key(*name))
                .collect();
            return Err(Error::Invalid(format!(
                "HID chip temperature set changed: missing={missing:?}, added={added:?}"
            )));
        }
        if let Some(fence) = self.fresh_after {
            for timing in &sample.timings {
                let previous = self
                    .timings
                    .iter()
                    .find(|old| old.name == timing.name)
                    .map_or(fence, |old| old.event_ticks.max(fence));
                if timing.event_ticks <= previous {
                    return Err(Error::Invalid(format!(
                        "HID {:?}: event {} must be newer than epoch fence {fence} and last accepted event {previous}",
                        timing.name, timing.event_ticks
                    )));
                }
            }
        }
        // Commit only after every sensor passed. A partially advanced batch
        // cannot consume an event or make a repeated batch appear healthy.
        self.timings = sample.timings;
        Ok(readings)
    }
}

pub struct HidTemperatures {
    #[cfg(target_os = "macos")]
    source: CheckedSource<native::HidSource>,
}

impl HidTemperatures {
    /// Discover and validate performance CPU, efficiency CPU and GPU groups.
    /// SOC sensors are included when present. The initial sensor set is frozen.
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                source: CheckedSource::open(native::HidSource::open()?)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        Err(Error::Missing("IOHID temperatures require macOS".into()))
    }

    /// Rebuild the client and require genuinely post-open, advancing HID events.
    /// Initial discovery does not count as a health sample; sample() may return
    /// an error until every sensor has produced an event newer than this epoch.
    pub fn open_fresh() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let fence = native::absolute_ticks();
            Ok(Self {
                source: CheckedSource::open_fresh(native::HidSource::open()?, fence)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        Err(Error::Missing("IOHID temperatures require macOS".into()))
    }

    /// Native timestamps for the last successful complete sample. Fresh sources
    /// return an empty slice until sample() accepts their first post-fence batch.
    /// Event timestamps do not prove freshness inside sensor firmware.
    pub fn timings(&self) -> &[ChipTiming] {
        #[cfg(target_os = "macos")]
        {
            &self.source.timings
        }
        #[cfg(not(target_os = "macos"))]
        &[]
    }
}

impl ChipTemperatureSource for HidTemperatures {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        #[cfg(target_os = "macos")]
        {
            self.source.sample()
        }
        #[cfg(not(target_os = "macos"))]
        Err(Error::Missing("IOHID temperatures require macOS".into()))
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::{Error, RawReading, RawSource, Result, TEMPERATURE_EVENT, group_for_product};
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::{NonNull, null};
    use std::time::{Duration, Instant};

    type CfRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const REDISCOVER: Duration = Duration::from_secs(60);

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(object: CfRef);
        fn CFGetTypeID(object: CfRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFStringCreateWithBytes(
            allocator: CfRef,
            bytes: *const u8,
            count: isize,
            encoding: u32,
            external: u8,
        ) -> CfRef;
        fn CFStringGetLength(string: CfRef) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32)
        -> u8;
        fn CFNumberCreate(allocator: CfRef, kind: isize, value: *const c_void) -> CfRef;
        fn CFDictionaryCreate(
            allocator: CfRef,
            keys: *const CfRef,
            values: *const CfRef,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CfRef;
        // Callback structs are opaque here; their addresses are passed unchanged.
        static kCFTypeDictionaryKeyCallBacks: c_void;
        static kCFTypeDictionaryValueCallBacks: c_void;
        fn CFArrayGetCount(array: CfRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDEventSystemClientCreate(allocator: CfRef) -> CfRef;
        // Apple WebKit's SPI declares void, so no return status is fabricated.
        fn IOHIDEventSystemClientSetMatching(client: CfRef, matching: CfRef);
        fn IOHIDEventSystemClientCopyServices(client: CfRef) -> CfRef;
        fn IOHIDServiceClientCopyProperty(service: CfRef, key: CfRef) -> CfRef;
        fn IOHIDServiceClientCopyEvent(
            service: CfRef,
            event_type: i64,
            matching: i32,
            options: i64,
        ) -> CfRef;
        fn IOHIDEventGetType(event: CfRef) -> u32;
        // Apple's HIDEvent timestamp accessor documents mach absolute time.
        fn IOHIDEventGetTimeStamp(event: CfRef) -> u64;
        // IOHIDFloat is double on LP64 macOS, including Apple M1.
        fn IOHIDEventGetFloatValue(event: CfRef, field: u32) -> f64;
    }

    struct Owned(NonNull<c_void>);

    impl Owned {
        fn new(pointer: CfRef, context: &str) -> Result<Self> {
            NonNull::new(pointer.cast_mut())
                .map(Self)
                .ok_or_else(|| Error::Missing(format!("HID {context} returned null")))
        }

        fn raw(&self) -> CfRef {
            self.0.as_ptr().cast_const()
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            // Each Owned is constructed exclusively from a Create/Copy result.
            unsafe { CFRelease(self.raw()) }
        }
    }

    fn cf_string(value: &str) -> Result<Owned> {
        Owned::new(
            unsafe {
                CFStringCreateWithBytes(null(), value.as_ptr(), value.len() as isize, UTF8, 0)
            },
            "CFStringCreateWithBytes",
        )
    }

    fn property_name(service: CfRef, product: &Owned) -> Result<String> {
        let name = Owned::new(
            unsafe { IOHIDServiceClientCopyProperty(service, product.raw()) },
            "Product property",
        )?;
        if unsafe { CFGetTypeID(name.raw()) != CFStringGetTypeID() } {
            return Err(Error::Invalid(
                "HID Product property is not a CFString".into(),
            ));
        }
        let length = unsafe { CFStringGetLength(name.raw()) };
        // Product identifiers should be short; bound any allocation from native data.
        if !(1..=1024).contains(&length) {
            return Err(Error::Invalid(format!("HID Product length {length}")));
        }
        let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8) } + 1;
        if !(1..=4097).contains(&capacity) {
            return Err(Error::Invalid("HID Product UTF-8 length is invalid".into()));
        }
        let mut bytes = vec![0; capacity as usize];
        if unsafe { CFStringGetCString(name.raw(), bytes.as_mut_ptr().cast(), capacity, UTF8) } == 0
        {
            return Err(Error::Invalid("HID Product is not valid UTF-8".into()));
        }
        let text = CStr::from_bytes_until_nul(&bytes)
            .ok()
            .and_then(|s| s.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| Error::Invalid("HID Product is not valid UTF-8".into()))?;
        if text.encode_utf16().count() != length as usize {
            return Err(Error::Invalid(
                "HID Product contains an embedded NUL or was truncated".into(),
            ));
        }
        Ok(text)
    }

    struct Selected {
        name: String,
        service: CfRef,
    }

    pub(super) struct HidSource {
        // Services are borrowed from this owned CFArray. It must outlive reads.
        services: Owned,
        selected: Vec<Selected>,
        client: Owned,
        product: Owned,
        discovered: Instant,
        timebase: Timebase,
    }

    pub(super) fn absolute_ticks() -> u64 {
        unsafe { mach_absolute_time() }
    }

    impl HidSource {
        pub(super) fn open() -> Result<Self> {
            let mut timebase = Timebase { numer: 0, denom: 0 };
            let status = unsafe { mach_timebase_info(&mut timebase) };
            if status != 0 || timebase.numer == 0 || timebase.denom == 0 {
                return Err(Error::Invalid(format!(
                    "HID mach_timebase_info failed: {status}, {}/{}",
                    timebase.numer, timebase.denom
                )));
            }
            let client = Owned::new(
                unsafe { IOHIDEventSystemClientCreate(null()) },
                "IOHIDEventSystemClientCreate",
            )?;
            let page_key = cf_string("PrimaryUsagePage")?;
            let usage_key = cf_string("PrimaryUsage")?;
            let page: i32 = 0xff00;
            let usage: i32 = 0x0005;
            // kCFNumberSInt32Type = 3, from the SDK's CFNumber.h.
            let page_value = Owned::new(
                unsafe { CFNumberCreate(null(), 3, (&raw const page).cast()) },
                "usage page number",
            )?;
            let usage_value = Owned::new(
                unsafe { CFNumberCreate(null(), 3, (&raw const usage).cast()) },
                "usage number",
            )?;
            let keys = [page_key.raw(), usage_key.raw()];
            let values = [page_value.raw(), usage_value.raw()];
            let matching = Owned::new(
                unsafe {
                    CFDictionaryCreate(
                        null(),
                        keys.as_ptr(),
                        values.as_ptr(),
                        2,
                        &raw const kCFTypeDictionaryKeyCallBacks,
                        &raw const kCFTypeDictionaryValueCallBacks,
                    )
                },
                "matching dictionary",
            )?;
            unsafe { IOHIDEventSystemClientSetMatching(client.raw(), matching.raw()) };
            let product = cf_string("Product")?;
            let (services, selected) = Self::discover(&client, &product)?;
            Ok(Self {
                services,
                selected,
                client,
                product,
                discovered: Instant::now(),
                timebase,
            })
        }

        fn discover(client: &Owned, product: &Owned) -> Result<(Owned, Vec<Selected>)> {
            let services = Owned::new(
                unsafe { IOHIDEventSystemClientCopyServices(client.raw()) },
                "IOHIDEventSystemClientCopyServices",
            )?;
            if unsafe { CFGetTypeID(services.raw()) != CFArrayGetTypeID() } {
                return Err(Error::Invalid(
                    "HID services result is not a CFArray".into(),
                ));
            }
            let count = unsafe { CFArrayGetCount(services.raw()) };
            if !(1..=4096).contains(&count) {
                return Err(Error::Missing(format!(
                    "HID temperature service count {count}"
                )));
            }
            let mut selected = Vec::new();
            for index in 0..count {
                let service = unsafe { CFArrayGetValueAtIndex(services.raw(), index) };
                if service.is_null() {
                    return Err(Error::Missing(format!("HID service {index} is null")));
                }
                let name = property_name(service, product)?;
                if group_for_product(&name).is_some() {
                    selected.push(Selected { name, service });
                }
            }
            Ok((services, selected))
        }
    }

    impl RawSource for HidSource {
        fn read(&mut self) -> Result<Vec<RawReading>> {
            if self.discovered.elapsed() >= REDISCOVER {
                let (services, selected) = Self::discover(&self.client, &self.product)?;
                self.selected = selected;
                self.services = services;
                self.discovered = Instant::now();
                // CheckedSource compares the rediscovered set to the frozen set.
            }
            self.selected
                .iter()
                .map(|sensor| {
                    let name = property_name(sensor.service, &self.product)?;
                    if name != sensor.name {
                        return Err(Error::Invalid(format!(
                            "HID Product changed from {:?} to {name:?}",
                            sensor.name
                        )));
                    }
                    let event = Owned::new(
                        unsafe {
                            IOHIDServiceClientCopyEvent(
                                sensor.service,
                                i64::from(TEMPERATURE_EVENT),
                                0,
                                0,
                            )
                        },
                        &format!("temperature event for {name:?}"),
                    )?;
                    let event_type = unsafe { IOHIDEventGetType(event.raw()) };
                    let event_ticks = unsafe { IOHIDEventGetTimeStamp(event.raw()) };
                    let read_ticks = unsafe { mach_absolute_time() };
                    let celsius =
                        unsafe { IOHIDEventGetFloatValue(event.raw(), TEMPERATURE_EVENT << 16) };
                    Ok(RawReading {
                        name,
                        event_type,
                        celsius,
                        event_ticks,
                        read_ticks,
                        timebase_numer: self.timebase.numer,
                        timebase_denom: self.timebase.denom,
                    })
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockHid {
        frames: VecDeque<Result<Vec<RawReading>>>,
    }

    impl RawSource for MockHid {
        fn read(&mut self) -> Result<Vec<RawReading>> {
            self.frames.pop_front().expect("a queued Mock HID frame")
        }
    }

    fn frame(soc: bool) -> Vec<RawReading> {
        let mut names = vec![
            "pACC MTR Temp Sensor0",
            "pACC MTR Temp Sensor1",
            "eACC MTR Temp Sensor0",
            "GPU MTR Temp Sensor0",
        ];
        if soc {
            names.push("SOC MTR Temp Sensor0");
        }
        names
            .into_iter()
            .map(|name| RawReading {
                name: name.into(),
                event_type: TEMPERATURE_EVENT,
                celsius: 55.0,
                event_ticks: 100,
                read_ticks: 101,
                timebase_numer: 125,
                timebase_denom: 3,
            })
            .collect()
    }

    fn checked(frames: Vec<Result<Vec<RawReading>>>) -> Result<CheckedSource<MockHid>> {
        CheckedSource::open(MockHid {
            frames: frames.into(),
        })
    }

    #[test]
    fn exact_products_and_index_range_only() {
        for (name, group) in [
            ("pACC MTR Temp Sensor0", SensorGroup::Performance),
            ("eACC MTR Temp Sensor63", SensorGroup::Efficiency),
            ("GPU MTR Temp Sensor1", SensorGroup::Gpu),
            ("SOC MTR Temp Sensor12", SensorGroup::Soc),
        ] {
            assert_eq!(group_for_product(name), Some(group));
        }
        for name in [
            "TH0T",
            "TCHP",
            "pACC MTR Temp Sensor",
            "pACC MTR Temp Sensor64",
            "pACC MTR Temp Sensor01",
            "pACC MTR Temp Sensor-1",
            "pACC MTR Temp Sensor+1",
            "pACC MTR Temp Sensor1 ",
            "pACC MTR Temp Sensor1junk",
        ] {
            assert_eq!(group_for_product(name), None, "{name}");
        }
    }

    #[test]
    fn startup_requires_each_cpu_group_and_gpu() {
        for prefix in ["pACC", "eACC", "GPU"] {
            let mut sample = frame(true);
            sample.retain(|r| !r.name.starts_with(prefix));
            assert!(matches!(checked(vec![Ok(sample)]), Err(Error::Missing(_))));
        }
        assert!(checked(vec![Ok(frame(false))]).is_ok());
        assert!(checked(vec![Ok(frame(true))]).is_ok());
    }

    #[test]
    fn sample_preserves_product_and_reads_fresh_values() {
        let mut changed = frame(true);
        changed[0].celsius = 72.25;
        let mut sensor = checked(vec![Ok(frame(true)), Ok(changed)]).unwrap();
        let readings = sensor.sample().unwrap();
        let r = readings
            .iter()
            .find(|r| r.name == "pACC MTR Temp Sensor0")
            .unwrap();
        assert_eq!(r.celsius, 72.25);
        assert_eq!(r.group, SensorGroup::Performance);
        assert_eq!(readings.len(), 5);
    }

    #[test]
    fn lost_single_sensor_fails_even_with_all_groups_remaining() {
        let mut fewer = frame(true);
        fewer.remove(1);
        let mut sensor = checked(vec![Ok(frame(true)), Ok(fewer)]).unwrap();
        assert!(sensor.sample().unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn optional_soc_cannot_disappear_after_start() {
        let mut sensor = checked(vec![Ok(frame(true)), Ok(frame(false))]).unwrap();
        assert!(sensor.sample().is_err());
    }

    #[test]
    fn rediscovery_cannot_silently_add_or_replace_sensors() {
        let mut sensor = checked(vec![Ok(frame(false)), Ok(frame(true))]).unwrap();
        assert!(sensor.sample().unwrap_err().to_string().contains("added"));
        let mut replacement = frame(true);
        replacement[0].name = "pACC MTR Temp Sensor2".into();
        let mut sensor = checked(vec![Ok(frame(true)), Ok(replacement)]).unwrap();
        assert!(sensor.sample().is_err());
    }

    #[test]
    fn duplicate_names_are_errors_at_start_and_during_sampling() {
        for during in [false, true] {
            let mut bad = frame(false);
            bad[1].name = bad[0].name.clone();
            if during {
                let mut source = checked(vec![Ok(frame(false)), Ok(bad)]).unwrap();
                assert!(
                    source
                        .sample()
                        .unwrap_err()
                        .to_string()
                        .contains("duplicate")
                );
            } else {
                assert!(checked(vec![Ok(bad)]).is_err());
            }
        }
    }

    #[test]
    fn invalid_event_types_and_temperatures_never_become_samples() {
        for value in [
            0.0,
            -1.0,
            f64::MIN_POSITIVE,
            130.01,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let mut bad = frame(false);
            bad[0].celsius = value;
            let mut source = checked(vec![Ok(frame(false)), Ok(bad)]).unwrap();
            assert!(source.sample().is_err(), "value {value}");
        }
        let mut bad = frame(false);
        bad[0].event_type = 25;
        assert!(decode(bad).unwrap_err().to_string().contains("event 15"));
        let mut boundary = frame(false);
        boundary[0].celsius = 130.0;
        assert!(decode(boundary).is_ok());
    }

    #[test]
    fn native_failure_does_not_return_previous_values() {
        let mut source = checked(vec![
            Ok(frame(false)),
            Err(Error::Missing("HID Product property is missing".into())),
        ])
        .unwrap();
        assert!(matches!(source.sample(), Err(Error::Missing(_))));
    }

    #[test]
    fn event_timestamp_validation_and_tick_conversion() {
        let mut sample = frame(false);
        sample[0].read_ticks = sample[0].event_ticks + 24_000_000;
        assert_eq!(event_age_ns(&sample[0]).unwrap(), 1_000_000_000);
        sample[0].read_ticks = sample[0].event_ticks + 120_000_000;
        assert_eq!(event_age_ns(&sample[0]).unwrap(), MAX_EVENT_AGE_NS);
        sample[0].read_ticks += 1;
        assert!(event_age_ns(&sample[0]).is_err());
        sample[0].read_ticks = 99;
        assert!(event_age_ns(&sample[0]).is_err());
        sample[0].read_ticks = 101;
        sample[0].event_ticks = 0;
        assert!(event_age_ns(&sample[0]).is_err());
        sample[0].event_ticks = 100;
        sample[0].timebase_denom = 0;
        assert!(event_age_ns(&sample[0]).is_err());
        sample[0].timebase_denom = 3;
        sample[0].timebase_numer = 0;
        assert!(event_age_ns(&sample[0]).is_err());
    }

    #[test]
    fn stale_event_cannot_be_accepted_even_with_plausible_temperature() {
        let mut stale = frame(true);
        stale[0].read_ticks += 240_000_000;
        let mut source = checked(vec![Ok(frame(true)), Ok(stale)]).unwrap();
        assert!(
            source
                .sample()
                .unwrap_err()
                .to_string()
                .contains("stale event")
        );
        assert_eq!(source.timings[0].age_ns, 41);
    }

    #[test]
    fn unchanged_temperatures_with_fresh_events_are_valid() {
        let mut current = frame(false);
        for raw in &mut current {
            raw.event_ticks += 24_000_000;
            raw.read_ticks += 24_000_000;
        }
        let mut source = checked(vec![Ok(frame(false)), Ok(current)]).unwrap();
        assert_eq!(source.sample().unwrap()[0].celsius, 55.0);
        assert_eq!(source.timings[0].event_ticks, 24_000_100);
    }
}

#[cfg(test)]
mod fresh_tests {
    use super::*;
    use std::collections::VecDeque;

    struct MockRaw {
        frames: VecDeque<Result<Vec<RawReading>>>,
    }
    impl RawSource for MockRaw {
        fn read(&mut self) -> Result<Vec<RawReading>> {
            self.frames.pop_front().expect("scripted HID snapshot")
        }
    }
    fn raw(ticks: u64, soc: bool) -> Vec<RawReading> {
        let mut names = vec![
            "pACC MTR Temp Sensor0",
            "eACC MTR Temp Sensor0",
            "GPU MTR Temp Sensor0",
        ];
        if soc {
            names.push("SOC MTR Temp Sensor0");
        }
        names
            .into_iter()
            .map(|name| RawReading {
                name: name.into(),
                event_type: TEMPERATURE_EVENT,
                celsius: 55.0,
                event_ticks: ticks,
                read_ticks: ticks + 10,
                timebase_numer: 1,
                timebase_denom: 1,
            })
            .collect()
    }
    fn source(frames: Vec<Vec<RawReading>>) -> MockRaw {
        MockRaw {
            frames: frames.into_iter().map(Ok).collect(),
        }
    }

    #[test]
    fn discovery_does_not_count_cached_events_as_post_fence_health() {
        let mut checked = CheckedSource::open_fresh(
            source(vec![raw(99, false), raw(100, false), raw(101, false)]),
            100,
        )
        .unwrap();
        assert!(checked.timings.is_empty());
        assert!(
            checked
                .sample()
                .unwrap_err()
                .to_string()
                .contains("epoch fence")
        );
        assert!(checked.timings.is_empty());
        checked.sample().unwrap();
        assert!(checked.timings.iter().all(|t| t.event_ticks == 101));
    }

    #[test]
    fn every_accepted_fresh_batch_must_advance_all_sensor_events() {
        let mut checked = CheckedSource::open_fresh(
            source(vec![
                raw(99, false),
                raw(101, false),
                raw(101, false),
                raw(102, false),
            ]),
            100,
        )
        .unwrap();
        checked.sample().unwrap();
        assert!(
            checked
                .sample()
                .unwrap_err()
                .to_string()
                .contains("last accepted event")
        );
        checked.sample().unwrap();
        assert!(checked.timings.iter().all(|t| t.event_ticks == 102));
    }

    #[test]
    fn rejected_partial_advance_never_consumes_the_other_sensors_events() {
        let mut partial = raw(102, true);
        partial[0].event_ticks = 101;
        let mut checked = CheckedSource::open_fresh(
            source(vec![raw(99, true), raw(101, true), partial, raw(102, true)]),
            100,
        )
        .unwrap();
        checked.sample().unwrap();
        assert!(checked.sample().is_err());
        assert!(checked.timings.iter().all(|t| t.event_ticks == 101));
        checked.sample().unwrap();
        assert!(checked.timings.iter().all(|t| t.event_ticks == 102));
    }

    #[test]
    fn reconstructed_source_can_validate_a_new_complete_sensor_set() {
        let mut old = CheckedSource::open_fresh(
            source(vec![raw(99, true), raw(101, true), raw(102, false)]),
            100,
        )
        .unwrap();
        old.sample().unwrap();
        assert!(
            old.sample()
                .unwrap_err()
                .to_string()
                .contains("set changed")
        );
        drop(old);
        let mut reopened =
            CheckedSource::open_fresh(source(vec![raw(102, false), raw(201, false)]), 200).unwrap();
        assert_eq!(reopened.sample().unwrap().len(), 3);
        let mut incomplete = raw(201, false);
        incomplete.retain(|r| !r.name.starts_with("GPU"));
        assert!(CheckedSource::open_fresh(source(vec![incomplete]), 200).is_err());
    }

    #[test]
    fn fresh_epoch_also_retains_timestamp_age_validation() {
        let mut stale = raw(101, false);
        stale[0].read_ticks = 6_000_000_102;
        let mut checked =
            CheckedSource::open_fresh(source(vec![raw(99, false), stale, raw(102, false)]), 100)
                .unwrap();
        assert!(
            checked
                .sample()
                .unwrap_err()
                .to_string()
                .contains("stale event")
        );
        assert!(checked.timings.is_empty());
        checked.sample().unwrap();
    }

    #[test]
    fn legacy_open_keeps_age_checked_repeated_event_behavior() {
        let mut checked =
            CheckedSource::open(source(vec![raw(100, false), raw(100, false)])).unwrap();
        checked.sample().unwrap();
        assert!(checked.timings.iter().all(|t| t.event_ticks == 100));
    }
}
