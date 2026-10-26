use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use m1fan::automation::{self, NativeFactory};
use m1fan::chip_temps::HidTemperatures;
use m1fan::control::{Clock, FailureWindow, Session, restore_all};
use m1fan::daemon::{self, DaemonLog, WorkerIpc};
use m1fan::fan::{self, Plan};
use m1fan::platform::{self, ControlLock, Hardware, RealClock};
use m1fan::power::NativePower;
use m1fan::service_state::ServiceState;
use m1fan::smc::{Error, NativeSmc, ReadSmc, Result};
use m1fan::supervisor::{self, Completion, Frame, Observer};
use m1fan::temps::Temperatures;
use m1fan::trace::Trace;
use std::path::PathBuf;

const HELP: &str = "m1fan 0.5.0 — Apple M1 fan and temperature control
Usage:
  m1fan status                  Read hardware, RPM, modes and temperatures
  m1fan verify-auto             Read-only check of automatic ownership
  m1fan monitor [--samples N]   Refresh once a second; read only
  sudo m1fan max                Set hardware maximum; Ctrl+C restores auto
  sudo m1fan auto               Restore and verify automatic fan management
  m1fan preview [--samples N]   Read-only chip/battery temperatures and proposed RPM
  sudo m1fan run                Foreground automatic thermal management; Ctrl+C restores
  sudo m1fan serve              Foreground service with background status/stop IPC
  sudo m1fan start              Start detached temperature control manually
  sudo m1fan daemon-status      Read background controller status
  sudo m1fan stop               Stop background control; wait for restoration
Options: --trace <new-file.jsonl> records each SMC operation for data/control commands.
Background control survives Terminal closure. deploy/ installs optional boot startup.
Chip: on 55C, off below 50C for 30s, max 70C. Battery: on 35C, off
below 35C for 30s, max 40C. Same policy on AC and battery power.
SIGINT/SIGTERM/SIGHUP and normal errors trigger cleanup. Sleep releases control; wake and gaps rebuild connections and qualify fresh data.
Failures retry after 5/15/30/60s, then every 60s, with automatic-mode preflight.
SIGKILL, power loss and kernel failure cannot run process cleanup.";

#[derive(Debug, PartialEq)]
enum Command {
    Status,
    Monitor(Option<usize>),
    Max,
    Auto,
    VerifyAuto,
    Preview(Option<usize>),
    Run,
    Serve,
    Start,
    Stop,
    DaemonStatus,
    Worker,
    Help,
    Version,
}

fn parse(args: &[String]) -> Result<Command> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["help" | "--help" | "-h"] => Ok(Command::Help),
        ["--version" | "-V"] => Ok(Command::Version),
        ["status"] => Ok(Command::Status),
        ["monitor"] => Ok(Command::Monitor(None)),
        ["monitor", "--samples", n] => n
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| Command::Monitor(Some(n)))
            .ok_or_else(|| Error::Invalid("--samples must be a positive integer".into())),
        ["max"] => Ok(Command::Max),
        ["auto"] => Ok(Command::Auto),
        ["verify-auto"] => Ok(Command::VerifyAuto),
        ["preview"] => Ok(Command::Preview(None)),
        ["preview", "--samples", n] => n
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .map(|n| Command::Preview(Some(n)))
            .ok_or_else(|| Error::Invalid("--samples must be a positive integer".into())),
        ["run"] => Ok(Command::Run),
        ["serve"] => Ok(Command::Serve),
        ["start"] => Ok(Command::Start),
        ["stop"] => Ok(Command::Stop),
        ["daemon-status"] => Ok(Command::DaemonStatus),
        ["_worker"] => Ok(Command::Worker),
        _ => Err(Error::Invalid(format!(
            "unknown command or options\n{HELP}"
        ))),
    }
}

