//! Reconnect supervisor around the verified v0.4 control session.
//! All SMC/HID objects remain on this thread; the power thread carries only events.
use crate::battery::{self, BatteryReading};
use crate::chip_temps::{ChipReading, ChipTemperatureSource, HidTemperatures, SensorGroup};
use crate::control::{Clock, restore_all};
use crate::fan::{self, FanState, Plan};
use crate::power::{PowerEvent, PowerSource};
use crate::service_smc::{self, IntentSmc};
use crate::smc::{Error, NativeSmc, ReadSmc, Result, WriteSmc};
use crate::supervisor::{self, Completion, Frame, Transition};
use crate::trace::Trace;
use std::cell::Cell;
use std::time::Duration;

pub trait Factory {
    type Smc: WriteSmc;
    type Source: ChipTemperatureSource;
    fn smc(&mut self) -> Result<Self::Smc>;
    /// Must return a newly discovered source that rejects pre-open/stale events.
    fn source(&mut self) -> Result<Self::Source>;
}
struct TrackedSmc<'a, S: WriteSmc> {
    inner: S,
    wrote: &'a Cell<bool>,
}
impl<S: WriteSmc> ReadSmc for TrackedSmc<'_, S> {
    fn read(&mut self, k: &str) -> Result<crate::smc::Value> {
        self.inner.read(k)
    }
    fn keys(&mut self) -> Result<Vec<String>> {
        self.inner.keys()
    }
    fn trace(&self) -> Trace {
        self.inner.trace()
    }
}
impl<S: WriteSmc> WriteSmc for TrackedSmc<'_, S> {
    fn write(&mut self, k: &str, v: &crate::smc::Value) -> Result<()> {
        self.wrote.set(true);
        self.inner.write(k, v)
    }
}
struct ConsistentSource<'a, S> {
    inner: S,
    selected: &'a mut Option<Vec<(String, SensorGroup)>>,
}
impl<S: ChipTemperatureSource> ChipTemperatureSource for ConsistentSource<'_, S> {
    fn sample(&mut self) -> Result<Vec<ChipReading>> {
        let readings = self.inner.sample()?;
        let mut names = readings
            .iter()
            .map(|r| (r.name.clone(), r.group))
            .collect::<Vec<_>>();
        names.sort_by(|a, b| a.0.cmp(&b.0));
        if names.is_empty() || names.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err(Error::Invalid("empty or duplicate chip sensor set".into()));
        }
        if let Some(known) = &self.selected {
            if names != *known {
                return Err(Error::Invalid(format!(
                    "chip sensor set differs across reconnect: expected={known:?}, received={names:?}"
                )));
            }
        } else {
            *self.selected = Some(names);
        }
        Ok(readings)
    }
}
pub struct NativeFactory {
    pub trace: Trace,
    pub service: bool,
}
impl Factory for NativeFactory {
    type Smc = IntentSmc<NativeSmc, fn() -> Result<bool>>;
    type Source = HidTemperatures;
    fn smc(&mut self) -> Result<Self::Smc> {
        let mut s = NativeSmc::open_control()?;
        s.set_trace(self.trace.clone());
        let enabled: fn() -> Result<bool> = if self.service {
            service_smc::installed_enabled
        } else {
            || Ok(true)
        };
        Ok(IntentSmc::new(s, enabled))
    }
    fn source(&mut self) -> Result<HidTemperatures> {
        HidTemperatures::open_fresh()
    }
}
#[derive(Clone, Debug)]
pub struct Status {
    pub uptime: Duration,
    pub phase: String,
    pub chip: Option<f32>,
    pub battery: Vec<BatteryReading>,
    pub fans: Vec<FanState>,
    pub requested: Option<Vec<f32>>,
    pub sample_age: Option<Duration>,
    pub last_takeover: String,
    pub last_release: String,
    pub last_power: String,
    pub errors: u64,
    pub last_error: String,
    pub retry_level: usize,
    pub retry_in: Option<Duration>,
    pub recovery_pending: bool,
    pub automatic_verified: bool,
    sampled_at: Option<Duration>,
}
impl Status {
    fn new(pending: bool) -> Self {
        Self {
            uptime: Duration::ZERO,
            phase: "starting".into(),
            chip: None,
            battery: vec![],
            fans: vec![],
            requested: None,
            sample_age: None,
            last_takeover: "none".into(),
            last_release: "none".into(),
            last_power: "none".into(),
            errors: 0,
            last_error: "none".into(),
            retry_level: 0,
            retry_in: None,
            recovery_pending: pending,
            automatic_verified: false,
            sampled_at: None,
        }
    }
    pub fn line(&self) -> String {
        let mut s = format!(
            "service={} uptime={:.1}s | chip_max={} | battery {}",
            self.phase,
            self.uptime.as_secs_f64(),
            self.chip
                .map(|v| format!("{v:.1}C"))
                .unwrap_or_else(|| "unavailable".into()),
            if self.battery.is_empty() {
                "unavailable".into()
            } else {
                self.battery
                    .iter()
                    .map(|r| format!("{}={:.1}C", r.key, r.celsius))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        );
        for f in &self.fans {
            s.push_str(&format!(
                " | Fan {}: actual {:.0} target {:.0} RPM {}",
                f.index,
                f.actual,
                f.target,
                fan::mode_label(f.mode)
            ));
        }
        s.push_str(&format!("\nsample_age={} recovery_pending={} automatic_verified={} retry_level={} retry_in={}\nlast_takeover={}\nlast_release={}\nlast_power={}\nerrors={} last_error={}",self.sample_age.map(|v|format!("{:.1}s",v.as_secs_f64())).unwrap_or_else(||"unavailable".into()),self.recovery_pending,self.automatic_verified,self.retry_level,self.retry_in.map(|v|format!("{:.1}s",v.as_secs_f64())).unwrap_or_else(||"none".into()),self.last_takeover,self.last_release,self.last_power,self.errors,self.last_error));
        s
    }
    pub fn log_key(&self) -> String {
        format!(
            "{} {:?} {} {}",
            self.phase, self.requested, self.errors, self.last_power
        )
    }
    fn error(&mut self, error: &Error) {
        self.errors = self.errors.saturating_add(1);
        self.last_error = format!("t={:.1}s {error}", self.uptime.as_secs_f64());
    }
    fn refresh(&mut self, now: Duration, started: Duration) {
        self.uptime = now.saturating_sub(started);
        self.sample_age = self.sampled_at.map(|t| now.saturating_sub(t));
    }
    fn sample(
        &mut self,
        chip: f32,
        battery: Vec<BatteryReading>,
        fans: Vec<FanState>,
        now: Duration,
    ) {
        self.chip = Some(chip);
        self.battery = battery;
        self.fans = fans;
        self.sampled_at = Some(now);
        self.sample_age = Some(Duration::ZERO);
    }
}
pub trait Observer {
    fn stop_requested(&mut self) -> Result<bool> {
        Ok(false)
    }
    fn ready(&mut self, status: &Status) -> Result<()> {
        self.update(status)
    }
    fn update(&mut self, status: &Status) -> Result<()>;
    fn event(&mut self, message: &str) -> Result<()>;
    fn healthy(&mut self) -> Result<()> {
        Ok(())
    }
    fn unhealthy(&mut self) {}
    /// Persist recovery responsibility before the first possible hardware write.
    fn recovery_pending(&mut self, _pending: bool) -> Result<()> {
        Ok(())
    }
}
#[derive(Default)]
pub struct Backoff {
    level: usize,
}
impl Backoff {
    pub fn next_delay(&mut self) -> Duration {
        let d = [5, 15, 30, 60][self.level.min(3)];
        self.level = self.level.saturating_add(1).min(4);
        Duration::from_secs(d)
    }
    pub fn reset(&mut self) {
        self.level = 0;
    }
}
fn chip_max(source: &mut impl ChipTemperatureSource) -> Result<f32> {
    let readings = source.sample()?;
    let mut maximum: Option<f32> = None;
    for r in readings {
        if !r.celsius.is_finite() || r.celsius <= 0. || r.celsius > 130. {
            return Err(Error::Invalid("invalid chip temperature".into()));
        }
        maximum = Some(maximum.map_or(r.celsius, |v| v.max(r.celsius)));
    }
    maximum.ok_or_else(|| Error::Missing("chip sensors".into()))
}
fn snapshot(
    smc: &mut impl ReadSmc,
    plan: &Plan,
    source: &mut impl ChipTemperatureSource,
) -> Result<(f32, Vec<BatteryReading>, Vec<FanState>)> {
    let chip = chip_max(source);
    let battery = battery::sample(smc);
    let fans = plan
        .fans
        .iter()
        .map(|f| fan::state(smc, f))
        .collect::<Result<Vec<_>>>();
    // An invalid temperature never conceals observed foreign manual ownership.
    if let Ok(fans) = &fans
        && fans.iter().any(|f| !matches!(f.mode, 0 | 3))
    {
        return Err(Error::Control(
            "preflight observed manual ownership; waiting for automatic mode".into(),
        ));
    }
    if plan.ftst && smc.read("Ftst")?.integer()? != 0 {
        return Err(Error::Control("preflight Ftst remains active".into()));
    }
    smc.trace().status()?;
    Ok((chip?, battery?, fans?))
}
struct EpochClock<'a, C: Clock, P: PowerSource> {
    clock: &'a C,
    power: &'a P,
    generation: u64,
}
impl<C: Clock, P: PowerSource> Clock for EpochClock<'_, C, P> {
    fn now(&self) -> Duration {
        self.clock.now()
    }
    fn settle(&self, d: Duration) {
        self.clock.settle(d);
    }
    fn wait(&self, d: Duration) -> bool {
        let end = self.now() + d;
        loop {
            if self.power.sleep_pending() || self.power.generation() != self.generation {
                return false;
            }
            if !self.clock.wait(Duration::ZERO) {
                return false;
            }
            let left = end.saturating_sub(self.now());
            if left.is_zero() {
                return true;
            }
            if !self.clock.wait(left.min(Duration::from_millis(50))) {
                return false;
            }
        }
    }
}
struct EpochObserver<'a, O: Observer, P: PowerSource, C: Clock> {
    outer: &'a mut O,
    power: &'a P,
    clock: &'a C,
    started: Duration,
    healthy_since: Option<Duration>,
    wrote: &'a Cell<bool>,
    generation: u64,
    status: &'a mut Status,
    ready: &'a mut bool,
    fault: Option<Error>,
    stop: bool,
    output_failed: bool,
    backoff: &'a mut Backoff,
}
impl<O: Observer, P: PowerSource, C: Clock> EpochObserver<'_, O, P, C> {
    fn output<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.output_failed = true;
        }
        result
    }
}
impl<O: Observer, P: PowerSource, C: Clock> supervisor::Observer for EpochObserver<'_, O, P, C> {
    fn stop_requested(&mut self) -> Result<bool> {
        let stop = self.outer.stop_requested();
        let stop = self.output(stop)?;
        self.stop |= stop;
        Ok(stop
            || self.fault.is_some()
            || self.power.sleep_pending()
            || self.power.generation() != self.generation)
    }
    fn arming(&mut self) -> Result<()> {
        let result = self.outer.recovery_pending(true);
        self.output(result)?;
        self.status.recovery_pending = true;
        self.status.automatic_verified = false;
        Ok(())
    }
    fn transition(&mut self, kind: Transition) -> Result<()> {
        self.status.refresh(self.clock.now(), self.started);
        match kind {
            Transition::Engaged => {
                self.status.recovery_pending = true;
                self.status.automatic_verified = false;
                self.status.last_takeover = format!(
                    "t={:.1}s temperature demand",
                    self.status.uptime.as_secs_f64()
                );
            }
            Transition::Released => {
                self.status.recovery_pending = false;
                self.status.automatic_verified = true;
                let r = self.outer.recovery_pending(false);
                self.output(r)?;
                self.status.last_release = format!(
                    "t={:.1}s both demands cleared for 30s; automatic verified",
                    self.status.uptime.as_secs_f64()
                );
            }
        }
        Ok(())
    }
    fn fault(&mut self, error: &Error, recovery: &Result<()>) -> Result<()> {
        self.status.refresh(self.clock.now(), self.started);
        self.status.error(error);
        self.status.recovery_pending = recovery.is_err();
        self.status.automatic_verified = recovery.is_ok();
        if recovery.is_ok() {
            let r = self.outer.recovery_pending(false);
            self.output(r)?;
        }
        self.status.last_release = format!(
            "t={:.1}s control fault; recovery={recovery:?}",
            self.status.uptime.as_secs_f64()
        );
        self.fault = Some(error.clone());
        self.outer.unhealthy();
        Ok(())
    }
    fn finished(&mut self, recovery: &Result<()>) -> Result<()> {
        self.status.automatic_verified = recovery.is_ok();
        if recovery.is_ok() {
            self.status.recovery_pending = false;
            let r = self.outer.recovery_pending(false);
            self.output(r)?;
        } else if self.wrote.get() {
            self.status.recovery_pending = true;
        }
        Ok(())
    }
    fn ready(&mut self, frame: &Frame) -> Result<()> {
        self.frame(frame)?;
        if !*self.ready {
            let r = self.outer.ready(self.status);
            self.output(r)?;
            *self.ready = true;
        }
        Ok(())
    }
    fn frame(&mut self, frame: &Frame) -> Result<()> {
        let now = self.clock.now();
        self.status.refresh(now, self.started);
        if let Some(problem) = &frame.problem {
            if self.fault.is_none() {
                self.status.error(&Error::Io(problem.clone()));
            }
            self.outer.unhealthy();
            self.healthy_since = None;
            self.status.phase = "telemetry-degraded".into();
        } else {
            let max = frame
                .readings
                .iter()
                .map(|r| r.celsius)
                .reduce(f32::max)
                .ok_or_else(|| Error::Missing("chip frame".into()))?;
            self.status
                .sample(max, frame.battery.clone(), frame.fans.clone(), now);
            self.status.phase = if frame.requested.is_some() {
                "manual"
            } else {
                "monitoring-automatic"
            }
            .into();
            let r = self.outer.healthy();
            self.output(r)?;
            if now.saturating_sub(*self.healthy_since.get_or_insert(now)) >= Duration::from_secs(60)
            {
                self.backoff.reset();
                self.status.retry_level = 0;
            }
        }
        if self.status.requested.is_none() && frame.requested.is_some() && frame.problem.is_none() {
            self.status.last_takeover = format!(
                "t={:.1}s {}; chip={:?}C; battery={:?}",
                self.status.uptime.as_secs_f64(),
                frame.state,
                self.status.chip,
                self.status.battery
            );
        }
        self.status.requested = frame.requested.clone();
        let r = self.outer.update(self.status);
        self.output(r)
    }
    fn event(&mut self, message: &str) -> Result<()> {
        let r = self.outer.event(message);
        self.output(r)
    }
}
fn events(
    power: &impl PowerSource,
    status: &mut Status,
    clock: &impl Clock,
    started: Duration,
    mut before_sleep: impl FnMut(&mut Status) -> Result<()>,
) -> Result<bool> {
    let mut changed = false;
    status.refresh(clock.now(), started);
    for event in power.drain() {
        match event {
            PowerEvent::WillSleep { token } => {
                status.last_power = format!("t={:.1}s will-sleep", status.uptime.as_secs_f64());
                // There may be no active Session while a previous crash or
                // failed recovery still owns this responsibility. Give that
                // responsibility one bounded recovery attempt before our ACK.
                // The independent power-thread deadline still allows sleep.
                if status.recovery_pending
                    && let Err(error) = before_sleep(status)
                {
                    status.error(&error);
                }
                status.last_release = format!(
                    "sleep: automatic_verified_at_handling={}",
                    status.automatic_verified
                );
                power.acknowledge_sleep(token)?;
                changed = true;
            }
            PowerEvent::WillPowerOn => {
                status.last_power = format!(
                    "t={:.1}s waking; awaiting has-powered-on",
                    status.uptime.as_secs_f64()
                );
                changed = true;
            }
            PowerEvent::DidWake => {
                status.last_power = format!(
                    "t={:.1}s has-powered-on; rebuild connections",
                    status.uptime.as_secs_f64()
                );
                changed = true;
            }
            PowerEvent::CancelledSleep => {
                status.last_power = format!(
                    "t={:.1}s sleep-cancelled; rebuild connections",
                    status.uptime.as_secs_f64()
                );
                changed = true;
            }
            PowerEvent::SleepAcknowledged {
                deadline, success, ..
            } => {
                if deadline || !success {
                    status.error(&Error::Control(format!("sleep acknowledgment deadline={deadline} success={success}; acknowledgment alone does not verify fan restoration")));
                }
            }
            PowerEvent::Error(message) => {
                return Err(Error::Control(format!(
                    "power notification failure: {message}"
                )));
            }
        }
    }
    Ok(changed)
}
#[derive(PartialEq)]
enum Wait {
    Done,
    Power,
    Stop,
}
fn wait(
    delay: Duration,
    power: &impl PowerSource,
    clock: &impl Clock,
    observer: &mut impl Observer,
    status: &mut Status,
    started: Duration,
) -> Result<Wait> {
    let deadline = clock.now() + delay;
    let generation = power.generation();
    let mut next_status = clock.now();
    loop {
        if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
            return Ok(Wait::Stop);
        }
        if power.generation() != generation {
            return Ok(Wait::Power);
        }
        let now = clock.now();
        status.refresh(now, started);
        status.retry_in = Some(deadline.saturating_sub(now));
        if now >= next_status {
            observer.update(status)?;
            next_status = now + Duration::from_secs(1);
        }
        if now >= deadline {
            return Ok(Wait::Done);
        }
        if !clock.wait((deadline - now).min(Duration::from_millis(50))) {
            return Ok(Wait::Stop);
        }
    }
}
fn recover(
    smc: &mut impl WriteSmc,
    plan: &Plan,
    clock: &impl Clock,
    pending: &mut bool,
) -> Result<()> {
    if supervisor::verify_automatic(smc, plan, clock).is_ok() {
        *pending = false;
        return Ok(());
    }
    if !*pending {
        return Err(Error::Control(
            "automatic ownership unavailable; foreign manual control is not overwritten".into(),
        ));
    }
    // At most two bounded recovery rounds per attempt. Keep responsibility after failure.
    let result = restore_all(smc, plan, clock).or_else(|first| {
        restore_all(smc, plan, clock)
            .map_err(|last| Error::Control(format!("recovery: {first}; final attempt: {last}")))
    });
    result?;
    supervisor::verify_automatic(smc, plan, clock)?;
    *pending = false;
    Ok(())
}
/// One instance owns ControlLock across all epochs. Native connections are rebuilt
/// only after the old Session has completed its recovery path.
pub fn run(
    factory: &mut impl Factory,
    power: &impl PowerSource,
    clock: &impl Clock,
    observer: &mut impl Observer,
    startup_delay: Duration,
    recovery_pending: bool,
) -> Result<Completion> {
    let started = clock.now();
    let mut status = Status::new(recovery_pending);
    let mut backoff = Backoff::default();
    let mut ready = false;
    let mut last_plan: Option<Plan> = None;
    let mut selected_sensors = None;
    let operation = (|| {
        if !startup_delay.is_zero() {
            status.phase = "process-restart-backoff".into();
            if wait(startup_delay, power, clock, observer, &mut status, started)? == Wait::Stop {
                return Ok(());
            }
        }
        loop {
            if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
                break;
            }
            events(power, &mut status, clock, started, |status| {
                sleep_release(factory, last_plan.as_ref(), clock, observer, status)
            })?;
            if power.sleep_pending() {
                status.phase = "sleeping".into();
                observer.unhealthy();
                if wait(
                    Duration::from_secs(1),
                    power,
                    clock,
                    observer,
                    &mut status,
                    started,
                )? == Wait::Stop
                {
                    break;
                }
                continue;
            }
            status.retry_in = None;
            status.phase = "reinitializing".into();
            status.refresh(clock.now(), started);
            observer.update(&status)?;
            let generation = power.generation();
            let epoch_clock = EpochClock {
                clock,
                power,
                generation,
            };
            let mut error_recorded = false;
            let wrote = Cell::new(false);
            let attempt = (|| {
                status.automatic_verified = false;
                let mut smc = TrackedSmc {
                    inner: factory.smc()?,
                    wrote: &wrote,
                };
                let plan = fan::plan(&mut smc)?;
                if plan.fans.is_empty() || !plan.problems.is_empty() {
                    return Err(Error::Control(format!(
                        "fan discovery incomplete: {:?}",
                        plan.problems
                    )));
                }
                if let Some(old) = &last_plan
                    && (old.fans.len() != plan.fans.len()
                        || old
                            .fans
                            .iter()
                            .zip(&plan.fans)
                            .any(|(a, b)| a.index != b.index || a.mode_key != b.mode_key))
                {
                    return Err(Error::Control(
                        "confirmed fan identity changed; recovery responsibility retained".into(),
                    ));
                }
                last_plan = Some(plan.clone());
                if !epoch_clock.wait(Duration::ZERO) {
                    return Err(Error::Interrupted);
                }
                recover(&mut smc, &plan, clock, &mut status.recovery_pending)?;
                status.automatic_verified = true;
                observer.recovery_pending(false)?;
                wrote.set(false);
                let mut source = ConsistentSource {
                    inner: factory.source()?,
                    selected: &mut selected_sensors,
                };
                let warmup_started = clock.now();
                let mut good = 0;
                let mut previous = clock.now();
                status.phase = "qualifying-fresh-sensors".into();
                loop {
                    if observer.stop_requested()? {
                        return Ok(true);
                    }
                    if !epoch_clock.wait(Duration::ZERO) {
                        return Err(Error::Interrupted);
                    }
                    let cycle = clock.now();
                    if cycle.saturating_sub(previous) > Duration::from_secs(5) {
                        return Err(Error::Control(
                            "preflight sampling gap; rebuild required".into(),
                        ));
                    }
                    let sampled = snapshot(&mut smc, &plan, &mut source);
                    if !epoch_clock.wait(Duration::ZERO) {
                        return Err(Error::Interrupted);
                    }
                    if clock.now().saturating_sub(cycle) > Duration::from_secs(5) {
                        return Err(Error::Control("preflight I/O gap; rebuild required".into()));
                    }
                    status.refresh(clock.now(), started);
                    match sampled {
                        Ok((chip, battery, fans)) => {
                            good += 1;
                            status.sample(chip, battery, fans, clock.now());
                        }
                        Err(e) => {
                            good = 0;
                            status.automatic_verified = false;
                            status.error(&e);
                            observer.unhealthy();
                            if matches!(e, Error::Diagnostic(_)) {
                                return Err(e);
                            }
                        }
                    }
                    observer.update(&status)?;
                    if clock.now().saturating_sub(warmup_started) > Duration::from_secs(10) {
                        return Err(Error::Control(
                            "fresh sensor qualification exceeded 10s".into(),
                        ));
                    }
                    if good >= 3 {
                        status.automatic_verified = true;
                        break;
                    }
                    if clock.now().saturating_sub(warmup_started) >= Duration::from_secs(10) {
                        return Err(Error::Control("fresh sensor qualification did not reach three consecutive healthy samples within 10s".into()));
                    }
                    previous = cycle;
                    if !epoch_clock.wait(Duration::from_secs(1)) {
                        return Err(Error::Interrupted);
                    }
                }
                // The qualification sample consumed the latest HID events.
                // Start the control loop on the next sensor period, never on
                // a duplicate read of the same accepted batch.
                let qualified_at = clock.now();
                if !epoch_clock.wait(Duration::from_secs(1)) {
                    return Err(Error::Interrupted);
                }
                if clock.now().saturating_sub(qualified_at) > Duration::from_secs(5) {
                    return Err(Error::Control(
                        "gap after sensor qualification; rebuild required".into(),
                    ));
                }
                let mut adapter = EpochObserver {
                    outer: observer,
                    power,
                    clock,
                    started,
                    healthy_since: None,
                    wrote: &wrote,
                    generation,
                    status: &mut status,
                    ready: &mut ready,
                    fault: None,
                    stop: false,
                    output_failed: false,
                    backoff: &mut backoff,
                };
                let outcome =
                    supervisor::run(&mut smc, &plan, &mut source, &epoch_clock, &mut adapter);
                let stop = adapter.stop;
                let failed_output = adapter.output_failed;
                let fault = adapter.fault.take();
                if failed_output {
                    return Err(Error::Diagnostic(format!(
                        "service output failed: {outcome:?}"
                    )));
                }
                if stop {
                    return outcome.map(|_| true);
                }
                if let Some(error) = fault {
                    error_recorded = true;
                    return Err(error);
                }
                outcome?;
                if power.generation() != generation || power.sleep_pending() {
                    return Err(Error::Interrupted);
                }
                // The underlying clock was interrupted by SIGINT/SIGTERM/SIGHUP.
                Ok(true)
            })();
            // The old connection/session is now gone. Only queue ACK after recovery.
            events(power, &mut status, clock, started, |status| {
                sleep_release(factory, last_plan.as_ref(), clock, observer, status)
            })?;
            match attempt {
                Ok(true) => break,
                Ok(false) => {}
                Err(Error::Interrupted) => {
                    if !clock.wait(Duration::ZERO) || observer.stop_requested()? {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    status.refresh(clock.now(), started);
                    if !error_recorded {
                        status.error(&error);
                    }
                    observer.unhealthy();
                    status.requested = None;
                    if matches!(error, Error::Diagnostic(_)) {
                        return Err(error);
                    }
                    if power.sleep_pending() {
                        continue;
                    }
                    status.phase = "recovery-backoff".into();
                    let delay = backoff.next_delay();
                    status.retry_level = backoff.level;
                    observer.event(&format!(
                        "{}; retry in {}s; recovery_pending={}",
                        error,
                        delay.as_secs(),
                        status.recovery_pending
                    ))?;
                    if wait(delay, power, clock, observer, &mut status, started)? == Wait::Stop {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    observer.unhealthy();
    status.phase = "stopping".into();
    status.requested = None;
    // A stopped instance never leaves an unverified responsibility silently behind.
    let verification =
        if power.sleep_pending() && status.automatic_verified && !status.recovery_pending {
            Ok(())
        } else {
            occur_recovery(
                factory,
                last_plan.as_ref(),
                clock,
                &mut status.recovery_pending,
            )
        };
    let persisted = if verification.is_ok() {
        observer.recovery_pending(false)
    } else {
        Ok(())
    };
    let reported = observer.event(if verification.is_ok() {
        "automatic mode verified; automatic supervisor stopped"
    } else {
        "automatic restoration NOT verified; recovery remains required"
    });
    let reported = persisted.and(reported);
    match (operation, verification, reported) {
        (_, Err(e), _) => Err(e),
        (Err(e), _, _) => Err(e),
        (_, _, Err(e)) => Err(e),
        _ => Ok(Completion::AutomaticVerified),
    }
}
fn occur_recovery(
    factory: &mut impl Factory,
    known: Option<&Plan>,
    clock: &impl Clock,
    pending: &mut bool,
) -> Result<()> {
    let mut smc = factory.smc()?;
    let plan = fan::plan(&mut smc)?;
    if let Some(old) = known
        && (old.fans.len() != plan.fans.len()
            || old
                .fans
                .iter()
                .zip(&plan.fans)
                .any(|(a, b)| a.index != b.index || a.mode_key != b.mode_key))
    {
        return Err(Error::Control("final recovery fan identity differs".into()));
    }
    recover(&mut smc, &plan, clock, pending)
}

fn sleep_release(
    factory: &mut impl Factory,
    known: Option<&Plan>,
    clock: &impl Clock,
    observer: &mut impl Observer,
    status: &mut Status,
) -> Result<()> {
    observer.unhealthy();
    let result = occur_recovery(factory, known, clock, &mut status.recovery_pending);
    status.automatic_verified = result.is_ok();
    result?;
    observer.recovery_pending(false)
}
