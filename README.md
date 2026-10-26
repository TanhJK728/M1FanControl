# M1FanControl

A lightweight open-source fan control and automatic thermal management tool for compatible Apple Silicon Macs.

M1FanControl v0.5.0 reads fan registers through AppleSMC and named chip temperature events through IOHID. It takes control when chip or battery temperatures trigger cooling demand, then returns the fans to macOS when both demands have cleared. The CLI supports foreground use, a manually started background controller, and an optional system LaunchDaemon.

## Features

- Live fan modes, actual and target RPM, hardware speed limits, and temperature readings.
- Hardware maximum fan speed with automatic restoration on Ctrl+C.
- Independent chip and battery cooling demands, hysteresis, and gradual speed reduction.
- Separate write acknowledgement, bounded register readback, ownership, and RPM convergence checks.
- Recovery responsibility recorded before service control writes, including across abnormal restarts.
- Sleep notification handling, wake reconnection, fresh sensor qualification, and bounded retry delays.
- Mock hardware and service tests; full SMC traces only when explicitly requested.

No GUI, kernel extension, SIP changes, or replacement of macOS thermal services is required.

## Architecture

```text
IOHID chip temperatures + AppleSMC battery temperatures
                         |
                validate fresh samples
                         |
              chip / battery demand policy
                         |
              control session + readback
                         |
                 AppleSMC fan registers

launchd -> foreground serve -> lifecycle supervisor
                            -> sleep/wake notifications
                            -> recovery journal + status IPC
```

The transport accepts only validated fan control keys. Speed limits come from the hardware on each control path. A single controller lock prevents concurrent M1FanControl writers. See [Architecture](docs/ARCHITECTURE.md) for the recovery and service lifecycle.

## Supported Hardware / Compatibility

Control is deliberately restricted to **Apple M1** on **ARM64 macOS**, with a physical fan and the expected SMC and IOHID interfaces. M1 Pro/Max/Ultra, newer chips, Intel Macs, and fanless Macs are outside the supported control scope. Automatic control requires complete named chip sensor groups and both battery temperature keys. Hardware validation covers a single-fan M1 configuration. Discovery and control support multiple fans in software; their physical behavior remains unvalidated.

Development hardware validation covered a fan-equipped M1 MacBook Pro, manual and temperature-triggered acceleration, background operation, stop/restoration, and service management. Physical lid sleep/wake, boot before login, and a complete natural cooling-to-release cycle still need end-to-end validation. Mock tests cover these lifecycle paths. No compatibility or performance measurements are claimed for other systems.

AppleSMC and the IOHID sensor SPI are undocumented or private interfaces that may change between macOS versions. The program rejects unsupported data rather than guessing keys or speed limits. Do not run another fan controller alongside it.

## Installation

Install Apple's Command Line Tools and a current stable Rust toolchain with `rustfmt` and `clippy`. The project uses Rust edition 2024. The only Cargo dependency is `libc`, pinned by `Cargo.lock`.

```sh
xcode-select --install
# Install Rust using the instructions at https://rustup.rs/.
git clone https://github.com/TanhJK728/M1FanControl.git
cd M1FanControl
cargo build --locked --release
./target/release/m1fan --help
```

Run the binary directly from `target/release`, or install the optional service using the procedure below. Building does not install or start a controller. No prebuilt binary is required.

## CLI Usage

```sh
# Read-only inspection
./target/release/m1fan status
./target/release/m1fan monitor --samples 10
./target/release/m1fan preview --samples 10
./target/release/m1fan verify-auto

# Foreground control; Ctrl+C restores automatic management
sudo ./target/release/m1fan max
sudo ./target/release/m1fan run

# Manually started background controller
sudo ./target/release/m1fan start
sudo ./target/release/m1fan daemon-status
sudo ./target/release/m1fan stop

# Explicit recovery, when no controller owns the lock
sudo ./target/release/m1fan auto
```

`monitor` and `preview` run until interrupted when `--samples` is omitted. `preview` shows validated temperatures and policy demand without writing SMC. `daemon-status` reports service/instance identity, uptime, temperatures, RPM, ownership, transitions, power events, and errors.

For a controlled diagnostic session, add `--trace /path/to/new-trace.jsonl`. The destination must be new. Traces contain raw register traffic and runtime measurements; review them before sharing. Normal operation does not create full SMC traces.

## Automatic Thermal Control

| Source | Begin cooling | Request hardware maximum | Clear demand |
| --- | --- | --- | --- |
| Hottest validated chip sensor | 55°C | 70°C | Below 50°C continuously for 30 seconds |
| Hottest validated battery sensor | 35°C | 40°C | Below 35°C continuously for 30 seconds |

