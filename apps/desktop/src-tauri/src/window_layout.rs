//! Initial placement uses the usable monitor area, including its DPI scale.
//! Keep the window horizontally centered and slightly above vertical center.
use tauri::{LogicalSize, PhysicalPosition, PhysicalSize, WebviewWindow};

fn geometry(width: u32, height: u32, scale: f64) -> (u32, u32, i32, i32) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let logical_width = f64::from(width) / scale;
    let logical_height = f64::from(height) / scale;
    let w = (logical_width * 0.82)
        .min(1184.0)
        .min((logical_width - 24.0).max(1.0));
    let h = (logical_height * 0.84)
        .min(740.0)
        .min((logical_height - 24.0).max(1.0));
    // Fit one 16:10 rectangle instead of sizing the axes independently.
    // Integer physical units preserve the ratio through DPI rounding.
    let units = ((w * scale / 16.0).min(h * scale / 10.0)).floor() as u32;
    let (w, h) = if units > 0 {
        (units * 16, units * 10)
    } else {
        (width.max(1), height.max(1))
    };
    (
        w,
        h,
        ((width - w) / 2) as i32,
        (f64::from(height - h) * 0.38).round() as i32,
    )
}

pub(crate) fn place(window: &WebviewWindow) -> tauri::Result<()> {
    window.set_maximizable(false)?;
    if let Some(monitor) = window.current_monitor()?.or(window.primary_monitor()?) {
        let area = monitor.work_area();
        let scale = monitor.scale_factor();
        let (width, height, x, y) = geometry(area.size.width, area.size.height, scale);
        // Small work areas must not be forced beyond the screen by the normal minimum.
        window.set_min_size(Some(LogicalSize::new(
            700.0_f64.min(f64::from(width) / scale),
            437.5_f64.min(f64::from(height) / scale),
        )))?;
        window.set_size(PhysicalSize::new(width, height))?;
        window.set_position(PhysicalPosition::new(
            area.position.x.saturating_add(x),
            area.position.y.saturating_add(y),
        ))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_window_fits_work_area_at_common_resolutions_and_dpi() {
        for (width, height, scale) in [
            (1920, 1040, 1.0),
            (1366, 728, 1.0),
            (3840, 2080, 2.0),
            (1280, 680, 1.5),
            (640, 480, 1.0),
        ] {
            let (w, h, x, y) = geometry(width, height, scale);
            assert!(w > 0 && h > 0 && x >= 0 && y >= 0);
            assert_eq!(w * 10, h * 16, "initial size must be 16:10");
            assert!(w + x as u32 <= width && h + y as u32 <= height);
            assert!(y as u32 <= (height - h) / 2 + 1);
            assert!((x as u32 * 2).abs_diff(width - w) <= 1);
        }
    }
    #[test]
    fn scaled_equivalent_work_areas_keep_the_same_logical_size() {
        let a = geometry(1920, 1040, 1.0);
        let b = geometry(3840, 2080, 2.0);
        assert_eq!((a.0 * 2, a.1 * 2), (b.0, b.1));
    }
}
