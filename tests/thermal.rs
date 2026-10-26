use std::time::Duration;

use m1fan::smc::Error;
use m1fan::thermal::{SpinupWatchdog, ThermalPolicy};

fn at(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

#[test]
fn curve_interpolates_synthetic_hardware_range_and_reaches_endpoints() {
    let policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    for (temperature, expected) in [
        (20.0, 2500.0),
        (40.0, 2500.0),
        (45.0, 3250.0),
        (50.0, 4000.0),
        (60.0, 5500.0),
        (70.0, 7000.0),
        (100.0, 7000.0),
    ] {
        assert_eq!(policy.curve_target(temperature).unwrap(), expected);
    }
    let fractional = ThermalPolicy::new(1200.1, 6000.9).unwrap();
    assert_eq!(fractional.curve_target(100.0).unwrap(), 6000.0);
    assert!(fractional.curve_target(20.0).unwrap() >= 1201.0);
}

#[test]
fn invalid_sensor_or_hardware_data_is_rejected_without_changing_target() {
    for (min, max) in [
        (f32::NAN, 7000.0),
        (0.0, f32::INFINITY),
        (-1.0, 7000.0),
        (1200.0, 1200.0),
        (1200.0, 1000.0),
        (1200.1, 1200.9),
    ] {
        assert!(ThermalPolicy::new(min, max).is_err());
    }
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    let held = policy.sample(50.0, at(0)).unwrap();
    for bad in [f32::NAN, f32::INFINITY, -1.0, 0.0, 130.1] {
        assert!(policy.sample(bad, at(1)).is_err());
        assert_eq!(policy.target(), Some(held));
    }
    assert!(policy.sample(50.0, at(2)).is_ok());
    assert!(policy.sample(50.0, at(1)).is_err());
}

#[test]
fn heating_raises_immediately_and_full_speed_bypasses_small_step_suppression() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    assert_eq!(policy.sample(40.0, at(0)).unwrap(), 2500.0);
    assert_eq!(policy.sample(50.0, at(1)).unwrap(), 4000.0);
    assert_eq!(policy.sample(69.5, at(2)).unwrap(), 6925.0);
    assert_eq!(policy.sample(70.0, at(3)).unwrap(), 7000.0);
}

#[test]
fn small_warming_changes_are_combined_into_fewer_target_updates() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    assert_eq!(policy.sample(50.0, at(0)).unwrap(), 4000.0);
    assert_eq!(policy.sample(50.5, at(1)).unwrap(), 4000.0);
    assert_eq!(policy.sample(51.0, at(2)).unwrap(), 4150.0);
}

#[test]
fn cooling_needs_three_degree_hysteresis_and_ten_continuous_seconds() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    assert_eq!(policy.sample(60.0, at(0)).unwrap(), 5500.0);
    for t in 1..=20 {
        assert_eq!(policy.sample(58.0, at(t)).unwrap(), 5500.0);
    }
    assert_eq!(policy.sample(55.0, at(21)).unwrap(), 5500.0);
    assert_eq!(policy.sample(55.0, at(30)).unwrap(), 5500.0);
    // A warm sample breaks the continuous confirmation period.
    assert_eq!(policy.sample(58.0, at(31)).unwrap(), 5500.0);
    for t in 32..=42 {
        assert_eq!(policy.sample(55.0, at(t)).unwrap(), 5500.0);
    }
    assert_eq!(policy.sample(55.0, at(43)).unwrap(), 5200.0);
    assert_eq!(policy.sample(55.0, at(44)).unwrap(), 4900.0);
    assert_eq!(policy.sample(55.0, at(45)).unwrap(), 4750.0);
}

#[test]
fn cooling_uses_elapsed_time_and_warming_cancels_ramp_immediately() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    policy.sample(70.0, at(0)).unwrap();
    policy.sample(40.0, at(1)).unwrap();
    assert_eq!(policy.sample(40.0, at(11)).unwrap(), 7000.0);
    assert_eq!(
        policy.sample(40.0, Duration::from_millis(11_500)).unwrap(),
        6850.0
    );
    assert_eq!(policy.sample(70.0, at(12)).unwrap(), 7000.0);
    assert_eq!(policy.sample(40.0, at(13)).unwrap(), 7000.0);
}