#[derive(Debug, PartialEq)]
struct Options {
    command: Command,
    trace: Option<PathBuf>,
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut remaining = Vec::new();
    let mut trace = None;
    let mut input = args.iter();
    while let Some(argument) = input.next() {
        if argument == "--trace" {
            if trace.is_some() {
                return Err(Error::Diagnostic(
                    "--trace may be specified only once".into(),
                ));
            }
            let path = input
                .next()
                .filter(|p| !p.is_empty() && !p.starts_with("--"))
                .ok_or_else(|| Error::Diagnostic("--trace requires a new file path".into()))?;
            trace = Some(PathBuf::from(path));
        } else {
            remaining.push(argument.clone());
        }
    }
    let command = parse(&remaining)?;
    if trace.is_some()
        && matches!(
            command,
            Command::Stop | Command::DaemonStatus | Command::Help | Command::Version
        )
    {
        return Err(Error::Invalid(
            "--trace applies to data/control commands only".into(),
        ));
    }
    Ok(Options { command, trace })
}

fn execute(options: Options) -> Result<()> {
    if matches!(
        options.command,
        Command::Max
            | Command::Auto
            | Command::Run
            | Command::Serve
            | Command::Start
            | Command::Stop
            | Command::DaemonStatus
            | Command::Worker
    ) {
        platform::require_root()?;
    }
    if matches!(
        options.command,
        Command::Start | Command::Stop | Command::DaemonStatus
    ) {
        platform::install_signals()?;
    }
    let reply = match options.command {
        Command::Start => Some(daemon::start(options.trace.as_deref())?),
        Command::Stop => Some(daemon::stop()?),
        Command::DaemonStatus => Some(daemon::status()?),
        _ => None,
    };
    if let Some(reply) = reply {
        return writeln!(io::stdout(), "{reply}").map_err(io_error);
    }
    let trace = match options.trace {
        Some(path) => Trace::create(&path)?,
        None => Trace::disabled(),
    };
    trace.event(
        "session_start",
        &format!(
            "version={} pid={} command={:?} unix_us={}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            options.command,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros()
        ),
    );
    trace.status()?;
    let result = execute_inner(options.command, &trace);
    trace.event("session_end", &format!("result={result:?}"));
    match (result, trace.status()) {
        (result, Ok(())) => result,
        (Ok(()), Err(log_error)) => Err(log_error),
        (Err(error), Err(log_error)) => Err(Error::Diagnostic(format!(
            "{error}; additionally {log_error}"
        ))),
    }
}

fn io_error(e: io::Error) -> Error {
    Error::Io(format!("terminal output: {e}"))
}

