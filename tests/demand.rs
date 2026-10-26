use std::time::Duration;

use m1fan::demand::ThresholdPolicy;

fn at(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}
fn policy() -> ThresholdPolicy {
    ThresholdPolicy::new(1000.0, 7000.0).unwrap()
}

#[test]
fn inactive_below_thresholds_and_activation_includes_exact_boundaries() {
    let mut p = policy();
    assert_eq!(p.sample(54.999, 34.999, at(0)).unwrap(), None);
    assert!(!p.chip_active());
    assert!(!p.battery_active());
    assert_eq!(p.sample(55.0, 34.999, at(1)).unwrap(), Some(2500.0));
    assert!(p.chip_active());
    assert!(!p.battery_active());
    let mut p = policy();
    assert_eq!(p.sample(54.999, 35.0, at(0)).unwrap(), Some(2500.0));
    assert!(!p.chip_active());
    assert!(p.battery_active());
}

#[test]
fn each_source_uses_its_own_linear_curve_and_hardware_bounds() {
    for (chip, battery, expected) in [
        (55., 30., 2500.),
        (60., 30., 4000.),
        (65., 30., 5500.),
        (70., 30., 7000.),
        (45., 35., 2500.),
        (45., 36., 3400.),
        (45., 37.5, 4750.),
        (45., 39., 6100.),
        (45., 40., 7000.),
        (100., 30., 7000.),
        (45., 60., 7000.),
    ] {
        assert_eq!(
            policy().sample(chip, battery, at(0)).unwrap(),
            Some(expected)
        );
    }
    let mut p = ThresholdPolicy::new(1200.1, 6000.9).unwrap();
    assert_eq!(p.sample(70., 30., at(0)).unwrap(), Some(6000.));
    let lower = p.sample(55., 30., at(20)).unwrap().unwrap();
    assert!(lower >= 1201. && lower <= 6000. && lower.fract() == 0.);
}

#[test]
fn chip_release_is_strictly_below_50_for_thirty_continuous_seconds() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    assert_eq!(p.sample(50., 30., at(100)).unwrap(), Some(2500.));
    assert!(p.chip_active());
    p.sample(49.9, 30., at(101)).unwrap();
    assert_eq!(p.sample(49.9, 30., at(130)).unwrap(), Some(2500.));
    assert_eq!(p.sample(49.9, 30., at(131)).unwrap(), None);
    assert!(!p.chip_active());
}

#[test]
fn chip_release_confirmation_resets_when_temperature_returns_to_50() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    p.sample(49., 30., at(1)).unwrap();
    p.sample(50., 30., at(30)).unwrap();
    p.sample(49., 30., at(31)).unwrap();
    assert!(p.sample(49., 30., at(60)).unwrap().is_some());
    assert_eq!(p.sample(49., 30., at(61)).unwrap(), None);
}

#[test]
fn battery_release_is_below_35_for_thirty_continuous_seconds() {
    let mut p = policy();
    p.sample(40., 35., at(0)).unwrap();
    p.sample(40., 34.9, at(1)).unwrap();
    p.sample(40., 35., at(30)).unwrap();
    p.sample(40., 34.9, at(31)).unwrap();
    assert!(p.sample(40., 34.9, at(60)).unwrap().is_some());
    assert_eq!(p.sample(40., 34.9, at(61)).unwrap(), None);
    assert!(!p.battery_active());
}

#[test]
fn inactive_hysteresis_band_does_not_reactivate_chip_after_release() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    p.sample(49., 30., at(1)).unwrap();
    p.sample(49., 30., at(31)).unwrap();
    assert_eq!(p.sample(54.9, 30., at(32)).unwrap(), None);
    assert_eq!(p.sample(55., 30., at(33)).unwrap(), Some(2500.));
}

#[test]
fn higher_active_source_wins_and_each_reason_releases_independently() {
    let mut p = policy();
    assert_eq!(p.sample(60., 39., at(0)).unwrap(), Some(6100.));
    assert!(p.chip_active());
    assert!(p.battery_active());
    p.sample(49., 39., at(1)).unwrap();
    assert_eq!(p.sample(49., 39., at(31)).unwrap(), Some(6100.));
    assert!(!p.chip_active());
    assert!(p.battery_active());
    p.sample(49., 34., at(32)).unwrap();
    assert_eq!(p.sample(49., 34., at(62)).unwrap(), None);
    assert!(!p.chip_active());
    assert!(!p.battery_active());
}

