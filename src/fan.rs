//! Mode-key probing adapts MacFanControl (de9af6f) and Stats (ffdf637).
//! Strict discovery and hardware-bound validation are project additions.
//! See docs/PROVENANCE.md and THIRD_PARTY_NOTICES.md.

use crate::smc::{Error, ReadSmc, Result, Value, encode_rpm};

#[derive(Clone, Debug)]
pub struct FanSpec {
    pub index: u8,
    pub mode_key: String,
    pub target_kind: Option<[u8; 4]>,
}

impl FanSpec {
    pub fn key(&self, suffix: &str) -> String {
        format!("F{}{suffix}", self.index)
    }
    pub fn encode_target(&self, rpm: f32) -> Result<Value> {
        encode_rpm(
            self.target_kind
                .ok_or_else(|| Error::Invalid(format!("F{}Tg metadata unavailable", self.index)))?,
            rpm,
        )
    }
    pub fn mode(&self, smc: &mut impl ReadSmc) -> Result<u32> {
        let value = smc.read(&self.mode_key)?;
        validate_mode(&self.mode_key, &value)?;
        value.integer()
    }
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub fans: Vec<FanSpec>,
    pub ftst: bool,
    pub problems: Vec<String>,
}

pub fn count(smc: &mut impl ReadSmc) -> Result<u8> {
    let n = smc.read("FNum")?.integer()?;
    if n > 10 {
        return Err(Error::Invalid(format!(
            "FNum={n}: only fan indices 0..9 are supported"
        )));
    }
    Ok(n as u8)
}

fn validate_mode(key: &str, value: &Value) -> Result<()> {
    if value.kind != *b"ui8 " || value.bytes.len() != 1 {
        return Err(Error::Invalid(format!("{key}: expected ui8 / 1 byte")));
    }
    Ok(())
}

// Collect all usable controls so auto can still restore other fans after a failure.
pub fn plan(smc: &mut impl ReadSmc) -> Result<Plan> {
    let mut plan = Plan {
        fans: Vec::new(),
        ftst: false,
        problems: Vec::new(),
    };
    let n = match count(smc) {
        Ok(n) => n,
        Err(e) => {
            plan.problems.push(format!("fan count unavailable: {e}"));
            0
        }
    };
    for index in 0..n {
        let spec = (|| {
            let lower = format!("F{index}md");
            let (mode_key, mode) = match smc.read(&lower) {
                Ok(v) => (lower, v),
                Err(Error::Missing(_)) => {
                    let upper = format!("F{index}Md");
                    let v = smc.read(&upper)?;
                    (upper, v)
                }
                Err(e) => return Err(e),
            };
            validate_mode(&mode_key, &mode)?;
            let target_kind = match smc.read(&format!("F{index}Tg")).and_then(|target| {
                let encoded = encode_rpm(target.kind, 0.0)?;
                if encoded.bytes.len() != target.bytes.len() {
                    return Err(Error::Invalid(format!("F{index}Tg: invalid payload size")));
                }
                Ok(target.kind)
            }) {
                Ok(kind) => Some(kind),
                Err(e) => {
                    plan.problems.push(format!("fan {index} target: {e}"));
                    None
                }
            };
            Ok(FanSpec {
                index,
                mode_key,
                target_kind,
            })
        })();
        match spec {
            Ok(spec) => plan.fans.push(spec),
            Err(e) => plan.problems.push(format!("fan {index}: {e}")),
        }
    }
    match smc.read("Ftst") {
        Ok(v) => match validate_mode("Ftst", &v).and_then(|()| v.integer()) {
            Ok(0 | 1) => plan.ftst = true,
            Ok(n) => plan.problems.push(format!("Ftst: unexpected state {n}")),
            Err(e) => plan.problems.push(e.to_string()),
        },
        Err(Error::Missing(_)) => (),
        Err(e) => plan.problems.push(format!("Ftst: {e}")),
    }
    Ok(plan)
}

#[derive(Clone, Debug)]
pub struct FanState {
    pub index: u8,
    pub min: f32,
    pub max: f32,
    pub actual: f32,
    pub target: f32,
    pub mode: u32,
}

pub fn rpm(smc: &mut impl ReadSmc, key: &str) -> Result<f32> {
    let value = smc.read(key)?;
    if !matches!(&value.kind, b"flt " | b"fpe2") {
        return Err(Error::Invalid(format!("{key}: unsupported RPM type")));
    }
    let n = value.number()?;
    if n < 0.0 {
        return Err(Error::Invalid(format!("{key}: negative RPM {n}")));
    }
    Ok(n)
}

pub fn state(smc: &mut impl ReadSmc, spec: &FanSpec) -> Result<FanState> {
    let min = rpm(smc, &spec.key("Mn"))?;
    let max = rpm(smc, &spec.key("Mx"))?;
    if min >= max || max <= 0.0 {
        return Err(Error::Invalid(format!(
            "fan {}: invalid hardware range {min}..{max}",
            spec.index
        )));
    }
    let actual = rpm(smc, &spec.key("Ac"))?;
    let target = rpm(smc, &spec.key("Tg"))?;
    // Allow tachometer variation; this is a data sanity margin, never a target.
    if actual > max * 1.2 || target > max + 1.0 {
        return Err(Error::Invalid(format!(
            "fan {}: RPM exceeds reported maximum {max}",
            spec.index
        )));
    }
    Ok(FanState {
        index: spec.index,
        min,
        max,
        actual,
        target,
        mode: spec.mode(smc)?,
    })
}

pub fn mode_label(mode: u32) -> String {
    match mode {
        0 => "automatic".into(),
        1 => "manual".into(),
        3 => "system automatic (3)".into(),
        n => format!("unknown ({n})"),
    }
}
