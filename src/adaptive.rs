//! Shared read-only preview and temperature-control orchestration.
use std::time::Duration;

use crate::chip_temps::{ChipReading, ChipTemperatureSource};
use crate::control::{Clock, FailureWindow, Session};
use crate::fan::{self, FanState, Plan};
use crate::smc::{Error, ReadSmc, Result, WriteSmc};
use crate::thermal::{SpinupWatchdog, ThermalPolicy};

const PERIOD: Duration = Duration::from_secs(1);
const MAX_GAP: Duration = Duration::from_secs(5);

pub struct Frame {
    pub elapsed: Duration,
    pub temperature: f32,
    pub hottest: String,
    pub readings: Vec<ChipReading>,
    pub fans: Vec<FanState>,
    pub requested: Vec<f32>,
    pub preview: bool,
}

impl Frame {
    pub fn line(&self) -> String {
        let mut line = format!(
            "t={:.1}s {} chip_max={:.1}C [{}]",
            self.elapsed.as_secs_f64(),
            if self.preview {
                "preview"
            } else {
                "temperature control"
            },
            self.temperature,
            self.hottest
        );
        for (fan, requested) in self.fans.iter().zip(&self.requested) {
            line.push_str(&format!(
                " | Fan {}: actual {:.0} target {:.0} {} {:.0} RPM {}",
                fan.index,
                fan.actual,
                fan.target,
                if self.preview {
                    "proposed"
                } else {
                    "requested"
                },
                requested,
                fan::mode_label(fan.mode)
            ));
        }
        line
    }
}

/// A failed output, log or startup handshake is a normal error and is cleaned up.
pub trait Observer {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn ready(&mut self, frame: &Frame) -> Result<()> {
        self.frame(frame)
    }
    fn frame(&mut self, frame: &Frame) -> Result<()>;
    fn event(&mut self, message: &str) -> Result<()>;
}

fn hottest(readings: &[ChipReading]) -> Result<(f32, String)> {
    let mut maximum: Option<&ChipReading> = None;
    for reading in readings {
        if !reading.celsius.is_finite() || reading.celsius <= 0.0 || reading.celsius > 130.0 {
            return Err(Error::Invalid(format!(
                "{}: invalid chip temperature {}",
                reading.name, reading.celsius
            )));
        }
        if maximum.is_none_or(|old| reading.celsius > old.celsius) {
            maximum = Some(reading);
        }
    }
    maximum
        .map(|r| (r.celsius, r.name.clone()))
        .ok_or_else(|| Error::Missing("no confirmed chip temperatures".into()))
}

fn states(smc: &mut impl ReadSmc, plan: &Plan) -> Result<Vec<FanState>> {
    if plan.fans.is_empty() || !plan.problems.is_empty() {
        return Err(Error::Invalid(format!(
            "fan discovery incomplete: {:?}",
            plan.problems
        )));
    }
    plan.fans.iter().map(|fan| fan::state(smc, fan)).collect()
}

fn policies(fans: &[FanState]) -> Result<Vec<ThermalPolicy>> {
    fans.iter()
        .map(|f| ThermalPolicy::new(f.min, f.max))
        .collect()
}

fn targets(policies: &mut [ThermalPolicy], temp: f32, now: Duration) -> Result<Vec<f32>> {
    policies.iter_mut().map(|p| p.sample(temp, now)).collect()
}

fn check_gap(previous: Duration, now: Duration) -> Result<()> {
    if now < previous || now - previous > MAX_GAP {
        return Err(Error::Control("sampling gap exceeded 5s (sleep, scheduling delay or blocked I/O); release automatic ownership and manually restart after wake".into()));
    }
    Ok(())
}

