//! Raw SMC temperature discovery adapts MacFanControl (de9af6f).
//! Per-sensor error reporting is a project addition. See docs/PROVENANCE.md
//! and THIRD_PARTY_NOTICES.md. These keys do not drive chip cooling demand.

use crate::smc::{Error, ReadSmc, Result};

#[derive(Debug)]
pub struct Temperatures {
    pub keys: Vec<String>,
    pub inactive_count: usize,
    pub discovery_warnings: Vec<String>,
}

impl Temperatures {
    pub fn discover(smc: &mut impl ReadSmc) -> Result<Self> {
        let mut keys = Vec::new();
        let mut inactive_count = 0;
        let mut discovery_warnings = Vec::new();
        for key in smc.keys()?.into_iter().filter(|k| k.starts_with('T')) {
            match smc.read(&key) {
                Ok(v) if matches!(&v.kind, b"flt " | b"sp78") => {
                    // Keep numeric sensors even when their current sample is invalid.
                    match v.number() {
                        Ok(0.0) => inactive_count += 1,
                        _ => keys.push(key),
                    }
                }
                Ok(_) => (),
                Err(e) => discovery_warnings.push(format!("{key}: {e}")),
            }
        }
        keys.sort();
        keys.dedup();
        if keys.is_empty() {
            return Err(Error::Invalid("no readable temperature sensors".into()));
        }
        Ok(Self {
            keys,
            inactive_count,
            discovery_warnings,
        })
    }

    pub fn sample(&self, smc: &mut impl ReadSmc) -> Vec<(String, Result<f32>)> {
        self.keys
            .iter()
            .map(|key| {
                let result = smc.read(key).and_then(|v| {
                    if !matches!(&v.kind, b"flt " | b"sp78") {
                        return Err(Error::Invalid(format!("{key}: temperature type changed")));
                    }
                    let t = v.number()?;
                    if !(0.0..=130.0).contains(&t) || t == 0.0 {
                        return Err(Error::Invalid(format!("{key}: abnormal temperature {t} C")));
                    }
                    Ok(t)
                });
                (key.clone(), result)
            })
            .collect()
    }
}
