//! Peak memory the way the table and the progress lines both write it.

/// Peak memory in binary units, to about three significant figures: 940KiB,
/// 10.4MiB, 1.02GiB.
pub(crate) fn bytes(kib: i64) -> String {
    const STEP: f64 = 1024.0;
    let (value, unit) = match kib as f64 {
        v if v < STEP => (v, "KiB"),
        v if v < STEP * STEP => (v / STEP, "MiB"),
        v => (v / (STEP * STEP), "GiB"),
    };

    if value >= 100.0 {
        format!("{value:.0}{unit}")
    } else if value >= 10.0 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.2}{unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_picks_a_unit_at_each_boundary() {
        assert_eq!(bytes(0), "0.00KiB");
        assert_eq!(bytes(1023), "1023KiB");
        assert_eq!(bytes(1024), "1.00MiB");
        assert_eq!(bytes(1024 * 1024 - 1), "1024MiB");
        assert_eq!(bytes(1024 * 1024), "1.00GiB");
    }

    #[test]
    fn bytes_keeps_three_figures_across_the_precision_switches() {
        // under 10 gets two decimals, 10 to 100 gets one, 100 and over gets none
        assert_eq!(bytes(9 * 1024), "9.00MiB");
        assert_eq!(bytes(10 * 1024), "10.0MiB");
        assert_eq!(bytes(99 * 1024), "99.0MiB");
        assert_eq!(bytes(100 * 1024), "100MiB");
    }

    #[test]
    fn bytes_rounds_up_into_an_extra_figure_just_below_a_switch() {
        // 99.98MiB is under the cutoff, so it takes the
        // one-decimal branch and rounds to a four-figure
        // "100.0MiB", a character wider than "100MiB"
        assert_eq!(bytes(99 * 1024 + 1013), "100.0MiB");
    }
}