/// This entry point cannot write: its only SMC capability is ReadSmc.
pub fn preview(
    smc: &mut impl ReadSmc,
    plan: &Plan,
    source: &mut impl ChipTemperatureSource,
    clock: &impl Clock,
    samples: Option<usize>,
    observer: &mut impl Observer,
) -> Result<()> {
    let first_fans = states(smc, plan)?;
    let mut policies = policies(&first_fans)?;
    let started = clock.now();
    let mut previous = started;
    let mut count = 0;
    loop {
        if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
            break;
        }
        let cycle = clock.now();
        check_gap(previous, cycle)?;
        let readings = source.sample()?;
        let (temperature, hottest) = hottest(&readings)?;
        let fans = states(smc, plan)?;
        for (initial, current) in first_fans.iter().zip(&fans) {
            if initial.min != current.min || initial.max != current.max {
                return Err(Error::Control(
                    "hardware bounds changed during preview".into(),
                ));
            }
        }
        check_gap(cycle, clock.now())?;
        let requested = targets(&mut policies, temperature, clock.now())?;
        let frame = Frame {
            elapsed: clock.now() - started,
            temperature,
            hottest,
            readings,
            fans,
            requested,
            preview: true,
        };
        if count == 0 {
            observer.ready(&frame)?;
        } else {
            observer.frame(&frame)?;
        }
        count += 1;
        if samples.is_some_and(|limit| count >= limit) {
            break;
        }
        previous = cycle;
        if !clock.wait(PERIOD.saturating_sub(clock.now() - cycle)) {
            break;
        }
    }
    Ok(())
}

/// Successful shutdown distinguishes untouched hardware from verified recovery.
#[derive(Debug, PartialEq, Eq)]
pub enum Completion {
    NoWrites,
    Restored,
}

fn verify_held(fans: &[FanState], initial: &[FanState], requested: &[f32]) -> Result<()> {
    if fans.len() != initial.len() || fans.len() != requested.len() {
        return Err(Error::Control(
            "fan snapshot count changed while controlling".into(),
        ));
    }
    for ((fan, baseline), target) in fans.iter().zip(initial).zip(requested) {
        if fan.index != baseline.index
            || fan.min != baseline.min
            || fan.max != baseline.max
            || fan.mode != 1
            || (fan.target - target).abs() > 0.25
        {
            return Err(Error::Control(format!(
                "fan {}: held state changed: mode={} target={} expected_target={} min={} max={} expected_range={}..{}",
                fan.index,
                fan.mode,
                fan.target,
                target,
                fan.min,
                fan.max,
                baseline.min,
                baseline.max
            )));
        }
    }
    Ok(())
}

fn telemetry_failure(
    smc: &mut impl ReadSmc,
    policies: &mut [ThermalPolicy],
    failures: &mut FailureWindow,
    observer: &mut impl Observer,
    message: &str,
) -> Result<()> {
    for policy in policies {
        policy.telemetry_gap();
    }
    smc.trace().event("thermal_sample_failed", message);
    observer.event(message)?;
    failures.record(false)
}

