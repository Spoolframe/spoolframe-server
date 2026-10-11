//! Settings › Remote access's uplink card (PX-46): the budget, the most
//! streams off the LAN, the admins' share, and the upload measured. The
//! server checks what is saved; these only word it.

use spoolframe_admin_api::UplinkMeasurement;

/// What the card says the budget does, for its value as typed: 0 or
/// nothing is no limit.
pub fn budget_meaning(text: &str) -> String {
    let text = text.trim();
    let mbps = if text.is_empty() {
        0
    } else {
        match text.parse::<u64>() {
            Ok(mbps) => mbps,
            Err(_) => return String::new(),
        }
    };
    if mbps == 0 {
        "No limit: a playback outside the home plays as the app asks.".into()
    } else {
        format!(
            "Playbacks outside the home share {mbps} Mbit/s. In Auto quality, each takes at \
             most its part."
        )
    }
}

/// A measurement as the card shows it: the upload, then each upload's
/// rate.
pub fn measured_line(m: &UplinkMeasurement) -> String {
    let samples: Vec<String> = m.samples_mbps.iter().map(|s| format!("{s:.1}")).collect();
    format!(
        "Upload {:.1} Mbit/s (uploads: {} Mbit/s)",
        m.upload_mbps,
        samples.join(", ")
    )
}

/// The button that takes the suggested budget.
pub fn use_suggested(m: &UplinkMeasurement) -> String {
    format!("Use {} Mbit/s (80%)", m.suggested_budget_mbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_nothing_are_no_limit() {
        assert!(budget_meaning("").starts_with("No limit"));
        assert!(budget_meaning("0").starts_with("No limit"));
        assert!(budget_meaning("20").contains("20 Mbit/s"));
        assert_eq!(budget_meaning("twenty"), "");
    }

    #[test]
    fn a_measurement_is_its_median_and_samples() {
        let m = UplinkMeasurement {
            upload_mbps: 21.3,
            samples_mbps: vec![21.3, 19.04, 22.0],
            suggested_budget_mbps: 17,
        };
        assert_eq!(
            measured_line(&m),
            "Upload 21.3 Mbit/s (uploads: 21.3, 19.0, 22.0 Mbit/s)"
        );
        assert_eq!(use_suggested(&m), "Use 17 Mbit/s (80%)");
    }
}
