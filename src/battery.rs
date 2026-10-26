//! Read-only battery temperatures from the two verified Apple Silicon SMC keys.
//!
//! The pinned Stats revision ffdf63741af0e801318c129ed29253f4e3679c4f maps
//! TB1T / TB2T to Battery 1 / Battery 2 in Modules/Sensors/values.swift:570-571:
//! https://github.com/exelban/stats/blob/ffdf63741af0e801318c129ed29253f4e3679c4f/Modules/Sensors/values.swift#L570-L571
//! Both keys must report four-byte little-endian `flt ` Celsius values. Their
//! precise physical locations are unconfirmed. TB0T and the IORegistry
//! Temperature field are not used.
//!
//! The accepted 0 < T <= 80 Celsius interval is a reading sanity boundary. It
//! does not define a safe operating temperature for the battery.

use crate::smc::{Error, ReadSmc, Result};

const KEYS: [&str; 2] = ["TB1T", "TB2T"];

#[derive(Clone, Debug, PartialEq)]
pub struct BatteryReading {
    pub key: String,
    pub celsius: f32,
}

fn validate_temperature(key: &str, temperature: f32) -> Result<()> {
    if !temperature.is_finite() || temperature <= 0.0 || temperature > 80.0 {
        return Err(Error::Invalid(format!(
            "battery {key}: abnormal temperature {temperature} C; expected a finite reading in (0, 80] C"
        )));
    }
    Ok(())
}

/// Read both confirmed keys. Any missing, malformed or invalid measurement
/// rejects the whole sample; partial readings are never returned as complete.
pub fn sample(smc: &mut impl ReadSmc) -> Result<Vec<BatteryReading>> {
    KEYS.into_iter()
        .map(|key| {
            let value = smc.read(key)?;
            if value.kind != *b"flt " || value.bytes.len() != 4 {
                return Err(Error::Invalid(format!(
                    "battery {key}: expected flt / 4 bytes, received {:?} / {} bytes",
                    String::from_utf8_lossy(&value.kind),
                    value.bytes.len()
                )));
            }
            let celsius = f32::from_le_bytes(value.bytes.as_slice().try_into().map_err(|_| {
                Error::Invalid(format!("battery {key}: invalid four-byte float payload"))
            })?);
            validate_temperature(key, celsius)?;
            Ok(BatteryReading {
                key: key.to_owned(),
                celsius,
            })
        })
        .collect()
}

