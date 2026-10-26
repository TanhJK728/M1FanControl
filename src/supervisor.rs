//! Threshold supervision: automatic monitoring, manual cooling, latched fault.
//! The only write capability is guarded by Session; faults never reacquire control.
use crate::battery::{self, BatteryReading};
use crate::chip_temps::{ChipReading, ChipTemperatureSource};
use crate::control::{Clock, FailureWindow, Session};
use crate::demand::ThresholdPolicy;
use crate::fan::{self, FanState, Plan};
use crate::smc::{Error, ReadSmc, Result, WriteSmc};
use crate::thermal::SpinupWatchdog;
use std::cell::Cell;
use std::time::Duration;

const PERIOD: Duration = Duration::from_secs(1);
const MAX_GAP: Duration = Duration::from_secs(5);

pub struct Frame {
    pub elapsed: Duration,
    pub readings: Vec<ChipReading>,
    pub battery: Vec<BatteryReading>,
    pub fans: Vec<FanState>,
    pub requested: Option<Vec<f32>>,
    pub state: String,
    pub problem: Option<String>,
}
impl Frame {
    pub fn line(&self) -> String {
        let chip = chip_max(&self.readings)
            .map(|(t, n)| format!("{t:.1}C [{n}]"))
            .unwrap_or_else(|_| "unavailable".into());
        let bat = self
            .battery
            .iter()
            .map(|r| format!("{}={:.1}C", r.key, r.celsius))
            .collect::<Vec<_>>()
            .join(" ");
        let mut line = format!(
            "t={:.1}s {} | chip_max={} | battery {}",
            self.elapsed.as_secs_f64(),
            self.state,
            chip,
            if bat.is_empty() { "unavailable" } else { &bat }
        );
        for (i, f) in self.fans.iter().enumerate() {
            let request = self
                .requested
                .as_ref()
                .and_then(|v| v.get(i))
                .map(|n| format!("{n:.0}"))
                .unwrap_or_else(|| "system".into());
            line.push_str(&format!(
                " | Fan {}: actual {:.0} target {:.0} requested {} RPM {}",
                f.index,
                f.actual,
                f.target,
                request,
                fan::mode_label(f.mode)
            ));
        }
        if let Some(problem) = &self.problem {
            line.push_str(&format!(" | {problem}"));
        }
        line
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Engaged,
    Released,
}

pub trait Observer {
    /// Persist recovery responsibility before the first possible manual write.
    /// An error aborts engagement through the normal cleanup/finished path.
    fn arming(&mut self) -> Result<()> {
        Ok(())
    }
    /// Called immediately after a verified ownership transition.
    fn transition(&mut self, _kind: Transition) -> Result<()> {
        Ok(())
    }
    /// Receives the typed fault and the completed bounded recovery result.
    /// Recovery failure must remain visible to any outer reconnect supervisor.
    fn fault(&mut self, _error: &Error, _recovery: &Result<()>) -> Result<()> {
        Ok(())
    }
    /// Receives final automatic-ownership verification, independently of whether
    /// the operation or its output failed. This runs only after cleanup.
    fn finished(&mut self, _recovery: &Result<()>) -> Result<()> {
        Ok(())
    }
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn ready(&mut self, frame: &Frame) -> Result<()> {
        self.frame(frame)
    }
    fn frame(&mut self, frame: &Frame) -> Result<()>;
    fn event(&mut self, message: &str) -> Result<()>;
}
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    AutomaticVerified,
}