fn display(
    smc: &mut impl ReadSmc,
    hw: &Hardware,
    plan: &Plan,
    temps: Option<&Temperatures>,
    clear: bool,
) -> Result<bool> {
    let mut out = io::stdout().lock();
    if clear {
        write!(out, "\x1b[2J\x1b[H").map_err(io_error)?;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(
        out,
        "{} | {} | macOS {} | unix {stamp}",
        hw.model, hw.chip, hw.os
    )
    .map_err(io_error)?;
    writeln!(out, "Fans: {} | SMC bounds and RPM", fan::count(smc)?).map_err(io_error)?;
    let mut healthy = plan.problems.is_empty();
    for problem in &plan.problems {
        writeln!(out, "ERROR: {problem}").map_err(io_error)?;
    }
    for spec in &plan.fans {
        match fan::state(smc, spec) {
            Ok(f) => {
                writeln!(
                    out,
                    "Fan {}: actual {:.0} | target {:.0} | min {:.0} | max {:.0} RPM | {} [{}]",
                    f.index,
                    f.actual,
                    f.target,
                    f.min,
                    f.max,
                    fan::mode_label(f.mode),
                    spec.mode_key
                )
                .map_err(io_error)?;
                if !matches!(f.mode, 0 | 1 | 3) {
                    healthy = false;
                }
            }
            Err(e) => {
                healthy = false;
                writeln!(out, "Fan {} ERROR: {e}", spec.index).map_err(io_error)?;
            }
        }
    }
    if plan.ftst {
        match smc.read("Ftst").and_then(|v| v.integer()) {
            Ok(n @ (0 | 1)) => writeln!(out, "Ftst: {n}").map_err(io_error)?,
            Ok(n) => {
                healthy = false;
                writeln!(out, "Ftst ERROR: unknown state {n}").map_err(io_error)?;
            }
            Err(e) => {
                healthy = false;
                writeln!(out, "Ftst ERROR: {e}").map_err(io_error)?;
            }
        }
    }
    match temps {
        Some(temps) => {
            writeln!(
                out,
                "Temperatures ({} SMC keys, Celsius):",
                temps.keys.len()
            )
            .map_err(io_error)?;
            for (index, (key, sample)) in temps.sample(smc).into_iter().enumerate() {
                match sample {
                    Ok(t) => write!(out, "  {key} {t:.1}").map_err(io_error)?,
                    Err(e) => {
                        healthy = false;
                        write!(out, "  {key} ERROR({e})").map_err(io_error)?;
                    }
                }
                if (index + 1) % 6 == 0 {
                    writeln!(out).map_err(io_error)?;
                }
            }
            writeln!(out).map_err(io_error)?;
        }
        None => {
            healthy = false;
            writeln!(out, "Temperatures: unavailable").map_err(io_error)?;
        }
    }
    out.flush().map_err(io_error)?;
    Ok(healthy)
}

fn note(message: impl std::fmt::Display) {
    let _ = writeln!(io::stderr().lock(), "m1fan: {message}");
}

// Boot-time services can appear after launchd starts us. Retry only discovery;
// this helper never receives any function that writes SMC data.
fn startup_read<T>(deadline: Option<Instant>, mut read: impl FnMut() -> Result<T>) -> Result<T> {
    loop {
        match read() {
            Ok(value) => return Ok(value),
            Err(error) => {
                let Some(deadline) = deadline else {
                    return Err(error);
                };
                if Instant::now() >= deadline {
                    return Err(error);
                }
                if !RealClock.wait(Duration::from_millis(500)) {
                    return Err(Error::Interrupted);
                }
            }
        }
    }
}

fn execute_inner(command: Command, trace: &Trace) -> Result<()> {
    match command {
        Command::Help => return writeln!(io::stdout(), "{HELP}").map_err(io_error),
        Command::Version => {
            return writeln!(io::stdout(), "m1fan {}", env!("CARGO_PKG_VERSION")).map_err(io_error);
        }
        _ => (),
    }
    let writing = matches!(
        command,
        Command::Max | Command::Auto | Command::Run | Command::Worker | Command::Serve
    );
    // Fail before opening a device or creating a lock when privileges are missing.
    if writing {
        platform::require_root()?;
    }
    let hw = Hardware::detect()?;
    if writing {
        hw.require_m1()?;
    }
    platform::install_signals()?;
    let mut service_state = if command == Command::Serve {
        Some(ServiceState::begin()?)
    } else {
        None
    };
    if let Some(state) = service_state.as_mut()
        && !state.enabled()?
    {
        state.finish(!state.recovery_pending())?;
        return Ok(());
    }
    let _lock = if writing {
        Some(ControlLock::acquire()?)
    } else {
        None
    };
    if matches!(command, Command::Run | Command::Worker | Command::Serve) {
        return execute_automatic(command, trace, service_state);
    }
    let deadline = None;
    let mut smc = startup_read(deadline, || {
        if writing {
            NativeSmc::open_control()
        } else {
            NativeSmc::open_read_only()
        }
    })?;
    smc.set_trace(trace.clone());
    let plan = startup_read(deadline, || {
        let plan = fan::plan(&mut smc)?;
        if command == Command::Serve && (plan.fans.is_empty() || !plan.problems.is_empty()) {
            return Err(Error::Control(format!(
                "waiting for fan discovery: {:?}",
                plan.problems
            )));
        }
        Ok(plan)
    })?;
    let clock = RealClock;
    if command == Command::VerifyAuto {
        supervisor::verify_automatic(&mut smc, &plan, &clock)?;
        note("automatic mode verified (read only)");
        return Ok(());
    }
    // Recovery needs no working temperature sensors or RPM limits.
    if command == Command::Auto {
        restore_all(&mut smc, &plan, &clock)?;
        note("automatic mode verified for all discovered fans; Ftst released when present");
        return Ok(());
    }
    if let Command::Preview(samples) = command {
        let mut source = HidTemperatures::open()?;
        return supervisor::preview(
            &mut smc,
            &plan,
            &mut source,
            &clock,
            samples,
            &mut TerminalObserver,
        );
    }
    let temps = match Temperatures::discover(&mut smc) {
        Ok(t) => {
            if t.inactive_count > 0 {
                note(format!(
                    "{} zero-valued temperature keys omitted at discovery",
                    t.inactive_count
                ));
            }
            for warning in &t.discovery_warnings {
                note(format!("sensor discovery: {warning}"));
            }
            Some(t)
        }
        Err(e) => {
            note(format!("temperature discovery: {e}"));
            None
        }
    };
    if command == Command::Status {
        return if display(&mut smc, &hw, &plan, temps.as_ref(), false)? {
            Ok(())
        } else {
            Err(Error::Io(
                "status contains unavailable or abnormal sensor data".into(),
            ))
        };
    }
    if let Command::Monitor(samples) = command {
        let mut n = 0;
        while !platform::stopped() {
            let begin = Instant::now();
            let _ = display(
                &mut smc,
                &hw,
                &plan,
                temps.as_ref(),
                io::stdout().is_terminal(),
            )?;
            n += 1;
            if samples.is_some_and(|limit| n >= limit) {
                break;
            }
            if !clock.wait(Duration::from_secs(1).saturating_sub(begin.elapsed())) {
                break;
            }
        }
        return Ok(());
    }
    // Complete all read-only preflight checks before creating restoration responsibility.
    if !display(&mut smc, &hw, &plan, temps.as_ref(), false)? {
        return Err(Error::Invalid(
            "max preflight failed; no SMC writes performed".into(),
        ));
    }
    let mut session = Session::new(&mut smc, plan.clone(), &clock);
    let operation = (|| {
        session.engage_max()?;
        note("maximum target and manual mode verified; Ctrl+C to restore automatic mode");
        let started = Instant::now();
        let mut failures = FailureWindow::default();
        while !platform::stopped() {
            let cycle = Instant::now();
            let control_ok = match session.check_hold(started.elapsed() >= Duration::from_secs(30))
            {
                Ok(()) => true,
                Err(e @ (Error::Control(_) | Error::Diagnostic(_))) => return Err(e),
                Err(e) => {
                    note(e);
                    false
                }
            };
            let sensor_ok = match display(
                session.smc(),
                &hw,
                &plan,
                temps.as_ref(),
                io::stdout().is_terminal(),
            ) {
                Ok(healthy) => healthy,
                Err(e @ Error::Io(_)) => return Err(e),
                Err(e) => {
                    note(e);
                    false
                }
            };
            failures.record(control_ok && sensor_ok)?;
            if !clock.wait(Duration::from_secs(1).saturating_sub(cycle.elapsed())) {
                break;
            }
        }
        Ok(())
    })();
    trace.event(
        "control_end",
        &format!(
            "result={operation:?} signal_requested={}",
            platform::stopped()
        ),
    );
    let recovery_required = session.recovery_required();
    let recovery = session.finish();
    if !recovery_required {
        note("control was not started; no SMC writes or recovery performed");
        return match operation {
            Err(Error::Interrupted) => Ok(()),
            result => result,
        };
    }
    match (operation, recovery) {
        (Ok(()) | Err(Error::Interrupted), Ok(())) => {
            note("automatic restoration verified");
            Ok(())
        }
        (Err(e), Ok(())) => {
            note("automatic restoration verified after failure");
            Err(e)
        }
        (operation, Err(e)) => Err(Error::Control(format!(
            "operation={operation:?}; automatic restoration FAILED: {e}; run sudo m1fan auto"
        ))),
    }
}

fn inventory(frame: &Frame) -> String {
    frame
        .readings
        .iter()
        .map(|r| format!("{}={:.2}C", r.name, r.celsius))
        .collect::<Vec<_>>()
        .join("; ")
}

struct TerminalObserver;
impl Observer for TerminalObserver {
    fn ready(&mut self, frame: &Frame) -> Result<()> {
        writeln!(
            io::stdout(),
            "Confirmed IOHID Products: {}",
            inventory(frame)
        )
        .map_err(io_error)?;
        self.frame(frame)
    }
    fn frame(&mut self, frame: &Frame) -> Result<()> {
        writeln!(io::stdout(), "{}", frame.line()).map_err(io_error)
    }
    fn event(&mut self, message: &str) -> Result<()> {
        writeln!(io::stderr(), "m1fan: {message}").map_err(io_error)
    }
}

impl automation::Observer for TerminalObserver {
    fn update(&mut self, status: &automation::Status) -> Result<()> {
        writeln!(io::stdout(), "{}", status.line()).map_err(io_error)
    }
    fn event(&mut self, message: &str) -> Result<()> {
        writeln!(io::stderr(), "m1fan: {message}").map_err(io_error)
    }
}

struct AutomaticObserver {
    ipc: WorkerIpc,
    log: DaemonLog,
    status: String,
    last_key: String,
    last_logged: Duration,
    service: Option<ServiceState>,
    stopped: bool,
    stop_error: Option<Error>,
}
impl AutomaticObserver {
    fn new(service: Option<ServiceState>) -> Result<Self> {
        let log = DaemonLog::open()?;
        let mut ipc = if service.is_some() {
            WorkerIpc::bind_service()?
        } else {
            WorkerIpc::bind()?
        };
        if let Some(state) = &service {
            ipc.set_instance(state.instance_id())?;
        }
        Ok(Self {
            ipc,
            log,
            status: "starting automatic supervisor".into(),
            last_key: String::new(),
            last_logged: Duration::ZERO,
            service,
            stopped: false,
            stop_error: None,
        })
    }
    fn complete(&mut self, result: &Result<Completion>) -> Result<()> {
        let ipc_result = match (&self.stop_error, result) {
            (Some(e), _) => Err(e.clone()),
            (_, Ok(_)) => Ok(()),
            (_, Err(e)) => Err(e.clone()),
        };
        let logged = self.log.line(&format!(
            "automatic_supervisor_exit result={result:?}; stop_error={:?}",
            self.stop_error
        ));
        let replied = self.ipc.finish(&ipc_result);
        let clean = result.is_ok() && self.stop_error.is_none();
        let journal = if let Some(state) = self.service.as_mut() {
            state.finish(clean)
        } else {
            Ok(())
        };
        replied.and(journal).and(logged)
    }
}
impl automation::Observer for AutomaticObserver {
    fn stop_requested(&mut self) -> Result<bool> {
        self.stopped |= self.ipc.poll(&self.status)?;
        if let Some(state) = &self.service {
            match state.enabled() {
                Ok(true) => {}
                Ok(false) => self.stopped = true,
                Err(e) => {
                    self.stopped = true;
                    self.stop_error = Some(e);
                }
            }
        }
        Ok(self.stopped)
    }
    fn ready(&mut self, status: &automation::Status) -> Result<()> {
        self.update(status)?;
        self.ipc.ready()
    }
    fn update(&mut self, status: &automation::Status) -> Result<()> {
        self.ipc.set_healthy(matches!(
            status.phase.as_str(),
            "manual" | "monitoring-automatic"
        ));
        self.status = format!(
            "m1fan {} pid={} instance={}\n{}",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            self.service
                .as_ref()
                .map(|s| s.instance_id())
                .unwrap_or("manual-background"),
            status.line()
        );
        let key = status.log_key();
        if key != self.last_key
            || status.uptime.saturating_sub(self.last_logged) >= Duration::from_secs(30)
        {
            self.log.line(&self.status)?;
            self.last_key = key;
            self.last_logged = status.uptime;
        }
        Ok(())
    }
    fn event(&mut self, message: &str) -> Result<()> {
        self.log.line(message)
    }
    fn healthy(&mut self) -> Result<()> {
        if let Some(state) = self.service.as_mut() {
            state.healthy_tick()?;
        }
        Ok(())
    }
    fn unhealthy(&mut self) {
        if let Some(state) = self.service.as_mut() {
            state.unhealthy();
        }
    }
    fn recovery_pending(&mut self, pending: bool) -> Result<()> {
        if let Some(state) = self.service.as_mut() {
            state
                .set_recovery_pending(pending)
                .map_err(|e| Error::Diagnostic(format!("persist recovery responsibility: {e}")))?;
        }
        Ok(())
    }
}
fn execute_automatic(command: Command, trace: &Trace, service: Option<ServiceState>) -> Result<()> {
    let delay = service
        .as_ref()
        .map(|s| s.restart_delay())
        .unwrap_or_default();
    let unclean = service.as_ref().is_some_and(|s| s.recovery_pending());
    let mut factory = NativeFactory {
        trace: trace.clone(),
        service: command == Command::Serve,
    };
    if command == Command::Run {
        let power = NativePower::open()?;
        return automation::run(
            &mut factory,
            &power,
            &RealClock,
            &mut TerminalObserver,
            delay,
            unclean,
        )
        .map(|_| ());
    }
    let mut observer = AutomaticObserver::new(service)?;
    let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||{
        let power = NativePower::open()?;
        automation::run(&mut factory,&power,&RealClock,&mut observer,delay,unclean)
    })).unwrap_or_else(|payload|{
        let reason=payload.downcast_ref::<String>().map(String::as_str).or_else(||payload.downcast_ref::<&str>().copied()).unwrap_or("unknown panic");
        Err(Error::Control(format!("automatic supervisor panic: {reason}; recovery not confirmed; restart will preflight recovery")))
    });
    let completion = observer.complete(&result);
    // Intentional service stops must not become KeepAlive restart requests. IPC
    // still reports any failed restoration to the caller before this clean exit.
    if command == Command::Serve && (observer.stopped || platform::stopped()) {
        if let Err(error) = &result {
            note(error);
        }
        if let Err(error) = &completion {
            note(error);
        }
        return Ok(());
    }
    match (result, completion) {
        (Ok(_), v) => v,
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(report)) => Err(Error::Io(format!("{e}; completion: {report}"))),
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let worker = args.iter().any(|a| a == "_worker");
    match parse_options(&args).and_then(execute) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            if worker {
                let _ = daemon::worker_failure(&e);
            }
            note(e);
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_is_explicit_and_rejects_duplicate_or_missing_paths() {
        assert_eq!(parse_options(&["status".into()]).unwrap().trace, None);
        assert_eq!(
            parse_options(&["max".into(), "--trace".into(), "run.jsonl".into()]).unwrap(),
            Options {
                command: Command::Max,
                trace: Some(PathBuf::from("run.jsonl"))
            }
        );
        assert!(parse_options(&["max".into(), "--trace".into()]).is_err());
        assert!(
            parse_options(&[
                "status".into(),
                "--trace".into(),
                "a".into(),
                "--trace".into(),
                "b".into()
            ])
            .is_err()
        );
    }
    #[test]
    fn cli_accepts_only_explicit_commands() {
        assert_eq!(parse(&["status".into()]).unwrap(), Command::Status);
        assert_eq!(
            parse(&["monitor".into(), "--samples".into(), "2".into()]).unwrap(),
            Command::Monitor(Some(2))
        );
        assert!(parse(&["max".into(), "8000".into()]).is_err());
        assert!(parse(&["monitor".into(), "--samples".into(), "0".into()]).is_err());
    }
    #[test]
    fn temperature_commands_require_explicit_syntax() {
        assert_eq!(parse(&["start".into()]).unwrap(), Command::Start);
        assert_eq!(parse(&["stop".into()]).unwrap(), Command::Stop);
        assert_eq!(
            parse(&["daemon-status".into()]).unwrap(),
            Command::DaemonStatus
        );
        assert_eq!(parse(&["run".into()]).unwrap(), Command::Run);
        assert_eq!(
            parse(&["preview".into(), "--samples".into(), "3".into()]).unwrap(),
            Command::Preview(Some(3))
        );
        assert!(parse(&["preview".into(), "--samples".into(), "0".into()]).is_err());
        assert!(parse(&["start".into(), "--on-boot".into()]).is_err());
        assert!(parse_options(&["stop".into(), "--trace".into(), "unused.jsonl".into()]).is_err());
    }
}