#[test]
fn second_source_can_activate_while_first_releases_without_automatic_gap() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    p.sample(49., 30., at(1)).unwrap();
    assert_eq!(p.sample(49., 40., at(31)).unwrap(), Some(7000.));
    assert!(!p.chip_active());
    assert!(p.battery_active());
}

#[test]
fn chip_or_battery_heating_raises_immediately_and_maximum_is_always_reachable() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    assert_eq!(p.sample(60., 30., at(1)).unwrap(), Some(4000.));
    assert_eq!(p.sample(60., 39.95, at(2)).unwrap(), Some(6955.));
    assert_eq!(p.sample(60., 40., at(3)).unwrap(), Some(7000.));
}

#[test]
fn tiny_target_increases_are_combined_until_deadband_is_reached() {
    let mut p = policy();
    p.sample(55., 30., at(0)).unwrap();
    assert_eq!(p.sample(55.2, 30., at(1)).unwrap(), Some(2500.));
    assert_eq!(p.sample(55.4, 30., at(2)).unwrap(), Some(2620.));
}

#[test]
fn decline_requires_ten_seconds_and_uses_elapsed_rate_without_tick_counting() {
    let mut p = policy();
    p.sample(70., 30., at(0)).unwrap();
    p.sample(55., 30., at(1)).unwrap();
    assert_eq!(p.sample(55., 30., at(10)).unwrap(), Some(7000.));
    assert_eq!(p.sample(55., 30., at(11)).unwrap(), Some(7000.));
    assert_eq!(
        p.sample(55., 30., Duration::from_millis(11_500)).unwrap(),
        Some(6850.)
    );
    assert_eq!(p.sample(55., 30., at(12)).unwrap(), Some(6700.));
    assert_eq!(p.sample(55., 30., at(12)).unwrap(), Some(6700.));
}

#[test]
fn warming_below_held_target_breaks_decline_confirmation() {
    let mut p = policy();
    p.sample(70., 30., at(0)).unwrap();
    p.sample(55., 30., at(1)).unwrap();
    assert_eq!(p.sample(60., 30., at(10)).unwrap(), Some(7000.));
    p.sample(60., 30., at(11)).unwrap();
    assert_eq!(p.sample(60., 30., at(21)).unwrap(), Some(7000.));
    assert_eq!(p.sample(60., 30., at(22)).unwrap(), Some(6700.));
}

#[test]
fn elapsed_time_at_the_current_demand_is_never_banked_for_a_future_decline() {
    let mut p = policy();
    p.sample(70., 30., at(0)).unwrap();
    p.sample(60., 30., at(1)).unwrap();
    p.sample(60., 30., at(11)).unwrap();
    for t in 12..=21 {
        p.sample(60., 30., at(t)).unwrap();
    }
    assert_eq!(p.target(), Some(4000.));
    assert_eq!(p.sample(60., 30., at(1000)).unwrap(), Some(4000.));
    // A lower goal starts a fresh confirmation instead of spending 979 idle seconds.
    assert_eq!(p.sample(55., 30., at(1001)).unwrap(), Some(4000.));
    assert_eq!(p.sample(55., 30., at(1011)).unwrap(), Some(4000.));
    assert_eq!(p.sample(55., 30., at(1012)).unwrap(), Some(3700.));
}

#[test]
fn sub_deadband_plateau_does_not_accumulate_unbounded_ramp_credit() {
    let mut p = policy();
    p.sample(60., 30., at(0)).unwrap();
    p.sample(59.8, 30., at(1)).unwrap();
    p.sample(59.8, 30., at(11)).unwrap();
    p.sample(59.8, 30., at(1000)).unwrap();
    assert_eq!(p.target(), Some(4000.));
    // The suppressed 60 RPM plateau contributes no credit; only this second counts.
    assert_eq!(p.sample(55., 30., at(1001)).unwrap(), Some(3700.));
}

#[test]
fn floor_endpoint_can_be_reached_with_a_last_step_smaller_than_deadband() {
    let mut p = ThresholdPolicy::new(1200., 6200.).unwrap();
    assert_eq!(p.sample(55.2, 30., at(0)).unwrap(), Some(2500.));
    p.sample(55., 30., at(1)).unwrap();
    p.sample(55., 30., at(11)).unwrap();
    assert_eq!(p.sample(55., 30., at(12)).unwrap(), Some(2450.));
}

