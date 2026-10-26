//! Bounded sessions with verified ownership and automatic restoration.
//! Retry/manual sequences adapt MacFanControl (de9af6f); the mode/Ftst
//! release order follows Stats (ffdf637). Observation and recovery protocols
//! are project implementations. See docs/PROVENANCE.md for precise scope.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::fan::{self, FanSpec, Plan};
use crate::smc::{Error, Result, Value, WriteSmc};

pub trait Clock {
    fn wait(&self, duration: Duration) -> bool;
    fn settle(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn now(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }
}

const WINDOW: Duration = Duration::from_secs(2);
const INTERVAL: Duration = Duration::from_millis(100);
const MAX_SAMPLES: usize = 21;
const STABLE_SAMPLES: usize = 3;
const TARGET_TOLERANCE: f32 = 0.25; // Unchanged from R1; only timing is changed.

fn byte(n: u8) -> Value {
    Value {
        kind: *b"ui8 ",
        bytes: vec![n],
    }
}

fn retry_write(
    smc: &mut impl WriteSmc,
    clock: &impl Clock,
    key: &str,
    value: &Value,
    restoring: bool,
    attempts: usize,
) -> Result<()> {
    let mut last = Error::Control("no write attempt".into());
    for attempt in 0..attempts {
        if !restoring {
            smc.trace().status()?;
            if !clock.wait(Duration::ZERO) {
                return Err(Error::Interrupted);
            }
        }
        smc.trace().event(
            "write_attempt",
            &format!("key={key} attempt={} restoring={restoring}", attempt + 1),
        );
        match smc.write(key, value) {
            Ok(()) => {
                smc.trace().event(
                    "write_acknowledged",
                    &format!("key={key} restoring={restoring}"),
                );
                return Ok(());
            }
            Err(
                e @ (Error::Permission(_)
                | Error::Invalid(_)
                | Error::Missing(_)
                | Error::Diagnostic(_)),
            ) => return Err(e),
            Err(e) => {
                smc.trace()
                    .event("write_rejected", &format!("key={key}: {e}"));
                last = e;
            }
        }
        if attempt + 1 < attempts {
            if restoring {
                clock.settle(INTERVAL);
            } else if !clock.wait(INTERVAL) {
                return Err(Error::Interrupted);
            }
        }
    }
    Err(last)
}

struct Observation {
    mode: Result<u32>,
    target: Result<f32>,
    actual: Result<f32>,
}

impl Observation {
    fn read(smc: &mut impl WriteSmc, fan: &FanSpec, stage: &str) -> Self {
        // Always attempt all three reads. A mode mismatch never hides target/actual.
        let mode = fan.mode(smc);
        let target = fan::rpm(smc, &fan.key("Tg"));
        let actual = fan::rpm(smc, &fan.key("Ac"));
        let observation = Self {
            mode,
            target,
            actual,
        };
        smc.trace().event(
            stage,
            &format!("fan={} {}", fan.index, observation.detail()),
        );
        observation
    }
    fn detail(&self) -> String {
        format!(
            "mode={:?} target={:?} actual={:?}",
            self.mode, self.target, self.actual
        )
    }
}

#[derive(Clone, Copy)]
enum Stage {
    ManualMode,
    Target(f32),
    Automatic,
}

fn observe(smc: &mut impl WriteSmc, fan: &FanSpec, clock: &impl Clock, stage: Stage) -> Result<()> {
    let (name, expected, restoring) = match stage {
        Stage::ManualMode => ("mode_observation", "mode=1".to_owned(), false),
        Stage::Target(rpm) => (
            "target_observation",
            format!("mode=1 target={rpm} tolerance={TARGET_TOLERANCE}"),
            false,
        ),
        Stage::Automatic => ("restore_observation", "mode=0 or 3".to_owned(), true),
    };
    let started = clock.now();
    let deadline = started + WINDOW;
    let mut consecutive = 0;
    let mut ever_expected = false;
    let mut last = String::from("no sample");
    for sample in 0..MAX_SAMPLES {
        if !restoring {
            smc.trace().status()?;
            if !clock.wait(Duration::ZERO) {
                return Err(Error::Interrupted);
            }
        }
        if sample > 0 && clock.now() > deadline {
            break;
        }
        let state = Observation::read(smc, fan, name);
        last = state.detail();
        if !restoring {
            smc.trace().status()?;
        }
        let matched = match stage {
            Stage::Automatic => matches!(state.mode, Ok(0 | 3)),
            Stage::ManualMode => {
                if ever_expected && state.mode.as_ref().is_ok_and(|m| *m != 1) {
                    return Err(Error::Control(format!(
                        "fan {}: mode changed after observing manual mode; {last}",
                        fan.index
                    )));
                }
                let mode_matches = matches!(state.mode, Ok(1));
                ever_expected |= mode_matches;
                mode_matches && state.target.is_ok() && state.actual.is_ok()
            }
            Stage::Target(rpm) => {
                if state.mode.as_ref().is_ok_and(|m| *m != 1) {
                    return Err(Error::Control(format!(
                        "fan {}: manual mode lost during target verification; expected {expected}; {last}",
                        fan.index
                    )));
                }
                let target_matches = state
                    .target
                    .as_ref()
                    .is_ok_and(|t| (*t - rpm).abs() <= TARGET_TOLERANCE);
                if ever_expected && state.target.is_ok() && !target_matches {
                    return Err(Error::Control(format!(
                        "fan {}: target changed after observing requested RPM; expected {expected}; {last}",
                        fan.index
                    )));
                }
                ever_expected |= target_matches;
                matches!(state.mode, Ok(1)) && target_matches && state.actual.is_ok()
            }
        };
        if matched {
            consecutive += 1;
        } else {
            consecutive = 0;
        }
        if consecutive == STABLE_SAMPLES && clock.now() <= deadline {
            smc.trace().event(
                "readback_verified",
                &format!(
                    "stage={name} fan={} expected={expected} consecutive={consecutive}; {last}",
                    fan.index
                ),
            );
            return Ok(());
        }
        let left = deadline.saturating_sub(clock.now());
        if sample + 1 == MAX_SAMPLES || left.is_zero() {
            break;
        }
        let delay = INTERVAL.min(left);
        if restoring {
            clock.settle(delay);
        } else if !clock.wait(delay) {
            return Err(Error::Interrupted);
        }
    }
    Err(Error::Control(format!(
        "fan {}: {name} timed out: observation budget 2000 ms / at most {MAX_SAMPLES} samples; elapsed_ms={}; expected {expected}, required {STABLE_SAMPLES} consecutive matches; {last}",
        fan.index,
        clock.now().saturating_sub(started).as_millis()
    )))
}

pub fn restore_all(smc: &mut impl WriteSmc, plan: &Plan, clock: &impl Clock) -> Result<()> {
    smc.trace().event(
        "restore_begin",
        "release modes; release validated Ftst; observe automatic ownership",
    );
    let mut problems = plan.problems.clone();
    for fan in &plan.fans {
        if let Err(e) = retry_write(smc, clock, &fan.mode_key, &byte(0), true, 3) {
            problems.push(format!("{} automatic: {e}", fan.mode_key));
        }
    }
    // Zero may fall below the reported hardware minimum. Release mode ownership
    // and let macOS choose its target (Stats' no-Ftst reset sequence).
    if plan.ftst
        && let Err(e) = retry_write(smc, clock, "Ftst", &byte(0), true, 3)
    {
        problems.push(format!("Ftst release: {e}"));
    }
    for fan in &plan.fans {
        if let Err(e) = observe(smc, fan, clock, Stage::Automatic) {
            problems.push(e.to_string());
        }
    }
    if plan.ftst {
        let mut verified = false;
        for attempt in 0..MAX_SAMPLES {
            match smc.read("Ftst").and_then(|v| v.integer()) {
                Ok(0) => {
                    verified = true;
                    break;
                }
                Ok(n) => smc.trace().event("restore_ftst", &format!("value={n}")),
                Err(e) => smc.trace().event("restore_ftst", &e.to_string()),
            }
            if attempt + 1 < MAX_SAMPLES {
                clock.settle(INTERVAL);
            }
        }
        if !verified {
            problems.push("Ftst release unverified after 21 observations".into());
        }
    }
    if problems.is_empty() {
        smc.trace().event(
            "restore_verified",
            if plan.ftst {
                "automatic mode observed in three consecutive samples; validated Ftst=0"
            } else {
                "automatic mode observed in three consecutive samples; Ftst absent"
            },
        );
        Ok(())
    } else {
        let detail = problems.join("; ");
        smc.trace().event("restore_failed", &detail);
        Err(Error::Control(detail))
    }
}

fn validate_target(fan: &FanSpec, target: f32, min: f32, max: f32) -> Result<()> {
    if !target.is_finite() || !(min..=max).contains(&target) {
        return Err(Error::Invalid(format!(
            "fan {}: requested target {target} outside hardware range {min}..{max}",
            fan.index
        )));
    }
    fan.encode_target(target)?;
    Ok(())
}

pub struct Session<'a, S: WriteSmc, C: Clock> {
    smc: &'a mut S,
    plan: Plan,
    clock: &'a C,
    armed: bool,
    minima: Vec<f32>,
    maxima: Vec<f32>,
    targets: Vec<f32>,
    baselines: Vec<f32>,
    convergence_logged: Vec<bool>,
}

