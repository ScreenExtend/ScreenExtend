mod live;
mod ring;
mod routing;

mod gain {
    use crate::macos_utils::audio::legacy::playthrough::{drift_trim, ramp};
    use crate::macos_utils::audio::legacy::volume_proxy::compute_gain;

    #[test]
    fn mute_forces_zero_gain_regardless_of_scalar() {
        assert_eq!(compute_gain(1.0, true), 0.0);
        assert_eq!(compute_gain(0.3, true), 0.0);
    }

    #[test]
    fn unmuted_gain_is_the_clamped_scalar() {
        assert_eq!(compute_gain(0.5, false), 0.5);
        assert_eq!(compute_gain(1.5, false), 1.0);
        assert_eq!(compute_gain(-0.2, false), 0.0);
    }

    #[test]
    fn ramp_moves_toward_target_without_overshoot() {
        // monotonic, never overshoots the target, converges
        let mut g = 0.0f32;
        let mut prev = -1.0f32;
        for _ in 0..2000 {
            g = ramp(g, 1.0);
            assert!(g >= prev, "gain must be monotonic up");
            assert!(g <= 1.0, "gain must not overshoot the target");
            prev = g;
        }
        assert!(g > 0.99, "gain should converge close to the target");
    }

    #[test]
    fn ramp_is_click_free_small_step() {
        // a single step must be a small fraction of the change (no instantaneous jump = no click)
        let g = ramp(0.0, 1.0);
        assert!(
            g > 0.0 && g < 0.1,
            "one ramp step should be gradual, got {g}"
        );
    }

    #[test]
    fn drift_trim_pulls_ring_toward_setpoint() {
        let setpoint = 4096usize;
        // too full → positive trim (consume faster to drain)
        let up = drift_trim(0.0, setpoint + 8000, setpoint);
        assert!(up > 0.0, "over-full ring should raise the trim, got {up}");
        // too empty → negative trim (consume slower to refill)
        let down = drift_trim(0.0, 0, setpoint);
        assert!(down < 0.0, "drained ring should lower the trim, got {down}");
        // at the setpoint → no change
        assert_eq!(drift_trim(0.0, setpoint, setpoint), 0.0);
    }

    #[test]
    fn drift_trim_is_bounded() {
        let mut t = 0.0;
        for _ in 0..1_000_000 {
            t = drift_trim(t, 60_000, 4096);
        }
        assert!(t <= 0.003 + 1e-9, "trim must stay bounded, got {t}");
    }
}
