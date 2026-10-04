/// Letterbox one application's logical content, reserving the status strip.
pub(crate) fn fit_rect(window: [f64; 2], content: [f64; 2]) -> [f64; 4] {
    if window
        .into_iter()
        .chain(content)
        .any(|v| !v.is_finite() || v <= 0.0)
    {
        return [0.0; 4];
    }
    let available = (window[1] - 64.0).max(0.0);
    let scale = (window[0] / content[0]).min(available / content[1]);
    let width = content[0] * scale;
    let height = content[1] * scale;
    [
        (window[0] - width) / 2.0,
        (available - height) / 2.0,
        width,
        height,
    ]
}

#[cfg(test)]
mod tests {
    use super::fit_rect;

    #[test]
    fn portrait_and_landscape_fit_leave_status_outside_input() {
        assert_eq!(
            fit_rect([400.0, 864.0], [800.0, 400.0]),
            [0.0, 300.0, 400.0, 200.0]
        );
        assert_eq!(
            fit_rect([800.0, 464.0], [400.0, 800.0]),
            [300.0, 0.0, 200.0, 400.0]
        );
        assert_eq!(fit_rect([0.0, 0.0], [800.0, 400.0]), [0.0; 4]);
        assert_eq!(fit_rect([400.0, 864.0], [f64::NAN, 400.0]), [0.0; 4]);
    }
}
