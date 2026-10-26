//! Deterministic low-temperature-first policy. Thresholds are engineering
//! defaults for an audible cooling profile, not Apple thermal safety limits.
use std::collections::VecDeque;
use std::time::Duration;

use crate::smc::{Error, Result};

const COOLING_HYSTERESIS_C: f32 = 3.0;
const COOLING_CONFIRMATION: Duration = Duration::from_secs(10);
const WRITE_DEADBAND_RPM: f32 = 100.0;
const SPINUP_WINDOW: Duration = Duration::from_secs(30);

fn temperature(value: f32) -> Result<()> {
    if !value.is_finite() || value <= 0.0 || value > 130.0 {
        return Err(Error::Invalid(format!(
            "invalid control temperature {value} C"
        )));
    }
    Ok(())
}

/// Feed the hottest verified chip temperature, using monotonic elapsed time.
/// Each result is the target to hold, including hysteresis and write suppression.
pub struct ThermalPolicy {
    min: f32,
    max: f32,
    current: Option<f32>,
    high_water_c: f32,
    cooling_since: Option<Duration>,
    last_sample: Option<Duration>,
    decrease_budget: f32,
}

impl ThermalPolicy {
    pub fn new(min: f32, max: f32) -> Result<Self> {
        if !min.is_finite() || !max.is_finite() || min < 0.0 || min >= max {
            return Err(Error::Invalid(format!(
                "invalid hardware range {min}..{max}"
            )));
        }
        let (min, max) = (min.ceil(), max.floor());
        if min > max {
            return Err(Error::Invalid(
                "hardware range contains no integer RPM".into(),
            ));
        }
        Ok(Self {
            min,
            max,
            current: None,
            high_water_c: 0.0,
            cooling_since: None,
            last_sample: None,
            decrease_budget: 0.0,
        })
    }

    pub fn target(&self) -> Option<f32> {
        self.current
    }

    /// 40/50/60/70 C map to 25/50/75/100% of the hardware RPM span.
    pub fn curve_target(&self, temp_c: f32) -> Result<f32> {
        temperature(temp_c)?;
        let fraction = (0.25 + (temp_c - 40.0) * 0.025).clamp(0.25, 1.0);
        Ok((self.min + (self.max - self.min) * fraction)
            .round()
            .clamp(self.min, self.max))
    }

    /// Unknown telemetry interrupts the requirement for continuous cooling.
    pub fn telemetry_gap(&mut self) {
        self.cooling_since = None;
        self.decrease_budget = 0.0;
    }

    pub fn sample(&mut self, temp_c: f32, now: Duration) -> Result<f32> {
        let desired = match self.curve_target(temp_c) {
            Ok(value) => value,
            Err(error) => {
                self.telemetry_gap();
                return Err(error);
            }
        };
        if self.last_sample.is_some_and(|last| now < last) {
            return Err(Error::Invalid("thermal policy time moved backwards".into()));
        }
        let previous_time = self.last_sample.replace(now).unwrap_or(now);
        let Some(current) = self.current else {
            self.current = Some(desired);
            self.high_water_c = temp_c;
            return Ok(desired);
        };

        if desired >= current {
            self.high_water_c = temp_c;
            self.cooling_since = None;
            self.decrease_budget = 0.0;
            // Full-speed endpoint must remain reachable through a small final step.
            if desired - current >= WRITE_DEADBAND_RPM || desired == self.max {
                self.current = Some(desired);
            }
            return Ok(self.current.unwrap());
        }

        if temp_c > self.high_water_c - COOLING_HYSTERESIS_C {
            self.cooling_since = None;
            self.decrease_budget = 0.0;
            return Ok(current);
        }
        let since = *self.cooling_since.get_or_insert(now);
        let eligible_at = since.saturating_add(COOLING_CONFIRMATION);
        if now < eligible_at {
            return Ok(current);
        }
        // Accumulate only time after confirmation. The 100 RPM deadband must not
        // prevent slow ramps on fans whose range spans fewer than 2000 RPM.
        let rate_time = now.saturating_sub(previous_time.max(eligible_at));
        self.decrease_budget += (self.max - self.min) * 0.05 * rate_time.as_secs_f32();
        let candidate = (current - self.decrease_budget)
            .ceil()
            .max(desired)
            .clamp(self.min, self.max);
        let floor = self.curve_target(40.0)?;
        if current - candidate >= WRITE_DEADBAND_RPM || candidate == floor {
            self.current = Some(candidate);
            self.decrease_budget -= current - candidate;
            if candidate == desired {
                self.high_water_c = temp_c;
                self.cooling_since = None;
                self.decrease_budget = 0.0;
            }
        }
        Ok(self.current.unwrap())
    }
}

/// The lowest target held during the previous 30 seconds is a conservative
/// acceleration threshold. Changing a target never restarts the grace period.
pub struct SpinupWatchdog {
    started: Duration,
    last_sample: Duration,
    targets: VecDeque<(Duration, f32)>,
}

impl SpinupWatchdog {
    pub fn new(start: Duration, initial_target: f32) -> Result<Self> {
        validate_speed(initial_target)?;
        Ok(Self {
            started: start,
            last_sample: start,
            targets: VecDeque::from([(start, initial_target)]),
        })
    }

    /// Call on every healthy telemetry sample, including unchanged targets.
    pub fn sample(&mut self, now: Duration, requested: f32, actual: f32) -> Result<()> {
        validate_speed(requested)?;
        validate_speed(actual)?;
        if now < self.last_sample {
            return Err(Error::Invalid(
                "spinup watchdog time moved backwards".into(),
            ));
        }
        self.last_sample = now;
        if self
            .targets
            .back()
            .is_some_and(|(_, target)| *target != requested)
        {
            self.targets.push_back((now, requested));
        }
        let boundary = now.saturating_sub(SPINUP_WINDOW);
        // Retain the target in force at the left edge, even if set before it.
        while self.targets.len() >= 2 && self.targets[1].0 <= boundary {
            self.targets.pop_front();
        }
        if now.saturating_sub(self.started) < SPINUP_WINDOW {
            return Ok(());
        }
        let minimum = self
            .targets
            .iter()
            .map(|(_, rpm)| *rpm)
            .fold(f32::INFINITY, f32::min);
        if actual < minimum * 0.8 {
            return Err(Error::Control(format!(
                "actual {actual:.0} RPM below 80% of the lowest requested target {minimum:.0} RPM in the previous 30 seconds"
            )));
        }
        Ok(())
    }
}

fn validate_speed(value: f32) -> Result<()> {
    if !value.is_finite() || value < 0.0 {
        return Err(Error::Invalid(format!("invalid watchdog RPM {value}")));
    }
    Ok(())
}