/// Return the hotter confirmed measurement, requiring both keys exactly once.
/// Ties retain the first reading; the source key is preserved for diagnostics.
pub fn hottest(readings: &[BatteryReading]) -> Result<(f32, String)> {
    let mut present = [false; 2];
    let mut hottest: Option<&BatteryReading> = None;
    for reading in readings {
        let index = KEYS
            .iter()
            .position(|key| *key == reading.key)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unconfirmed battery temperature key {:?}",
                    reading.key
                ))
            })?;
        if present[index] {
            return Err(Error::Invalid(format!(
                "duplicate battery temperature key {}",
                reading.key
            )));
        }
        present[index] = true;
        validate_temperature(&reading.key, reading.celsius)?;
        if hottest.is_none_or(|current| reading.celsius > current.celsius) {
            hottest = Some(reading);
        }
    }
    for (key, found) in KEYS.into_iter().zip(present) {
        if !found {
            return Err(Error::Missing(format!("battery temperature key {key}")));
        }
    }
    hottest
        .map(|reading| (reading.celsius, reading.key.clone()))
        .ok_or_else(|| Error::Missing("no complete battery temperature sample".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smc::Value;
    use std::collections::BTreeMap;

    struct MockSmc {
        values: BTreeMap<String, Value>,
        reads: Vec<String>,
    }

    impl MockSmc {
        fn new() -> Self {
            Self {
                values: [("TB1T".into(), float(37.25)), ("TB2T".into(), float(34.5))].into(),
                reads: Vec::new(),
            }
        }
    }

    impl ReadSmc for MockSmc {
        fn read(&mut self, key: &str) -> Result<Value> {
            self.reads.push(key.to_owned());
            self.values
                .get(key)
                .cloned()
                .ok_or_else(|| Error::Missing(key.to_owned()))
        }

        fn keys(&mut self) -> Result<Vec<String>> {
            panic!("battery sampling must use only the confirmed keys");
        }
    }

    fn float(celsius: f32) -> Value {
        Value {
            kind: *b"flt ",
            bytes: celsius.to_le_bytes().to_vec(),
        }
    }

    fn readings(first: f32, second: f32) -> Vec<BatteryReading> {
        vec![
            BatteryReading {
                key: "TB1T".into(),
                celsius: first,
            },
            BatteryReading {
                key: "TB2T".into(),
                celsius: second,
            },
        ]
    }

    #[test]
    fn reads_only_both_confirmed_keys_and_decodes_little_endian() {
        let mut smc = MockSmc::new();
        smc.values.insert(
            "TB1T".into(),
            Value {
                kind: *b"flt ",
                bytes: vec![0x00, 0x00, 0x15, 0x42],
            },
        );
        smc.values.insert("TB0T".into(), float(70.0));
        let values = sample(&mut smc).unwrap();
        assert_eq!(smc.reads, ["TB1T", "TB2T"]);
        assert_eq!(values.len(), 2);
        assert!((values[0].celsius - 37.25).abs() < 0.00001);
        assert_eq!(values[1].celsius, 34.5);
    }

    #[test]
    fn either_missing_sensor_rejects_the_complete_sample() {
        for key in KEYS {
            let mut smc = MockSmc::new();
            smc.values.remove(key);
            assert!(matches!(sample(&mut smc), Err(Error::Missing(name)) if name == key));
        }
    }

    #[test]
    fn wrong_type_and_wrong_payload_lengths_are_rejected() {
        for value in [
            Value {
                kind: *b"sp78",
                bytes: vec![38, 0],
            },
            Value {
                kind: *b"ui32",
                bytes: vec![0, 0, 0, 38],
            },
            Value {
                kind: *b"flt ",
                bytes: vec![0; 3],
            },
            Value {
                kind: *b"flt ",
                bytes: vec![0; 5],
            },
        ] {
            for key in KEYS {
                let mut smc = MockSmc::new();
                smc.values.insert(key.into(), value.clone());
                let error = sample(&mut smc).unwrap_err().to_string();
                assert!(error.contains(key) && error.contains("expected flt / 4 bytes"));
            }
        }
    }

    #[test]
    fn nonfinite_and_out_of_range_readings_fail() {
        for temperature in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0, 80.01] {
            for key in KEYS {
                let mut smc = MockSmc::new();
                smc.values.insert(key.into(), float(temperature));
                assert!(sample(&mut smc).is_err(), "{key}={temperature}");
            }
            assert!(hottest(&readings(temperature, 35.0)).is_err());
        }
        assert_eq!(
            hottest(&readings(0.1, 80.0)).unwrap(),
            (80.0, "TB2T".into())
        );
    }

    #[test]
    fn hottest_returns_either_sensor_and_keeps_its_key() {
        assert_eq!(
            hottest(&readings(37.25, 34.5)).unwrap(),
            (37.25, "TB1T".into())
        );
        assert_eq!(
            hottest(&readings(34.5, 37.25)).unwrap(),
            (37.25, "TB2T".into())
        );
        assert_eq!(
            hottest(&readings(37.25, 37.25)).unwrap(),
            (37.25, "TB1T".into())
        );
    }

    #[test]
    fn hottest_rejects_partial_duplicate_and_unconfirmed_sets() {
        assert!(hottest(&[]).is_err());
        assert!(hottest(&readings(37.25, 34.5)[..1]).is_err());
        let mut duplicate = readings(37.25, 34.5);
        duplicate[1].key = "TB1T".into();
        assert!(
            hottest(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        let mut unconfirmed = readings(37.25, 34.5);
        unconfirmed[1].key = "TB0T".into();
        assert!(
            hottest(&unconfirmed)
                .unwrap_err()
                .to_string()
                .contains("unconfirmed")
        );
    }
}
