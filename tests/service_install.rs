//! Deployment tests use temporary directories, the current ordinary UID and
//! injected launchctl replies. No system service or hardware is accessed.
use std::path::Path;
use std::process::Command;

#[test]
fn deployment_transaction_mock_tests() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new("/usr/bin/perl")
        .arg(root.join("deploy/tests/install.t"))
        .output()
        .expect("run system Perl deployment tests");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deployment_shell_and_plist_syntax() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for action in [
        "install",
        "enable",
        "disable",
        "status",
        "activate",
        "deactivate",
        "uninstall",
    ] {
        let output = Command::new("/bin/bash")
            .arg("-n")
            .arg(root.join(format!("deploy/{action}.sh")))
            .output()
            .expect("check shell syntax");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new("/usr/bin/plutil")
        .arg("-lint")
        .arg(root.join("deploy/local.m1fan.control.plist"))
        .output()
        .expect("check plist syntax");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
