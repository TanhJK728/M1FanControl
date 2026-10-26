# Public source release audit

This v0.5.0 release was assembled from an explicit file allowlist into a separate directory and a new Git repository. It contains the complete Rust project, synthetic tests, portable deployment templates, license texts and rewritten technical documentation.

## Verification

- `cargo fmt --check`: passed.
- `cargo clippy --locked -- -D warnings`: passed.
- `cargo test --locked`: 250 passed, one passive native power test ignored by default.
- `cargo build --locked --release`: passed.
- Deployment tests include 29 Perl transaction groups, all shell entry-point syntax checks and plist linting.
- Dependencies and a stable Rust toolchain were downloaded independently; the build used no original development checkout, environment script or prior build cache.
- Tests used synthetic SMC/HID inputs, temporary files and injected service responses. No fan-control writes or service-management operations were performed for this release audit.

The compiler used for this audit was Rust 1.99.1. This is the tested toolchain, not a claimed minimum supported Rust version. The package uses edition 2024; a current stable toolchain is recommended.

## Privacy and provenance

Every distributed file received source/document review, Gitleaks scanning and custom identifier/path/runtime-data checks. Fresh commit metadata and all local Git objects were scanned before publication. Public files contain no identified personal machine inventory, captured temperature/RPM history, local absolute user path, credential or old Git history.

Machine-derived test values were replaced with coherent synthetic values, including their encoded payloads and fault-injection expectations. Runtime path constants, SMC protocol keys and upstream authors' required copyright notices are retained. The commit identity uses a project name and a GitHub noreply address.

Original private development reports, evidence, reference clones, environment files, build outputs, toolchains, installation records, runtime files and audit working material are excluded. Detailed private findings remain outside the repository. The [public file list](PUBLIC_FILES.txt) enumerates the reviewed payload. The [provenance audit](PROVENANCE.md) records adapted implementation, interface references and independent contributions, including the transitive IOHID source review.

The existing development project and installed controller were kept unchanged. No binary release attachment is distributed. Hardware validation limits remain documented in [README](../README.md) and [Development history](DEVELOPMENT_HISTORY.md).