An active demand rises linearly from 25% to 100% of the **hardware RPM span**: `minimum + fraction × (maximum − minimum)`. The greater demand wins. Both demands must clear before automatic management is restored. The same policy applies on AC and battery power.

These thresholds are configurable-in-source engineering defaults for early, audible cooling, not Apple temperature limits or a battery lifespan guarantee. Battery keys `TB1T` and `TB2T` follow Stats' sensor mapping; their exact physical locations are not established. This program does not manage charging or set a battery charge limit.

Heating increases the target promptly. Cooling requires a confirmation interval and uses elapsed-time speed reduction with a small-change deadband. Invalid, incomplete, stale, or repeatedly unavailable telemetry triggers recovery and a fresh qualification cycle. Temperature and control behavior are described in [Architecture](docs/ARCHITECTURE.md).

## Background Service & Sleep/Wake Support

Build and test as an ordinary user. Then install the reviewed binary using its SHA-256:

```sh
cargo test --locked
binary_sha=$(shasum -a 256 target/release/m1fan | awk '{print $1}')
sudo ./deploy/install.sh "$PWD/target/release/m1fan" "$binary_sha"
sudo "/Library/Application Support/M1FanControl/enable.sh"
sudo "/Library/Application Support/M1FanControl/status.sh"
```

Installation starts **disabled**. Enabling registers boot startup and starts `m1fan serve` in the foreground under launchd, independent of a Terminal session. The installed binary and scripts live under `/Library/Application Support/M1FanControl`; they do not depend on the checkout. The local hash checks that installation uses the binary you reviewed; it is not an external release signature.

```sh
# Disable boot startup, stop, and verify automatic restoration
sudo "/Library/Application Support/M1FanControl/disable.sh"

# Stop, restore, and remove the managed installation
sudo "/Library/Application Support/M1FanControl/uninstall.sh"
```

`m1fan stop` stops the current instance without changing boot enablement. Use `disable.sh` for persistent disablement. Logs rotate; private management history is retained after uninstall. See [Service management](deploy/README.md) for paths, failure handling, and upgrade instructions.

Sleep notifications gate control and request restoration before acknowledging sleep, with a bounded acknowledgement deadline. Wake and monitoring gaps over five seconds rebuild connections and require automatic ownership plus three fresh healthy samples. Recovery attempts use delays of 5, 15, 30, and 60 seconds, then 60 seconds thereafter. Ordinary errors and SIGINT/SIGTERM/SIGHUP attempt verified restoration. SIGKILL, power loss, and kernel failure cannot execute process cleanup; the service journal preserves pending recovery for a subsequent instance when runtime state survives. Runtime state under `/var/run` is normally cleared across a reboot.

## Development and Testing

```sh
cargo fmt --check
cargo clippy --locked -- -D warnings
cargo test --locked
cargo build --locked --release
```

The default suite uses synthetic hardware readings, deterministic clocks, temporary files, and injected deployment commands. It does not take fan ownership, install services, or exercise physical sleep. The passive native power-notification test is ignored by default. Shell and plist syntax checks are included in the deployment tests and require macOS tools.

Read-only checks on a compatible machine can use `status` and `preview`. Hardware control validation is a separate explicit step. A successful mock suite does not establish physical sleep/wake or compatibility on an untested model.

## Prior Art & Acknowledgments

- [MacFanControl](https://github.com/raminsharifi/MacFanControl): Rust AppleSMC implementation and Apple Silicon control sequences.
- [Stats](https://github.com/exelban/stats): SMC control, IOHID reading, sensor mappings, and lifecycle prior art.
- [macos-smc-fan](https://github.com/agoodkind/macos-smc-fan): research on Apple Silicon fan arbitration and system target overwrites.
- Chromium and Apple interface sources: sensor naming, ABI, event clocks, and power notification semantics.

The [Provenance audit](docs/PROVENANCE.md) separates adapted code, protocol knowledge, and independent implementation, with fixed revisions and comparison methods. Applicable copyright and license texts are in [Third-party notices](THIRD_PARTY_NOTICES.md) and `licenses/`.

## Original Contributions

The independently implemented engineering work includes staged readback and ownership diagnostics, recovery armed before a potentially mutating operation, typed write restrictions, injected hardware/clock tests, dual-source cooling demand, sensor freshness and membership validation, durable recovery responsibility, and a foreground service with bounded wake/restart qualification and protected management transactions.

These contributions build on the prior art above. The SMC transport and parts of sensor acquisition are adapted implementations. [Development history](docs/DEVELOPMENT_HISTORY.md) traces the technical evolution; [Provenance](docs/PROVENANCE.md) gives module-level classifications and differences.

## License

[MIT](LICENSE), with applicable third-party copyrights and license terms retained in [Third-party notices](THIRD_PARTY_NOTICES.md). macOS frameworks and developer tools are supplied separately by their respective providers.