fn chip_max(readings: &[ChipReading]) -> Result<(f32, String)> {
    let mut max: Option<&ChipReading> = None;
    for r in readings {
        if !r.celsius.is_finite() || r.celsius <= 0. || r.celsius > 130. {
            return Err(Error::Invalid(format!(
                "{} invalid chip temperature {}",
                r.name, r.celsius
            )));
        }
        if max.is_none_or(|v| r.celsius > v.celsius) {
            max = Some(r);
        }
    }
    max.map(|r| (r.celsius, r.name.clone()))
        .ok_or_else(|| Error::Missing("chip temperatures".into()))
}
fn states(smc: &mut impl ReadSmc, plan: &Plan) -> Result<Vec<FanState>> {
    if plan.fans.is_empty() || !plan.problems.is_empty() {
        return Err(Error::Control(format!(
            "fan discovery incomplete: {:?}",
            plan.problems
        )));
    }
    plan.fans.iter().map(|f| fan::state(smc, f)).collect()
}
fn verify(fans: &[FanState], initial: &[FanState], requested: Option<&[f32]>) -> Result<()> {
    if fans.len() != initial.len() || requested.is_some_and(|r| r.len() != fans.len()) {
        return Err(Error::Control("fan count changed".into()));
    }
    for (i, (f, old)) in fans.iter().zip(initial).enumerate() {
        if f.index != old.index || f.min != old.min || f.max != old.max {
            return Err(Error::Control("fan identity or bounds changed".into()));
        }
        let valid = match requested {
            Some(v) => f.mode == 1 && (f.target - v[i]).abs() <= 0.25,
            None => matches!(f.mode, 0 | 3),
        };
        if !valid {
            return Err(Error::Control(format!(
                "fan {} ownership changed: mode={} target={} expected={:?}",
                f.index, f.mode, f.target, requested
            )));
        }
    }
    Ok(())
}
// Abort compound engagement before its next write after a long I/O/sleep gap.
// Restoration uses settle(), so cancellation never disables recovery.
struct ControlClock<'a, C: Clock> {
    inner: &'a C,
    cycle: Cell<Duration>,
}
impl<C: Clock> Clock for ControlClock<'_, C> {
    fn now(&self) -> Duration {
        self.inner.now()
    }
    fn wait(&self, d: Duration) -> bool {
        gap(self.cycle.get(), self.now()).is_ok()
            && self.inner.wait(d)
            && gap(self.cycle.get(), self.now()).is_ok()
    }
    fn settle(&self, d: Duration) {
        self.inner.settle(d);
    }
}
fn control_step<T>(result: Result<T>, cycle: Duration, clock: &impl Clock) -> Result<T> {
    gap(cycle, clock.now())?;
    result.map_err(|e| match e {
        Error::Interrupted => Error::Interrupted,
        _ => Error::Control(format!("control operation failed: {e}")),
    })
}
fn gap(previous: Duration, now: Duration) -> Result<()> {
    if now < previous || now - previous > MAX_GAP {
        return Err(Error::Control(
            "sampling gap exceeded 5s (including sleep); control latched off until stop/start"
                .into(),
        ));
    }
    Ok(())
}
fn policies(initial: &[FanState]) -> Result<Vec<ThresholdPolicy>> {
    initial
        .iter()
        .map(|f| ThresholdPolicy::new(f.min, f.max))
        .collect()
}
fn proposed(
    policies: &mut [ThresholdPolicy],
    chip: f32,
    bat: f32,
    now: Duration,
) -> Result<Option<Vec<f32>>> {
    let all = policies
        .iter_mut()
        .map(|p| p.sample(chip, bat, now))
        .collect::<Result<Vec<_>>>()?;
    if all.iter().all(Option::is_none) {
        return Ok(None);
    }
    all.into_iter()
        .collect::<Option<Vec<_>>>()
        .map(Some)
        .ok_or_else(|| Error::Control("inconsistent fan demand state".into()))
}
fn readings(
    source: &mut impl ChipTemperatureSource,
    smc: &mut impl ReadSmc,
    plan: &Plan,
    elapsed: Duration,
) -> Result<Frame> {
    // Read fan state even when another source fails: callers also check ownership.
    let chip = source.sample().and_then(|r| chip_max(&r).map(|_| r));
    let battery = battery::sample(smc);
    let fans = states(smc, plan);
    Ok(Frame {
        elapsed,
        readings: chip?,
        battery: battery?,
        fans: fans?,
        requested: None,
        state: String::new(),
        problem: None,
    })
}
pub fn verify_automatic(smc: &mut impl ReadSmc, plan: &Plan, clock: &impl Clock) -> Result<()> {
    if plan.fans.is_empty() || !plan.problems.is_empty() {
        return Err(Error::Control(
            "automatic verification requires complete fan discovery".into(),
        ));
    }
    for n in 0..3 {
        // Recovery ownership does not depend on a working tachometer or bounds.
        for spec in &plan.fans {
            let mode = spec.mode(smc)?;
            if !matches!(mode, 0 | 3) {
                return Err(Error::Control(format!(
                    "{} automatic mode unverified: {mode}",
                    spec.mode_key
                )));
            }
        }
        if plan.ftst && smc.read("Ftst")?.integer()? != 0 {
            return Err(Error::Control("Ftst remains active".into()));
        }
        if n < 2 {
            clock.settle(Duration::from_millis(100));
        }
    }
    Ok(())
}
/// Compile-time read-only preview of the exact new policy, with real telemetry.
pub fn preview(
    smc: &mut impl ReadSmc,
    plan: &Plan,
    source: &mut impl ChipTemperatureSource,
    clock: &impl Clock,
    samples: Option<usize>,
    observer: &mut impl Observer,
) -> Result<()> {
    let initial = states(smc, plan)?;
    let mut policies = policies(&initial)?;
    let started = clock.now();
    let mut previous = started;
    let mut count = 0;
    loop {
        if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
            break;
        }
        let cycle = clock.now();
        gap(previous, cycle)?;
        let mut frame = readings(source, smc, plan, cycle - started)?;
        // Preview accepts existing manual ownership without touching it.
        for (old, f) in initial.iter().zip(&frame.fans) {
            if old.min != f.min || old.max != f.max {
                return Err(Error::Control("fan bounds changed".into()));
            }
        }
        gap(cycle, clock.now())?;
        frame.requested = proposed(
            &mut policies,
            chip_max(&frame.readings)?.0,
            battery::hottest(&frame.battery)?.0,
            clock.now(),
        )?;
        frame.state = if frame.requested.is_some() {
            "preview: cooling requested"
        } else {
            "preview: macOS automatic requested"
        }
        .into();
        if count == 0 {
            observer.ready(&frame)?;
        } else {
            observer.frame(&frame)?;
        }
        smc.trace().event("supervisor_preview", &frame.line());
        count += 1;
        if samples.is_some_and(|n| count >= n) {
            break;
        }
        previous = cycle;
        if !clock.wait(PERIOD.saturating_sub(clock.now() - cycle)) {
            break;
        }
    }
    Ok(())
}

