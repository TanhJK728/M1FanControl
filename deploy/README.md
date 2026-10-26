# System service deployment

The installer creates a system LaunchDaemon for the foreground `m1fan serve` process. It installs the built binary and management helpers under `/Library/Application Support/M1FanControl`; the service does not depend on the clone directory, a Terminal session or a logged-in user.

## Build and install

Use a compatible ARM64 Mac with Apple M1, Xcode Command Line Tools and a Rust toolchain supporting this package. Build and test as an ordinary user from a fresh clone. Review the project compatibility notes before enabling control.

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build --release

M1FAN_BINARY="$PWD/target/release/m1fan"
M1FAN_SHA256="$(shasum -a 256 "$M1FAN_BINARY" | awk '{print $1}')"
sudo ./deploy/install.sh "$M1FAN_BINARY" "$M1FAN_SHA256"
```

The hash binds installation to the local binary you just reviewed and built. It does not authenticate a third-party download. The installer reads that binary directly and does not run Cargo as root. It checks source type, expected hash, destination ownership, permissions and existing installation state. Installation remains disabled and does not start a controller.

Enable explicitly:

```bash
sudo "/Library/Application Support/M1FanControl/enable.sh"
sudo "/Library/Application Support/M1FanControl/status.sh"
```

Enabling starts temperature monitoring and permits hardware writes when the policy requests cooling. Complete a foreground controller's shutdown or the appropriate background stop before installation. The shared control lock rejects concurrent M1FanControl controllers. Other fan-control applications should also release control before enabling this service.

## Daily commands

| Action | Command |
| --- | --- |
| Enable now and at startup | `sudo "/Library/Application Support/M1FanControl/enable.sh"` |
| Installation, launchd and controller status | `sudo "/Library/Application Support/M1FanControl/status.sh"` |
| Controller status without a deployment transaction | `sudo "/Library/Application Support/M1FanControl/m1fan" daemon-status` |
| Stop this running instance and verify recovery | `sudo "/Library/Application Support/M1FanControl/m1fan" stop` |
| Disable startup, stop and verify automatic mode | `sudo "/Library/Application Support/M1FanControl/disable.sh"` |
| Disable, verify recovery and uninstall managed files | `sudo "/Library/Application Support/M1FanControl/uninstall.sh"` |

`stop` retains installation and startup intent. `enable.sh` starts a cleanly stopped registered job without terminating an already running instance. `disable.sh` is the persistent off switch. `activate.sh` and `deactivate.sh` remain aliases for enable and disable.

The management status command records a transaction and includes raw launchd diagnostics. It does not change fan ownership. Use the controller's `daemon-status` command for frequent live queries.

## Installation and recovery protocol

The installation receipt contains the package version, source binary hash and hashes of every installed payload file and plist. Ownership/type/permission checks and hash checks run before management changes. Unexpected files, altered managed files and unknown installation versions cause a refusal that preserves existing content. A known legacy template is recognized only to provide a clear migration refusal.

Disable first persists disabled intent. It requests a normal IPC stop, removes the launchd registration, obtains the control lock and performs a read-only automatic-mode verification. If necessary, a bounded `auto` helper attempts restoration, followed by another verification. Failure to verify recovery or service removal retains the installation and reports the failure. Uninstall rechecks the receipt, archives it and removes only its registered files after this protocol succeeds.

To restore default macOS fan control while keeping the installation available:

```bash
sudo "/Library/Application Support/M1FanControl/disable.sh"
```

To restore default control and remove the service:

```bash
sudo "/Library/Application Support/M1FanControl/uninstall.sh"
```

If a restoration error is reported, inspect the message and verify that other controllers have stopped. The manual recovery entry point is `sudo m1fan auto`, using the installed binary or a reviewed local build, followed by the read-only `m1fan verify-auto`. The control lock prevents that command from racing an active M1FanControl session.

## Startup, retries and readiness

The plist uses `RunAtLoad=true`, `KeepAlive` for unsuccessful exits, a 60-second `ThrottleInterval` and a 30-second exit timeout. A managed intentional stop exits successfully, while recovery errors remain visible in IPC and the journal.

Application retries use 5, 15, 30 and 60 seconds, then 60 seconds per attempt. Each new monitoring epoch verifies automatic mode and three consecutive healthy sensor snapshots. The cross-instance journal also escalates retries after abnormal exits. These delays coexist with launchd throttling, so actual restart latency can be longer.

Enable shares a 120-second overall deadline across startup and readiness checks. The kickstart client has a 75-second cap within that deadline. Readiness requires three replies from the same instance with the expected version, protocol and service role. Sleep, qualification, faults and backoff clear the live ready flag. A readiness timeout can leave the service enabled and registered; inspect status and use disable to stop it and verify recovery.

## Runtime data and logs

| Location | Contents |
| --- | --- |
| `/Library/LaunchDaemons/local.m1fan.control.plist` | System job definition |
| `/Library/Application Support/M1FanControl` | Stable executable, management helpers, private receipt and enabled marker |
| `/var/run/m1fan` | Private IPC, last status, restart journal and runtime logs |
| `/Library/Application Support/M1FanControl-history` | Private management transactions and archived uninstall receipts |

Normal runtime logs rotate between `daemon.log` and `daemon.log.1`, each bounded to approximately 2 MiB. Deployment history is retained separately. Uninstall preserves runtime logs, management history, source files and unrelated files. Runtime storage is normally cleared by the operating system across a reboot; recovery records there are not a durable archive across power cycles.

The standard service does not enable full SMC tracing. For a dedicated diagnostic session, use the CLI's explicit `--trace` option after orderly service shutdown. Review trace and status output for personal system information before sharing it.

## Testing

The normal Rust test command includes the deployment tests:

```bash
cargo test
```

`tests/service_install.rs` runs the Perl transaction suite, shell syntax checks and plist linting. The transaction suite creates temporary directories owned by the caller and injects launchctl/IPC replies. It never installs or stops the real service or writes SMC. It also checks a bounded timeout against a short-lived test child. No sudo is needed.

An optional native power-notification registration test is ignored by default. Physical lid sleep/wake, startup before login and natural cooling through the release thresholds remain hardware-specific acceptance checks. For an installed service, inspect live status before and after normal user-initiated sleep/wake and confirm monitoring resumes, fresh sample age, expected mode and any reported errors. Avoid changing system sleep state merely to run the automated test suite.

For an upgrade, build and review the new clone first. Use the currently installed version's uninstall entry point, confirm automatic recovery, then install the new reviewed binary and enable it. There is no in-place overwrite or automatic legacy migration.
