# Development history

This is a technical summary of the project's local R1–R6 development. The public repository begins with a new, reviewed source snapshot. Private machine inventories, raw traces, measurements, deployment records and historical build directories are excluded. Source attribution is documented separately in [PROVENANCE.md](PROVENANCE.md).

## R1 — Minimal direct SMC CLI

The initial implementation established `status`, `monitor`, `max` and `auto`. It used AppleSMC through IOKit, discovered fan keys and RPM limits dynamically, restricted writable keys, and isolated the transport behind Mock interfaces. Manual control ran in the foreground with signal and unwind recovery. The initial checks covered building, Mock behavior and read-only hardware access.

MacFanControl and Stats supplied protocol, encoding and control-sequence references. The project added its own capability-separated interface, validation rules, explicit failure handling and restoration checks. Protocol-derived implementation remains attributed to its sources.

## R2 — Observable control and bounded readback

An early control attempt exposed a combined mode/target mismatch error that could not distinguish write failure, delayed state, or later state replacement. The source review identified an immediate single-read verification weakness and insufficient diagnostic separation. The original output did not identify the device-level cause conclusively.

Version 0.2.0 separated mode and target write acknowledgments from their observations. It introduced bounded, interruptible observation windows, latched previously observed state, distinct ownership-loss errors and opt-in wire tracing. Recovery remained bounded and was verified independently. New Mock cases covered delayed reads, overwritten state, partial writes and interrupted operation. Public research on Apple Silicon fan control informed the investigation without establishing that any particular macOS process caused the original failure.

## R3 — Manual control hardware acceptance

Controlled hardware use confirmed that the manual target could be applied, actual fan speed could rise and remain elevated, and interruption could return control to macOS. This established the manual control and recovery baseline used by subsequent development. Raw samples and device-specific measurements remain private.

## R4 — Named chip temperatures and manual background operation

Version 0.3.0 added an IOHID temperature source with explicit chip-product selection, event freshness checks and stable sensor-set validation. Read-only preview and a temperature-driven foreground controller separated policy behavior from hardware writes.

A chip-only policy introduced hysteresis, delayed decreases, rate limiting, write suppression and an RPM watchdog. Detached workers added a startup handshake, private IPC, status queries, verified stop replies and bounded logs. Follow-up hardware acceptance confirmed temperature-driven control, operation after Terminal closure and recovery after stopping. The chip-only library components remain tested; the current automatic CLI uses the later dual-source policy.

## R5 — Independent chip and battery demand

Version 0.4.0 added the reference-mapped `TB1T` and `TB2T` battery readings and independent demand timers. Chip demand starts at 55°C and clears after 30 continuous seconds below 50°C. Battery demand starts at 35°C and clears after 30 continuous seconds below 35°C. The corresponding full-speed thresholds are 70°C and 40°C.

The larger active demand determines the target. When both clear, the controller verifies automatic mode and continues monitoring. The policy applies equally on AC and battery power. Sensor errors interrupt continuous-cooling confirmation. The project does not set a charging limit or claim a measured battery-lifetime benefit.

This phase also introduced deployment templates and management helpers. The controller initially latched control off after a long sampling gap or fatal fault, requiring an explicit restart. Mock coverage verified dual-source transitions, dynamic control, telemetry faults, release and deployment transactions. Follow-up hardware acceptance established dual-source takeover and normal stop recovery.

## R6 — Managed startup and reconnect lifecycle

Version 0.5.0 preserved the dual-source policy and added a system LaunchDaemon that directly manages the foreground `serve` process. Installation uses a stable system path, records installed-file hashes and starts disabled until explicitly enabled. Enable verifies versioned live IPC readiness; disable and uninstall restore and verify macOS control before completing removal.

Native IOPM notifications introduced a sleep gate, generation-based interruption, bounded sleep acknowledgment and full-wake reconnection. The outer automation layer rebuilds SMC/HID connections after wake, cancelled sleep or a sampling gap, verifies automatic ownership and requires fresh healthy data before resuming control. Bounded retry delays replace permanent lockout.

A private journal records recovery responsibility before possible writes and preserves it across abnormal process exits. Per-write installed-service intent checks prevent re-acquisition while disabling. Application retry state and launchd throttling limit restart frequency. The implementation distinguishes a recorded restoration obligation from evidence that automatic mode was actually observed.

Hardware acceptance covered managed thermal control, explicit stop, disable, uninstall, reinstallation and enable after a clean stop. A deployment wait-budget issue discovered during re-enablement was corrected with a shared overall deadline and sufficient allowance for the kickstart client. Its internal launchd cause was not established by a system trace.

## Validation boundary

The current implementation has automated coverage for sleep events, wake reconnection, sensor qualification, retriggering, retry escalation, conflicting processes, partial writes, persistent recovery responsibility and management failures. Hardware testing has confirmed fan control, normal recovery and the managed installation lifecycle on an M1 system.

Physical lid-close/wake sequences, system startup before user login, natural cooling through both release thresholds and deliberate crash/restart recovery remain separate hardware acceptance items. Their state-machine tests establish software behavior under the injected conditions. No broad Apple Silicon compatibility or benchmark result is inferred from them.