/// Faults restore once (bounded Session recovery), then only monitor until stopped.
pub fn run(
    smc: &mut impl WriteSmc,
    plan: &Plan,
    source: &mut impl ChipTemperatureSource,
    clock: &impl Clock,
    observer: &mut impl Observer,
) -> Result<Completion> {
    let initial = states(smc, plan)?;
    let mut policies = policies(&initial)?;
    let control_clock = ControlClock {
        inner: clock,
        cycle: Cell::new(clock.now()),
    };
    let mut session = Session::new(smc, plan.clone(), &control_clock);
    let started = clock.now();
    let mut previous = started;
    let mut requested: Option<Vec<f32>> = None;
    let mut watchdogs: Vec<SpinupWatchdog> = Vec::new();
    let mut failures = FailureWindow::default();
    let mut fault: Option<String> = None;
    let mut ready = false;
    let operation = (|| {
        loop {
            if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
                break;
            }
            let cycle = clock.now();
            control_clock.cycle.set(cycle);
            let outcome = (|| {
                if fault.is_none() {
                    gap(previous, cycle)?;
                }
                let hold = if session.recovery_required() {
                    session.check_hold(false)
                } else {
                    Ok(())
                };
                let chip = source.sample().and_then(|r| chip_max(&r).map(|_| r));
                let battery = battery::sample(session.smc());
                // Preserve and inspect this result before propagating temperature errors.
                let fans = states(session.smc(), plan);
                session.smc().trace().status()?;
                if fault.is_none() {
                    if let Err(e @ (Error::Control(_) | Error::Diagnostic(_))) = &hold {
                        return Err(e.clone());
                    }
                    if let Ok(f) = &fans {
                        verify(f, &initial, requested.as_deref())?;
                    }
                    gap(cycle, clock.now())?;
                    hold?;
                }
                let mut frame = Frame {
                    elapsed: cycle - started,
                    readings: chip?,
                    battery: battery?,
                    fans: fans?,
                    requested: None,
                    state: String::new(),
                    problem: None,
                };
                if let Some(reason) = &fault {
                    frame.state = "FAULT LATCHED; monitoring only; restart required".into();
                    frame.problem = Some(reason.clone());
                    return Ok(frame);
                }
                if let Some(old) = &requested {
                    for ((w, f), &rpm) in watchdogs.iter_mut().zip(&frame.fans).zip(old) {
                        w.sample(clock.now(), rpm, f.actual)?;
                    }
                }
                let next = proposed(
                    &mut policies,
                    chip_max(&frame.readings)?.0,
                    battery::hottest(&frame.battery)?.0,
                    clock.now(),
                )?;
                match (&requested, &next) {
                    (None, Some(target)) => {
                        if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
                            return Err(Error::Interrupted);
                        }
                        control_step(observer.arming(), cycle, clock)?;
                        control_step(session.engage_targets(target), cycle, clock)?;
                        control_step(observer.transition(Transition::Engaged), cycle, clock)?;
                        watchdogs = target
                            .iter()
                            .map(|&v| SpinupWatchdog::new(clock.now(), v))
                            .collect::<Result<_>>()?;
                        control_step(
                            observer.event("manual cooling engaged; mode and targets verified"),
                            cycle,
                            clock,
                        )?;
                    }
                    (Some(_), None) => {
                        control_step(session.restore(), cycle, clock)?;
                        control_step(observer.transition(Transition::Released), cycle, clock)?;
                        watchdogs.clear();
                        control_step(observer.event("cooling requirements cleared; automatic restoration verified; monitoring continues"),cycle,clock)?;
                    }
                    (Some(old), Some(target)) => {
                        if old != target {
                            control_step(session.update_targets(target), cycle, clock)?;
                        }
                    }
                    (None, None) => {}
                }
                let changed = requested != next;
                requested = next;
                if changed {
                    let refreshed = states(session.smc(), plan);
                    gap(cycle, clock.now())?;
                    frame.fans = refreshed?;
                }
                verify(&frame.fans, &initial, requested.as_deref())?;
                gap(cycle, clock.now())?;
                if let Some(target) = &requested {
                    for ((w, f), &rpm) in watchdogs.iter_mut().zip(&frame.fans).zip(target) {
                        w.sample(clock.now(), rpm, f.actual)?;
                    }
                }
                frame.requested = requested.clone();
                frame.state = if requested.is_some() {
                    format!(
                        "manual cooling (chip={} battery={})",
                        policies[0].chip_active(),
                        policies[0].battery_active()
                    )
                } else {
                    "automatic; monitoring".into()
                };
                Ok(frame)
            })();
            let frame = match outcome {
                Ok(frame) => {
                    if fault.is_none() {
                        failures.record(true)?;
                    }
                    frame
                }
                Err(Error::Interrupted) => break,
                Err(error) => {
                    for p in &mut policies {
                        p.telemetry_gap();
                    }
                    let fatal = matches!(error, Error::Control(_) | Error::Diagnostic(_))
                        || failures.record(false).is_err();
                    if fault.is_none() && fatal {
                        let armed = session.recovery_required();
                        let recovery = session.finish();
                        observer.fault(&error, &recovery)?;
                        requested = None;
                        watchdogs.clear();
                        let message = match recovery {
                            Ok(()) => format!(
                                "FAULT LATCHED: {error}; {}; restart required",
                                if armed {
                                    "automatic restoration verified"
                                } else {
                                    "control was not armed; hardware will be checked on stop"
                                }
                            ),
                            Err(e) => {
                                format!(
                                    "FAULT LATCHED: {error}; automatic restoration FAILED: {e}; run auto after stopping"
                                )
                            }
                        };
                        session.smc().trace().event("supervisor_fault", &message);
                        fault = Some(message.clone());
                        observer.event(&message)?;
                    }
                    Frame {
                        elapsed: clock.now() - started,
                        readings: vec![],
                        battery: vec![],
                        fans: vec![],
                        requested: requested.clone(),
                        state: if fault.is_some() {
                            "FAULT LATCHED; monitoring only"
                        } else {
                            "telemetry unavailable; retaining last verified target"
                        }
                        .into(),
                        problem: Some(format!(
                            "{}; {error}",
                            fault.as_deref().unwrap_or("transient read failure")
                        )),
                    }
                }
            };
            session
                .smc()
                .trace()
                .event("supervisor_sample", &frame.line());
            if !ready && frame.problem.is_none() {
                observer.ready(&frame)?;
                ready = true;
            } else {
                observer.frame(&frame)?;
                if !ready && let Some(reason) = &fault {
                    return Err(Error::Control(format!("startup failed: {reason}")));
                }
            }
            previous = cycle;
            if !clock.wait(PERIOD.saturating_sub(clock.now().saturating_sub(cycle))) {
                break;
            }
        }
        Ok(())
    })();
    let was_armed = session.recovery_required();
    let recovery = session.finish();
    // Always verify the current ownership, even if an earlier recovery failed.
    let verified = verify_automatic(session.smc(), plan, clock);
    let notified = observer.finished(&verified);
    let message = if verified.is_ok() {
        if was_armed {
            "automatic restoration verified"
        } else {
            "automatic mode verified; supervisor stopped"
        }
    } else {
        "automatic mode verification FAILED; run sudo m1fan auto"
    };
    let report = observer.event(message);
    session.smc().trace().event(
        "supervisor_stop",
        &format!("operation={operation:?}; recovery={recovery:?}; verification={verified:?}"),
    );
    let outcome = match (operation, verified, report, recovery) {
        (_, Err(e), _, _) => Err(Error::Control(format!(
            "automatic verification failed: {e}"
        ))),
        (Err(e), _, _, _) => Err(e),
        (_, _, Err(e), _) => Err(e),
        (_, _, _, Err(e)) => Err(Error::Control(format!(
            "recovery reported an error, but current automatic mode verified: {e}"
        ))),
        _ => Ok(Completion::AutomaticVerified),
    };
    match (outcome, notified) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(hook)) => Err(hook),
        (Err(error), Err(hook)) => Err(Error::Control(format!(
            "{error}; final verification hook failed: {hook}"
        ))),
    }
}
