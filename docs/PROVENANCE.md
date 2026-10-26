# Code provenance

This audit classifies the v0.5.0 public source by actual implementation relationships. The transport contains adapted code; the surrounding control, validation, thermal policy and service machinery contain substantial independent implementation. A translation or refactor can still be adapted code. Shared protocol constants and required API declarations do not establish copied implementation by themselves.

## Categories and method

- **A — Direct reuse:** unchanged or minimally changed expression or test data.
- **B — Adapted implementation:** translated, refactored, or structurally derived implementation.
- **C — Conceptual/interface reference:** protocol facts, mappings, algorithms or interface semantics used by independently written code.
- **D — Original implementation:** project-specific implementation confirmed by source comparison.

The review read each final module, corresponding functions in the fixed reference revisions, and the development descriptions. It compared whitespace/comment-free tokens, identifier-normalized tokens, function ordering, data structures, error handling, ownership, and timing. Token candidates were manually examined: common FFI declarations, imports and standard Rust boilerplate were not counted as copied functions. Cross-language relationships were reviewed explicitly. Existing notices were treated as leads, not as sufficient evidence.

The counts below are **inclusive review footprints in the current code**, not an exact number of copied statements. A B region can contain independent checks or formatting. Overlapping source attributions are counted once in the union. The private comparison artifacts and device records are not distributed.

## Fixed references