#[test]
fn cooling_reaches_floor_despite_last_step_under_deadband() {
    let mut policy = ThermalPolicy::new(1200.0, 6200.0).unwrap();
    policy.sample(41.0, at(0)).unwrap();
    policy.sample(35.0, at(1)).unwrap();
    policy.sample(35.0, at(11)).unwrap();
    assert_eq!(policy.sample(35.0, at(12)).unwrap(), 2450.0);
    assert_eq!(policy.target(), Some(policy.curve_target(35.0).unwrap()));
}

#[test]
fn narrow_range_accumulates_elapsed_rate_budget_instead_of_stalling() {
    let mut policy = ThermalPolicy::new(1000.0, 2000.0).unwrap();
    policy.sample(70.0, at(0)).unwrap();
    policy.sample(40.0, at(1)).unwrap();
    policy.sample(40.0, at(11)).unwrap();
    assert_eq!(policy.sample(40.0, at(12)).unwrap(), 2000.0);
    assert_eq!(policy.sample(40.0, at(13)).unwrap(), 1900.0);
    for t in 14..=26 {
        policy.sample(40.0, at(t)).unwrap();
    }
    assert_eq!(policy.target(), Some(1250.0));
}

#[test]
fn target_changes_cannot_restart_stalled_fan_grace_period() {
    let mut watchdog = SpinupWatchdog::new(at(0), 3000.0).unwrap();
    for t in 1..30 {
        watchdog
            .sample(at(t), 3000.0 + t as f32 * 100.0, 1000.0)
            .unwrap();
    }
    assert!(matches!(
        watchdog.sample(at(30), 6000.0, 1000.0),
        Err(Error::Control(_))
    ));
}

#[test]
fn recent_lower_target_avoids_false_spinup_alarm_then_expires() {
    let mut watchdog = SpinupWatchdog::new(at(0), 6000.0).unwrap();
    watchdog.sample(at(29), 2000.0, 2000.0).unwrap();
    watchdog.sample(at(30), 6000.0, 2000.0).unwrap();
    watchdog.sample(at(59), 6000.0, 2000.0).unwrap();
    assert!(watchdog.sample(at(60), 6000.0, 2000.0).is_err());
}

#[test]
fn spinup_uses_unchanged_target_across_window_boundary_and_validates_samples() {
    let mut watchdog = SpinupWatchdog::new(at(0), 6000.0).unwrap();
    watchdog.sample(at(30), 6000.0, 4800.0).unwrap();
    watchdog.sample(at(100), 6000.0, 6000.0).unwrap();
    assert!(watchdog.sample(at(101), 6000.0, 4799.0).is_err());
    assert!(watchdog.sample(at(99), 6000.0, 6000.0).is_err());
    assert!(watchdog.sample(at(102), f32::NAN, 6000.0).is_err());
    assert!(watchdog.sample(at(102), 6000.0, f32::NAN).is_err());
}

#[test]
fn telemetry_gap_interrupts_continuous_cooling_confirmation() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    policy.sample(70.0, at(0)).unwrap();
    policy.sample(40.0, at(1)).unwrap();
    policy.telemetry_gap();
    assert_eq!(policy.sample(40.0, at(9)).unwrap(), 7000.0);
    assert_eq!(policy.sample(40.0, at(18)).unwrap(), 7000.0);
    assert_eq!(policy.sample(40.0, at(19)).unwrap(), 7000.0);
    assert_eq!(policy.sample(40.0, at(20)).unwrap(), 6700.0);
}

#[test]
fn saturation_does_not_leave_an_obsolete_hot_temperature_hysteresis_anchor() {
    let mut policy = ThermalPolicy::new(1000.0, 7000.0).unwrap();
    policy.sample(100.0, at(0)).unwrap();
    policy.sample(70.0, at(1)).unwrap();
    for t in 2..=20 {
        assert_eq!(policy.sample(69.0, at(t)).unwrap(), 7000.0);
    }
    policy.sample(67.0, at(21)).unwrap();
    policy.sample(67.0, at(31)).unwrap();
    assert_eq!(policy.sample(67.0, at(32)).unwrap(), 6700.0);
}