impl<'a, S: WriteSmc, C: Clock> Session<'a, S, C> {
    pub fn new(smc: &'a mut S, plan: Plan, clock: &'a C) -> Self {
        Self {
            smc,
            plan,
            clock,
            armed: false,
            minima: Vec::new(),
            maxima: Vec::new(),
            targets: Vec::new(),
            baselines: Vec::new(),
            convergence_logged: Vec::new(),
        }
    }
    pub fn smc(&mut self) -> &mut S {
        self.smc
    }
    pub fn recovery_required(&self) -> bool {
        self.armed
    }

    pub fn engage_max(&mut self) -> Result<()> {
        self.engage(None)
    }

    pub fn engage_targets(&mut self, targets: &[f32]) -> Result<()> {
        self.engage(Some(targets))
    }

    fn engage(&mut self, requested: Option<&[f32]>) -> Result<()> {
        self.smc.trace().status()?;
        if self.armed {
            return Err(Error::Invalid("control session is already armed".into()));
        }
        if requested.is_some_and(|targets| targets.len() != self.plan.fans.len()) {
            return Err(Error::Invalid(
                "one target is required for every fan".into(),
            ));
        }
        if self.plan.fans.is_empty() || !self.plan.problems.is_empty() {
            return Err(Error::Invalid(format!(
                "fan controls unavailable: {}",
                self.plan.problems.join("; ")
            )));
        }
        self.minima.clear();
        self.maxima.clear();
        self.targets.clear();
        self.baselines.clear();
        self.convergence_logged.clear();
        for (position, fan) in self.plan.fans.iter().enumerate() {
            let state = fan::state(self.smc, fan)?;
            if !matches!(state.mode, 0 | 3) {
                return Err(Error::Invalid(format!(
                    "fan {} is already controlled (mode {}); stop other fan tools and run auto first",
                    fan.index, state.mode
                )));
            }
            let target = requested.map_or(state.max, |targets| targets[position]);
            validate_target(fan, target, state.min, state.max)?;
            self.smc.trace().event(
                "control_baseline",
                &format!(
                    "fan={} mode={} target={} actual={} min={} max={}",
                    fan.index, state.mode, state.target, state.actual, state.min, state.max
                ),
            );
            self.minima.push(state.min);
            self.maxima.push(state.max);
            self.targets.push(target);
            self.baselines.push(state.actual);
            self.convergence_logged.push(false);
        }
        if self.plan.ftst && self.smc.read("Ftst")?.integer()? != 0 {
            return Err(Error::Invalid(
                "Ftst is already active; stop other fan tools and run auto first".into(),
            ));
        }
        self.smc.trace().status()?;
        if !self.clock.wait(Duration::ZERO) {
            return Err(Error::Interrupted);
        }
        self.armed = true; // BEFORE the first possibly mutating call.
        for (position, (fan, target)) in self.plan.fans.iter().zip(&self.targets).enumerate() {
            if !self.clock.wait(Duration::ZERO) {
                return Err(Error::Interrupted);
            }
            match retry_write(self.smc, self.clock, &fan.mode_key, &byte(1), false, 1) {
                Ok(()) => (),
                Err(Error::Smc { code: 0x82, .. }) if self.plan.ftst => {
                    retry_write(self.smc, self.clock, "Ftst", &byte(1), false, 10)?;
                    if self.smc.read("Ftst")?.integer()? != 1 {
                        return Err(Error::Control("Ftst unlock readback mismatch".into()));
                    }
                    if !self.clock.wait(Duration::from_secs(3)) {
                        return Err(Error::Interrupted);
                    }
                    retry_write(self.smc, self.clock, &fan.mode_key, &byte(1), false, 30)?;
                }
                Err(e) => return Err(e),
            }
            self.smc.trace().event("mode_write_accepted", &fan.mode_key);
            observe(self.smc, fan, self.clock, Stage::ManualMode)?;
            self.smc
                .trace()
                .event("mode_readback_consistent", &fan.mode_key);
            // Refresh the actual range immediately before target write.
            let min = fan::rpm(self.smc, &fan.key("Mn"))?;
            let max = fan::rpm(self.smc, &fan.key("Mx"))?;
            if min != self.minima[position] || max != self.maxima[position] {
                return Err(Error::Control(format!(
                    "fan {}: hardware range changed before target write: {min}..{max}, requested {target}",
                    fan.index
                )));
            }
            if fan.mode(self.smc)? != 1 {
                return Err(Error::Control(format!(
                    "fan {}: manual mode lost before target write",
                    fan.index
                )));
            }
            // An ambiguous target-write failure ends the operation; no blind target
            // retry can overwrite a system target after ownership changes.
            retry_write(
                self.smc,
                self.clock,
                &fan.key("Tg"),
                &fan.encode_target(*target)?,
                false,
                1,
            )?;
            self.smc.trace().event(
                "target_write_accepted",
                &format!("key={} requested={target}", fan.key("Tg")),
            );
            observe(self.smc, fan, self.clock, Stage::Target(*target))?;
            self.smc
                .trace()
                .event("target_readback_consistent", &fan.key("Tg"));
        }
        self.smc.trace().status()?;
        Ok(())
    }