/// Every failure after the first possible write passes through Session::finish.
/// A panic still has Session's Drop restoration guard.
pub fn run(
    smc: &mut impl WriteSmc,
    plan: &Plan,
    source: &mut impl ChipTemperatureSource,
    clock: &impl Clock,
    observer: &mut impl Observer,
) -> Result<Completion> {
    let initial = states(smc, plan)?;
    let mut policies = policies(&initial)?;
    let started = clock.now();
    let readings = source.sample()?;
    let (temperature, hottest_name) = hottest(&readings)?;
    check_gap(started, clock.now())?;
    let mut requested = targets(&mut policies, temperature, clock.now())?;
    let mut session = Session::new(smc, plan.clone(), clock);
    let operation = (|| {
        if observer.stop_requested()? {
            return Err(Error::Interrupted);
        }
        session.engage_targets(&requested)?;
        check_gap(started, clock.now())?;
        let mut watchdogs = requested
            .iter()
            .map(|&rpm| SpinupWatchdog::new(clock.now(), rpm))
            .collect::<Result<Vec<_>>>()?;
        let fans = states(session.smc(), plan)?;
        verify_held(&fans, &initial, &requested)?;
        check_gap(started, clock.now())?;
        let frame = Frame {
            elapsed: clock.now() - started,
            temperature,
            hottest: hottest_name,
            readings,
            fans,
            requested: requested.clone(),
            preview: false,
        };
        observer.ready(&frame)?;
        check_gap(started, clock.now())?;
        let mut failures = FailureWindow::default();
        let mut previous = clock.now();
        let mut next_tick = previous + PERIOD;
        while clock.wait(next_tick.saturating_sub(clock.now())) {
            if observer.stop_requested()? {
                break;
            }
            let cycle = clock.now();
            check_gap(previous, cycle)?;
            previous = cycle;
            next_tick = cycle + PERIOD;
            let held = session.check_hold(false);
            if let Err(e @ (Error::Control(_) | Error::Diagnostic(_))) = &held {
                return Err(e.clone());
            }
            // Attempt both sources so failures cannot conceal loss of fan ownership.
            let temperatures = source.sample().and_then(|values| {
                let (max, name) = hottest(&values)?;
                Ok((values, max, name))
            });
            let fans = states(session.smc(), plan);
            check_gap(cycle, clock.now())?;
            // A second snapshot may reveal takeover after check_hold, including
            // when an unrelated temperature/first read failed during this cycle.
            if let Ok(fans) = &fans {
                verify_held(fans, &initial, &requested)?;
            }
            match (held, temperatures, fans) {
                (Ok(()), Ok((readings, temperature, hottest)), Ok(mut fans)) => {
                    for ((watchdog, fan), &rpm) in watchdogs.iter_mut().zip(&fans).zip(&requested) {
                        watchdog.sample(clock.now(), rpm, fan.actual)?;
                    }
                    let proposed = targets(&mut policies, temperature, clock.now())?;
                    if proposed != requested {
                        session.update_targets(&proposed)?;
                        requested = proposed;
                        let changed_at = clock.now();
                        check_gap(cycle, changed_at)?;
                        for ((watchdog, fan), &rpm) in
                            watchdogs.iter_mut().zip(&fans).zip(&requested)
                        {
                            watchdog.sample(changed_at, rpm, fan.actual)?;
                        }
                        // Only a changed target needs a new snapshot. A transient
                        // read failure consumes the same consecutive-failure budget.
                        let refreshed = states(session.smc(), plan);
                        check_gap(cycle, clock.now())?;
                        match refreshed {
                            Ok(refreshed) => {
                                verify_held(&refreshed, &initial, &requested)?;
                                fans = refreshed;
                            }
                            Err(e @ (Error::Control(_) | Error::Diagnostic(_))) => return Err(e),
                            Err(error) => {
                                telemetry_failure(
                                    session.smc(),
                                    &mut policies,
                                    &mut failures,
                                    observer,
                                    &format!("post-update telemetry unavailable: {error}"),
                                )?;
                                continue;
                            }
                        }
                    }
                    let frame = Frame {
                        elapsed: clock.now() - started,
                        temperature,
                        hottest,
                        readings,
                        fans,
                        requested: requested.clone(),
                        preview: false,
                    };
                    session.smc().trace().event("thermal_sample", &frame.line());
                    observer.frame(&frame)?;
                    // A preliminary healthy read must not reset failures before
                    // the post-update read has also succeeded.
                    failures.record(true)?;
                }
                (held, temperatures, fans) => {
                    let message = format!(
                        "telemetry unavailable: hold={held:?}; temperatures={:?}; fans={:?}",
                        temperatures.err(),
                        fans.err()
                    );
                    telemetry_failure(
                        session.smc(),
                        &mut policies,
                        &mut failures,
                        observer,
                        &message,
                    )?;
                }
            }
        }
        Ok(())
    })();
    session
        .smc()
        .trace()
        .event("thermal_control_end", &format!("result={operation:?}"));
    let armed = session.recovery_required();
    let recovery = session.finish();
    let recovered = recovery.is_ok();
    let outcome = match (operation, recovery) {
        (Ok(()) | Err(Error::Interrupted), Ok(())) => Ok(if armed {
            Completion::Restored
        } else {
            Completion::NoWrites
        }),
        (Err(e), Ok(())) => Err(e),
        (operation, Err(recovery)) => Err(Error::Control(format!(
            "operation={operation:?}; automatic restoration FAILED: {recovery}; run sudo m1fan auto"
        ))),
    };
    let message = if !armed {
        "control was not started; no SMC writes performed"
    } else if recovered {
        "automatic restoration verified"
    } else {
        "automatic restoration FAILED; run sudo m1fan auto"
    };
    // Recovery reporting must reflect the explicit cleanup result, never the armed flag alone.
    match observer.event(message) {
        Ok(()) => outcome,
        Err(output) => match outcome {
            Ok(_) => Err(output),
            Err(e) => Err(Error::Io(format!("{e}; output: {output}"))),
        },
    }
}