| Reference | Revision | Files used in the comparison |
| --- | --- | --- |
| [MacFanControl](https://github.com/raminsharifi/MacFanControl/tree/de9af6fc481764478ccd927ae00e44ba333cc6cf) | `de9af6fc481764478ccd927ae00e44ba333cc6cf` | `src/smc.rs`, `fan.rs`, `control.rs`, `temps.rs`, `curve.rs` and application code |
| [Stats](https://github.com/exelban/stats/tree/ffdf63741af0e801318c129ed29253f4e3679c4f) | `ffdf63741af0e801318c129ed29253f4e3679c4f` | `SMC/smc.swift`, `Modules/Sensors/{reader.m,bridge.h,values.swift,readers.swift,popup.swift}` |
| [macos-smc-fan](https://github.com/agoodkind/macos-smc-fan/tree/31a1feae0c4999ebd8cdddfbd90d2c98182091b8) | `31a1feae0c4999ebd8cdddfbd90d2c98182091b8` | SMC/ABI/codecs, `FanController.swift`, research and testing documents |
| [freedomtan/sensors](https://github.com/freedomtan/sensors/tree/595f675ad3697debbdf966e98ed8ab23ad2bfb6c) | `595f675ad3697debbdf966e98ed8ab23ad2bfb6c` | `sensors/sensors.m`, `LICENSE`; IOHID prior-art ancestry |
| [Chromium M1 sensors](https://chromium.googlesource.com/chromium/src/+/123.0.6285.0/components/power_metrics/m1_sensors_mac.mm) | tag `123.0.6285.0`, blob `6f225c9c491bf223374ec6851b3521f28989c49f` | `components/power_metrics/m1_sensors_mac.mm` |
| [Apple IOKitUser](https://github.com/apple-oss-distributions/IOKitUser/blob/323ead896d04424f87184d8f6ff0cce811aab106/pwr_mgt.subproj/IOPMLib.c) | `323ead896d04424f87184d8f6ff0cce811aab106` | `pwr_mgt.subproj/IOPMLib.c` |
| [Apple XNU](https://github.com/apple-oss-distributions/xnu/tree/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea) | `f6217f891ac0bb64f3d375211650a4c1ff8ca1ea` | `iokit/Kernel/{IOPMrootDomain.cpp,IOServicePM.cpp}` |

Apple's [QA1340](https://developer.apple.com/library/archive/qa/qa1340/_index.html), SDK IOPM/IOKit/Core Foundation headers, [WebKit's `IOKitSPIMac.h`](https://github.com/WebKit/WebKit/blob/283c780414b7c24e4a911c823af665f53a15e8f7/Source/WebCore/PAL/pal/spi/mac/IOKitSPIMac.h), [IOHIDFamily event timestamp definitions](https://github.com/apple-oss-distributions/IOHIDFamily/blob/777ccd9698845aadf711e32d843c8c9b777431d9/HID/Headers/HIDEvent.h), and launchd documentation provided C-level ABI and semantic checks. No Apple library/kernel function or Chromium aggregation algorithm was identified as copied or translated.

## Module-level assessment

| Module | Categories | Source relationship and project-specific work |
| --- | --- | --- |
| `smc.rs` | B, C, D; A test datum | Transport/ABI/codecs follow MacFanControl, cross-checked with Stats. Exact-size values, initialized padding, response validation, explicit read/write capabilities, restricted writes and tracing integration are project additions. One synthetic fpe2 test vector is reused. |
| `fan.rs` | B, C, D | Lower/upper-case mode probing follows both references. Fan register names are C. Error-preserving discovery, strict metadata validation and dynamic limits are D. |
| `control.rs` | B, C, D | Bounded retry/manual sequence follows MacFanControl; mode/Ftst release follows Stats. Staged observation, ownership-loss diagnostics, partial-write responsibility, update validation and recovery lifecycle are D. macos-smc-fan research is C. |
| `temps.rs` | B, C, D | Enumerate/filter/discover outline follows MacFanControl. Retained per-sensor errors and strict sampling are D. Raw key temperatures do not drive chip demand. |
| `chip_temps.rs` | B, C, D | Cached client/services and periodic rediscovery adapt Stats. Product groups and IOHID interfaces are C. Strict name grammar, complete/frozen membership, finite values, timestamps, freshness, ownership wrappers and failure handling are D. |
| `battery.rs` | C, D | `TB1T`/`TB2T` labels follow Stats. Typed complete-pair validation, maximum selection and error handling are D. Physical locations are not asserted. |
| `thermal.rs`, `demand.rs` | D with C prior art | Piecewise curves and hysteresis are established ideas. These threshold states, independent release timers, elapsed-time descent, deadband handling and rolling RPM watchdog are project implementations; no reference curve implementation was identified as translated. |
| `adaptive.rs`, `supervisor.rs`, `automation.rs` | D with C lifecycle prior art | Project state machines, mock interfaces, sensor qualification, persistent membership, reconnect epochs, power generations and bounded retry. Stats already demonstrates sleep release/wake restoration and HID reset; those ideas are acknowledged. |
| `power.rs` | C, D | Apple API declarations/message semantics are C. The dedicated run loop, bounded ACK/event queues, cycle-aware deduplication, generation gate and three-second ACK deadline are D. |
| `daemon.rs` | D | Project detached startup handshake, peer-checked IPC, ready/stop protocol, bounded clients and log rotation. Common Unix process/socket idioms are not claimed as inventions. |
| `service_state.rs`, `service_smc.rs` | D | Durable `control_pending`, atomic intent, crash backoff, protected file checks and per-write service gate. |
| `platform.rs`, `main.rs`, `trace.rs`, `lib.rs` | D with C platform interfaces | CLI composition, signals, lock, observer/status logic and trace format are project implementations. System ABI/standard-library signatures are C. |
| `deploy/` | D with C launchd semantics | Original receipt/hash/ownership checks, bounded management transactions, stable readiness, disable/restore/uninstall ordering and tests. Apple's launchd settings are interface knowledge. |
| `tests/` and inline tests | D except the A datum above | Synthetic mocks and fault-injection scenarios are project tests. Fixture values are synthetic, not captured device readings. |

## Adapted regions and approximate scale

Symbols are the stable identifiers; line ranges below refer to the public snapshot reviewed for this release.

| File / symbols | Public line ranges | B footprint | Principal attribution |
| --- | --- | ---: | --- |
| `smc.rs`: `Value` codecs, `encode_rpm`, `Packet`, `response`, native FFI, `open`, call core, `info`, `read`, `keys`, write core, `Drop` | 48–90, 92–115, 157–174, 203–230, 266–287, 318–349, 368–381, 391–415, 423–445, 447–470, 482–502, 506–510 | 279 lines | MacFanControl; Stats protocol cross-check |
| `fan.rs`: `plan` mode-key probing | 72–82 | 11 lines | MacFanControl + Stats |
| `control.rs`: `retry_write`, release ordering, manual/Ftst fallback | 37–86, 229–240, 392–405 | 76 lines | MacFanControl retry/manual; Stats release/manual |
| `temps.rs`: `Temperatures::discover` | 15–42 | 28 lines | MacFanControl |
| `chip_temps.rs`: `REDISCOVER`, `Hid` cache state, initialization and rediscovery | 311, 449–457, 507–515, 551–557 | 26 lines | Stats cached reader |
| **Union of B regions** | No double counting | **420 lines** | Includes independently added statements inside adapted regions |

The one A test-data line is `smc.rs:576`: the synthetic `[0x13, 0x88] → 1250` fpe2 vector also present in MacFanControl. No substantial verbatim implementation function was identified. Exact matching of required FFI declarations does not imply that an interface itself is an original contribution.

Per-source B footprints are approximately **382 lines for MacFanControl** and **63 lines for Stats**. They overlap by **25 lines**, giving the 420-line union. macos-smc-fan contributed research context on arbitration and target overwrites; **no A/B implementation reuse was identified** from that project. Its timing/retry controller was compared explicitly and was not treated as the source of this controller.

The public `src/*.rs` contains **9,464 physical lines**, including comments, whitespace and inline tests. B regions occupy roughly **4.4%** of that denominator, plus one A test-data line. This is an adapted-region estimate, not a precise copied-code percentage. Across `src/` plus the 5,627 integration-test lines, the A+B footprint is roughly **2.8%** of 15,091 Rust lines.

The twelve application modules `adaptive`, `automation`, `daemon`, `demand`, `lib`, `main`, `platform`, `service_smc`, `service_state`, `supervisor`, `thermal` and `trace` have **5,672 lines** reviewed as project implementations using ordinary platform interfaces: approximately **60% of `src/`**. They provide a conservative independently implemented module footprint. The remaining roughly 36% outside these modules and A/B includes substantial original code in mixed modules as well as C-level protocol/API material. It is not valid to label all of that remainder either copied or original. Adding the independently written integration tests raises the conservative original-module/test footprint to approximately **75% of all Rust lines**. Deployment code is additional independent work outside this Rust-only denominator.

## IOHID ancestry and license check

Stats' `bridge.h` cites [MenuMeters](https://github.com/yujitach/MenuMeters/blob/289351048c0dac005aa8f044614094525f10e79d/hardware_reader/applesilicon_hardware_reader.m), whose reader cites [fermion-star/apple_sensors](https://github.com/fermion-star/apple_sensors/blob/c6c160ba878642f6342940777c4a72170e9aa780/temp_sensor.m), which credits freedomtan/sensors. This chain was checked instead of assuming Stats' top-level MIT notice settles every ancestor.

The earlier freedomtan source is BSD-3-Clause; its license is retained at [licenses/sensors-BSD-3-Clause.txt](../licenses/sensors-BSD-3-Clause.txt). MenuMeters' GPL-2.0 reader was examined separately. Its dictionary-building implementation, null-event continuation behavior and distinctive comments are not carried into the Rust reader. The common IOHID API sequence and fields already appear in the earlier BSD source. The project classifies those interface facts as C. Stats' cache and 60-second rediscovery change was independently located in [commit d85c263](https://github.com/exelban/stats/commit/d85c26351cb16335d6af2219f19e543016d4d416) and is conservatively B; the Rust reader's validation and lifetime/error machinery are D. No MenuMeters-specific GPL implementation expression was identified in the distributed code.

## Original contributions relative to the references

| Contribution | Category | Concrete difference |
| --- | --- | --- |
| Hardware-bound discovery and typed write capability | D around B/C transport | Requires actual metadata/limits and reports missing controls; no guessed fan maximum or arbitrary key writer. |
| Staged SMC observation and diagnostics | D | Separates transport/firmware acknowledgement, mode match, target match, subsequent ownership loss and actual RPM response. Delayed readback has a finite observation budget. |
| Recovery protocol | D around small B sequences | Arms responsibility before writes, validates recovery across all discovered fans, preserves partial-write responsibility, and avoids treating a moving automatic target as failure. |
| Mock hardware and clock test system | D | Exercises delayed state, overwrites, interrupted and partial operations, unavailable sensors, power generations, restart responsibilities and management failures. |
| Named chip data qualification | D with B/C acquisition | Validates complete sensor groups, persistent membership and event timing; uses the maximum accepted chip reading rather than Chromium's per-group means. |
| Dual-source cooling | D with C sensor mapping | Independent chip/battery demand and release timers, elapsed-time smoothing, deadband and rolling response monitoring. |
| Service and wake recovery | D with C lifecycle prior art | Native IOPM ACK deadlines, interruption generations, fresh requalification and bounded retries; Stats uses application-level workspace notifications and saved settings. |
| Persistent recovery and management | D | Journal before first possible write, installed intent checked at each write, protected receipts, stable IPC readiness and verified automatic recovery before removal. |

Algorithmic ideas such as hysteresis, linear interpolation, backoff and sleep notifications are established techniques. The original contribution is this project's concrete implementation and integration, with the behavioral differences above.

## Licenses and dependencies

See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for exact copyrights and full license paths. The project license is MIT. The only Cargo dependency is `libc` **0.2.191**, licensed **MIT OR Apache-2.0**; both upstream texts match the downloaded locked crate. No upstream repository clone, macOS framework, toolchain or binary is distributed.