    /// Update targets once per changed fan. All targets and held ownership are
    /// validated before any write; a failure leaves recovery armed for the caller.
    pub fn update_targets(&mut self, targets: &[f32]) -> Result<()> {
        if !self.armed || targets.len() != self.plan.fans.len() {
            return Err(Error::Invalid(
                "armed session and one target per fan required".into(),
            ));
        }
        for (position, (fan, target)) in self.plan.fans.iter().zip(targets).enumerate() {
            validate_target(fan, *target, self.minima[position], self.maxima[position])?;
        }
        self.check_hold(false)?;
        for (position, (fan, target)) in self.plan.fans.iter().zip(targets).enumerate() {
            if *target == self.targets[position] {
                continue;
            }
            // Earlier fans may have taken time to settle. Recheck this fan just
            // before its single write, including its previously confirmed target.
            let state = fan::state(self.smc, fan)?;
            if state.mode != 1
                || (state.target - self.targets[position]).abs() > TARGET_TOLERANCE
                || state.min != self.minima[position]
                || state.max != self.maxima[position]
            {
                return Err(Error::Control(format!(
                    "fan {}: ownership or hardware range changed before target update: {state:?}",
                    fan.index
                )));
            }
            retry_write(
                self.smc,
                self.clock,
                &fan.key("Tg"),
                &fan.encode_target(*target)?,
                false,
                1,
            )?;
            self.smc.trace().event(
                "target_update_accepted",
                &format!(
                    "fan={} previous={} requested={target}",
                    fan.index, self.targets[position]
                ),
            );
            observe(self.smc, fan, self.clock, Stage::Target(*target))?;
            self.targets[position] = *target;
            self.baselines[position] = state.actual;
            self.convergence_logged[position] = false;
        }
        self.smc.trace().status()?;
        Ok(())
    }

