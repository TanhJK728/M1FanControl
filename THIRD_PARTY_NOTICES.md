# Third-party notices

M1FanControl is MIT-licensed. The following attributions apply to adapted implementation and retained third-party test data. Full upstream license text is included without alteration. Source relationships and estimated scope are detailed in [docs/PROVENANCE.md](docs/PROVENANCE.md).

## Adapted implementation and reused test data

### MacFanControl

- Project: https://github.com/raminsharifi/MacFanControl
- Revision: `de9af6fc481764478ccd927ae00e44ba333cc6cf`.
- Files consulted/adapted: `src/smc.rs`, `src/fan.rs`, `src/control.rs`, `src/temps.rs`.
- Copyright (c) 2026 Ramin Sharifi.
- License: MIT; full text in [licenses/MacFanControl-MIT.txt](licenses/MacFanControl-MIT.txt).
- Scope: low-level SMC transport/ABI/codecs, mode probing, bounded retry/manual sequence and temperature-discovery outline. The fpe2 synthetic test vector is reused; surrounding tests and validation are project implementation.

### Stats

- Project: https://github.com/exelban/stats
- Revision: `ffdf63741af0e801318c129ed29253f4e3679c4f`.
- Files: `SMC/smc.swift`; `Modules/Sensors/reader.m`, `bridge.h`, `values.swift`, `readers.swift`, `popup.swift`.
- Top-level license notice: Copyright (c) 2019 Serhiy Mytrovtsiy.
- Applicable file notices: Copyright © 2021 Serhiy Mytrovtsiy. All rights reserved. (`smc.swift`, `reader.m`, `bridge.h`); Copyright © 2020 Serhiy Mytrovtsiy. All rights reserved. (`values.swift`).
- License: MIT; full text in [licenses/Stats-MIT.txt](licenses/Stats-MIT.txt).
- Scope: mode probing and control/release sequences; client/service caching and periodic IOHID rediscovery. Sensor mappings, IOHID interface facts and sleep/wake behavior also informed the project. The project uses its own validation, state machines, ownership and recovery implementation.

## IOHID reader ancestry

The Stats reader references MenuMeters, which references fermion-star/apple_sensors and the earlier freedomtan/sensors implementation. The audit followed that chain and compared the relevant functions.

- [freedomtan/sensors](https://github.com/freedomtan/sensors/tree/595f675ad3697debbdf966e98ed8ab23ad2bfb6c): BSD-3-Clause.
- Copyright (c) 2016-2018, "freedom" Koan-Sin Tan. All rights reserved.
- Complete original notice retained in [licenses/sensors-BSD-3-Clause.txt](licenses/sensors-BSD-3-Clause.txt) for this historical reader lineage.

The shared IOHID access sequence and fields already appear in that BSD source. MenuMeters' GPL-2.0 implementation was checked separately; no MenuMeters-specific implementation block was identified as copied or translated into the distributed Rust code. Its source is not distributed. The audit does not interpret Stats' MIT notice as relicensing MenuMeters. See the [detailed ancestry assessment](docs/PROVENANCE.md#iohid-ancestry-and-license-check).

## Cargo dependency

`Cargo.lock` pins `libc` 0.2.191, the only third-party Rust dependency.

- Project: https://github.com/rust-lang/libc
- Copyright (c) The Rust Project Developers.
- License: MIT OR Apache-2.0.
- Full upstream texts: [licenses/libc-MIT.txt](licenses/libc-MIT.txt), [licenses/libc-APACHE.txt](licenses/libc-APACHE.txt).

The texts were compared with the downloaded locked crate. macOS system frameworks, Command Line Tools and the Rust toolchain are not bundled.

## Research and interface references

These references contributed protocol/interface knowledge or research context. No substantive implementation copied or translated from them was identified in this release; they are separate from the adapted-code totals above.

- [macos-smc-fan](https://github.com/agoodkind/macos-smc-fan/tree/31a1feae0c4999ebd8cdddfbd90d2c98182091b8), Alexander Goodkind: Apple Silicon fan arbitration, target replacement and testing research. [Upstream MIT license and research notice](https://github.com/agoodkind/macos-smc-fan/blob/31a1feae0c4999ebd8cdddfbd90d2c98182091b8/LICENSE.md).
- [Chromium M1 sensors](https://chromium.googlesource.com/chromium/src/+/123.0.6285.0/components/power_metrics/m1_sensors_mac.mm), Copyright 2021 The Chromium Authors, BSD-style license: CPU Product naming and interface corroboration. Chromium's averaging algorithm is not used.
- [Apple WebKit IOKitSPIMac.h](https://github.com/WebKit/WebKit/blob/283c780414b7c24e4a911c823af665f53a15e8f7/Source/WebCore/PAL/pal/spi/mac/IOKitSPIMac.h), Copyright (C) 2020 Apple Inc. All rights reserved., BSD-2-Clause: private IOKit ABI declarations.
- Apple IOHIDFamily, IOKitUser, XNU, SDK headers and [QA1340](https://developer.apple.com/library/archive/qa/qa1340/_index.html): event time units, notification/ACK semantics and resource ownership. Apple library/kernel implementations, including APSL-licensed source, are not redistributed or ported.
- Apple launchd manuals and [Creating Launch Daemons and Agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html): service configuration semantics.

The independently written project implementation remains under [LICENSE](LICENSE). Keep applicable third-party notices with source or binary redistributions that include these portions.