#[test]
fn narrow_hardware_span_can_progress_through_deadband() {
    let mut p = ThresholdPolicy::new(1000., 2000.).unwrap();
    p.sample(70., 30., at(0)).unwrap();
    p.sample(55., 30., at(1)).unwrap();
    p.sample(55., 30., at(11)).unwrap();
    assert_eq!(p.sample(55., 30., at(12)).unwrap(), Some(2000.));
    assert_eq!(p.sample(55., 30., at(13)).unwrap(), Some(1900.));
    for t in 14..=26 {
        p.sample(55., 30., at(t)).unwrap();
    }
    assert_eq!(p.target(), Some(1250.));
}

#[test]
fn all_reasons_releasing_overrides_any_target_smoothing_state() {
    let mut p = policy();
    p.sample(70., 40., at(0)).unwrap();
    p.sample(49., 34., at(1)).unwrap();
    assert_eq!(p.sample(49., 34., at(31)).unwrap(), None);
    assert_eq!(p.target(), None);
}

#[test]
fn telemetry_gap_clears_release_and_decline_timers_but_preserves_reasons() {
    let mut p = policy();
    p.sample(70., 40., at(0)).unwrap();
    p.sample(49., 34., at(1)).unwrap();
    p.telemetry_gap();
    assert!(p.chip_active());
    assert!(p.battery_active());
    assert_eq!(p.target(), Some(7000.));
    assert_eq!(p.sample(49., 34., at(29)).unwrap(), Some(7000.));
    assert_eq!(p.sample(49., 34., at(39)).unwrap(), Some(7000.));
    assert_eq!(p.sample(49., 34., at(40)).unwrap(), Some(6700.));
    assert!(p.sample(49., 34., at(58)).unwrap().is_some());
    assert_eq!(p.sample(49., 34., at(59)).unwrap(), None);
}

#[test]
fn invalid_readings_and_backward_time_clear_timers_without_changing_request() {
    for (chip, battery) in [
        (f32::NAN, 30.),
        (60., f32::NAN),
        (0., 30.),
        (131., 30.),
        (60., 0.),
        (60., 80.1),
        (f32::INFINITY, 30.),
    ] {
        let mut p = policy();
        p.sample(70., 40., at(0)).unwrap();
        p.sample(49., 34., at(1)).unwrap();
        assert!(p.sample(chip, battery, at(29)).is_err());
        assert_eq!(p.target(), Some(7000.));
        assert!(p.chip_active());
        assert!(p.battery_active());
        assert_eq!(p.sample(49., 34., at(30)).unwrap(), Some(7000.));
        assert!(p.sample(49., 34., at(59)).unwrap().is_some());
        assert_eq!(p.sample(49., 34., at(60)).unwrap(), None);
    }
    let mut p = policy();
    p.sample(70., 30., at(10)).unwrap();
    assert!(p.sample(50., 30., at(9)).is_err());
    assert_eq!(p.target(), Some(7000.));
}

#[test]
fn invalid_hardware_ranges_are_rejected() {
    for (min, max) in [
        (f32::NAN, 7000.),
        (0., f32::INFINITY),
        (-1., 7000.),
        (1200., 1200.),
        (1200., 1000.),
        (1200.1, 1200.9),
    ] {
        assert!(ThresholdPolicy::new(min, max).is_err());
    }
}

#[test]
fn identical_temperature_stream_has_identical_policy_for_both_power_sources() {
    // Neither instance receives a supply argument: AC/battery only affects telemetry.
    let (mut ac, mut battery_power) = (policy(), policy());
    for (t, chip, battery) in [
        (0, 40., 30.),
        (1, 55., 30.),
        (2, 60., 36.),
        (3, 70., 40.),
        (4, 49., 34.),
        (20, 49., 34.),
        (34, 49., 34.),
        (35, 60., 30.),
    ] {
        assert_eq!(
            ac.sample(chip, battery, at(t)).unwrap(),
            battery_power.sample(chip, battery, at(t)).unwrap()
        );
        assert_eq!(ac.chip_active(), battery_power.chip_active());
        assert_eq!(ac.battery_active(), battery_power.battery_active());
    }
}