    pub fn check_hold(&mut self, check_spinup: bool) -> Result<()> {
        self.smc.trace().status()?;
        if !self.armed {
            return Err(Error::Invalid("control session is not armed".into()));
        }
        for (position, (fan, target)) in self.plan.fans.iter().zip(&self.targets).enumerate() {
            let sample = Observation::read(self.smc, fan, "hold_observation");
            if sample.mode.as_ref().is_ok_and(|m| *m != 1) {
                let detail = format!(
                    "fan {}: previously verified manual mode lost; {}; external/system writer attribution requires separate evidence",
                    fan.index,
                    sample.detail()
                );
                self.smc.trace().event("control_lost_mode", &detail);
                return Err(Error::Control(detail));
            }
            if sample
                .target
                .as_ref()
                .is_ok_and(|t| (*t - target).abs() > TARGET_TOLERANCE)
            {
                let detail = format!(
                    "fan {}: previously verified target overwritten; expected {target}; {}",
                    fan.index,
                    sample.detail()
                );
                self.smc.trace().event("control_lost_target", &detail);
                return Err(Error::Control(detail));
            }
            sample.mode?;
            sample.target?;
            let actual = sample.actual?;
            let min_now = fan::rpm(self.smc, &fan.key("Mn"))?;
            let max_now = fan::rpm(self.smc, &fan.key("Mx"))?;
            if max_now != self.maxima[position]
                || min_now != self.minima[position]
                || actual > max_now * 1.2
            {
                return Err(Error::Control(format!(
                    "fan {}: hardware limits or actual RPM invalid/changed: min={min_now} max={max_now} actual={actual}",
                    fan.index
                )));
            }
            let initial_gap = (target - self.baselines[position]).abs();
            let gap = (target - actual).abs();
            if !self.convergence_logged[position] && initial_gap > 50.0 && gap + 50.0 <= initial_gap
            {
                self.smc.trace().event("actual_rpm_converging", &format!("fan={} baseline={} actual={actual} target={target} distance_decreased_by_at_least=50_RPM", fan.index, self.baselines[position]));
                self.convergence_logged[position] = true;
            }
            if !self.convergence_logged[position] && initial_gap <= 50.0 {
                self.smc.trace().event(
                    "actual_already_near_target",
                    &format!(
                        "fan={} baseline={} actual={actual} target={target}",
                        fan.index, self.baselines[position]
                    ),
                );
                self.convergence_logged[position] = true;
            }
            if check_spinup && actual < target * 0.8 {
                return Err(Error::Control(format!(
                    "fan {}: actual {actual:.0} RPM remains below 80% of {target:.0} after 30s",
                    fan.index
                )));
            }
        }
        self.smc.trace().status()?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        let result = match self.restore() {
            Ok(()) => Ok(()),
            Err(first) => self.restore().map_err(|last| {
                Error::Control(format!("initial recovery: {first}; final recovery: {last}"))
            }),
        };
        self.armed = false;
        result
    }
    pub fn restore(&mut self) -> Result<()> {
        if !self.armed {
            self.smc.trace().event(
                "restore_not_needed",
                "control never armed; no recovery performed or verified",
            );
            return Ok(());
        }
        let result = restore_all(self.smc, &self.plan, self.clock);
        if result.is_ok() {
            self.armed = false;
        }
        result
    }
}
impl<S: WriteSmc, C: Clock> Drop for Session<'_, S, C> {
    fn drop(&mut self) {
        if self.armed {
            let outcome = restore_all(self.smc, &self.plan, self.clock);
            let mut stderr = std::io::stderr().lock();
            match outcome {
                Ok(()) => {
                    let _ = writeln!(stderr, "m1fan: fallback restoration verified automatic");
                }
                Err(e) => {
                    let _ = writeln!(
                        stderr,
                        "m1fan: restoration FAILED: {e}; run sudo m1fan auto"
                    );
                }
            }
        }
    }
}
#[derive(Default)]
pub struct FailureWindow {
    consecutive: u8,
}
impl FailureWindow {
    pub fn record(&mut self, healthy: bool) -> Result<()> {
        self.consecutive = if healthy {
            0
        } else {
            self.consecutive.saturating_add(1)
        };
        if self.consecutive >= 3 {
            return Err(Error::Control(
                "three consecutive telemetry failures; restoring automatic mode".into(),
            ));
        }
        Ok(())
    }
}
