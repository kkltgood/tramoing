//! Temperatures are degrees Celsius; absent sensors must never become zero.
#[derive(Clone, Debug, Default)]
pub struct Temperatures {
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
    /// Provider / failure details for local diagnostics, never a temperature estimate.
    pub source: String,
}

pub fn valid(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0 && value <= 150.0).then_some(value)
}

pub fn hottest(values: impl IntoIterator<Item = f64>) -> Option<f64> {
    values.into_iter().filter_map(valid).reduce(f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_and_invalid_sensors_do_not_mask_real_readings() {
        assert_eq!(hottest([0.0, -1.0, f64::NAN, 65535.0]), None);
        assert_eq!(hottest([43.5, 52.25, f64::INFINITY]), Some(52.25));
    }
}
