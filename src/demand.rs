//! Temperature-triggered ownership demand. The same policy applies on AC and
//! battery power; electrical supply is intentionally absent from this interface.
use std::time::Duration;

use crate::smc::{Error, Result};

const RELEASE_DELAY: Duration = Duration::from_secs(30);
const DECLINE_DELAY: Duration = Duration::from_secs(10);
const WRITE_DEADBAND_RPM: f32 = 100.0;

pub struct ThresholdPolicy {
    min: f32,
    max: f32,
    current: Option<f32>,
    chip_active: bool,
    battery_active: bool,
    chip_below_since: Option<Duration>,
    battery_below_since: Option<Duration>,
    decline_since: Option<Duration>,
    ramp: Option<f32>,
    last_demand: Option<f32>,
    last_sample: Option<Duration>,
}

impl ThresholdPolicy {
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
            chip_active: false,
            battery_active: false,
            chip_below_since: None,
            battery_below_since: None,
            decline_since: None,
            ramp: None,
            last_demand: None,
            last_sample: None,
        })
    }

    pub fn chip_active(&self) -> bool {
        self.chip_active
    }

    pub fn battery_active(&self) -> bool {
        self.battery_active
    }

    /// The target currently requested by the policy; None means automatic mode.
    pub fn target(&self) -> Option<f32> {
        self.current
    }

    /// Unknown readings cannot count toward either continuous confirmation.
    /// Retain ownership reasons and the last request until healthy data returns.
    pub fn telemetry_gap(&mut self) {
        self.chip_below_since = None;
        self.battery_below_since = None;
        self.decline_since = None;
        self.ramp = None;
        self.last_demand = None;
    }

    fn curve(&self, temp: f32, activation: f32, full: f32) -> f32 {
        let fraction = (0.25 + 0.75 * (temp - activation) / (full - activation)).clamp(0.25, 1.0);
        (self.min + (self.max - self.min) * fraction)
            .round()
            .clamp(self.min, self.max)
    }

    /// Sample verified chip/battery temperatures using monotonic elapsed time.
    /// Chip demand activates at 55 C and releases after 30 continuous seconds
    /// below 50 C. Battery demand activates at 35 C and releases after 30
    /// continuous seconds below 35 C. Each active source retains a 25% floor.
    pub fn sample(&mut self, chip: f32, battery: f32, now: Duration) -> Result<Option<f32>> {
        if let Err(error) = validate_temperature("chip", chip, 130.0)
            .and_then(|()| validate_temperature("battery", battery, 80.0))
        {
            self.telemetry_gap();
            return Err(error);
        }
        if self.last_sample.is_some_and(|last| now < last) {
            self.telemetry_gap();
            return Err(Error::Invalid(
                "threshold policy time moved backwards".into(),
            ));
        }
        let previous = self.last_sample.replace(now).unwrap_or(now);
        update_reason(
            &mut self.chip_active,
            &mut self.chip_below_since,
            chip >= 55.0,
            chip < 50.0,
            now,
        );
        update_reason(
            &mut self.battery_active,
            &mut self.battery_below_since,
            battery >= 35.0,
            battery < 35.0,
            now,
        );
        let chip_demand = self.chip_active.then(|| self.curve(chip, 55.0, 70.0));
        let battery_demand = self.battery_active.then(|| self.curve(battery, 35.0, 40.0));
        let desired = match (chip_demand, battery_demand) {
            (Some(chip), Some(battery)) => Some(chip.max(battery)),
            (Some(demand), None) | (None, Some(demand)) => Some(demand),
            (None, None) => None,
        };
        let Some(desired) = desired else {
            // Release takes priority over target smoothing and write deadband.
            self.current = None;
            self.decline_since = None;
            self.ramp = None;
            self.last_demand = None;
            return Ok(None);
        };
        let prior_demand = self.last_demand.replace(desired);
        let Some(current) = self.current else {
            self.current = Some(desired);
            return Ok(self.current);
        };
        if desired >= current {
            self.decline_since = None;
            self.ramp = None;
            if desired - current >= WRITE_DEADBAND_RPM || desired == self.max {
                self.current = Some(desired);
            }
            return Ok(self.current);
        }
        if prior_demand.is_some_and(|prior| desired > prior) {
            // Warming breaks continuous cooling even when the existing fan
            // request is still higher than this warmer instantaneous demand.
            self.decline_since = None;
            self.ramp = None;
            return Ok(self.current);
        }
        let since = *self.decline_since.get_or_insert(now);
        let eligible = since.saturating_add(DECLINE_DELAY);
        if now < eligible {
            return Ok(self.current);
        }
        let elapsed = now.saturating_sub(previous.max(eligible));
        // Smooth the internal request instead of banking unused elapsed time.
        // It stops at the present demand; a later colder sample only receives
        // its own elapsed-time allowance. The deadband combines nearby writes.
        // A plateau hidden inside the write deadband carries no unused decline
        // allowance into a newly colder request.
        if prior_demand.is_some_and(|prior| desired < prior && self.ramp == Some(prior)) {
            self.ramp = Some(current);
        }
        let ramp = self.ramp.get_or_insert(current);
        *ramp = (*ramp - (self.max - self.min) * 0.05 * elapsed.as_secs_f32()).max(desired);
        let candidate = ramp.ceil().clamp(self.min, self.max);
        let floor = self.curve(55.0, 55.0, 70.0);
        if current - candidate >= WRITE_DEADBAND_RPM || candidate == floor {
            self.current = Some(candidate);
        }
        Ok(self.current)
    }
}

fn update_reason(
    active: &mut bool,
    below_since: &mut Option<Duration>,
    activate: bool,
    below_release: bool,
    now: Duration,
) {
    if activate {
        *active = true;
        *below_since = None;
    } else if *active && below_release {
        let since = *below_since.get_or_insert(now);
        if now.saturating_sub(since) >= RELEASE_DELAY {
            *active = false;
            *below_since = None;
        }
    } else {
        *below_since = None;
    }
}

fn validate_temperature(name: &str, value: f32, upper: f32) -> Result<()> {
    if !value.is_finite() || value <= 0.0 || value > upper {
        return Err(Error::Invalid(format!(
            "invalid {name} temperature {value} C"
        )));
    }
    Ok(())
}
