//! Ordinary-user command gates are tested without opening a runtime directory,
//! contacting AppleSMC, or running a privileged background process.
use m1fan::daemon;
use m1fan::smc::Error;

#[test]
fn ordinary_user_control_commands_fail_before_runtime_access() {
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for result in [daemon::start(None), daemon::stop(), daemon::status()] {
        assert!(matches!(result, Err(Error::Permission(_))));
    }
}
