use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize};

const MIN_VISIBLE_WIDTH: i64 = 120;
const MIN_VISIBLE_HEIGHT: i64 = 40;
const EDGE_MARGIN: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

fn monitor_rect(monitor: &tauri::Monitor) -> Rect {
    Rect {
        x: monitor.position().x,
        y: monitor.position().y,
        width: monitor.size().width,
        height: monitor.size().height,
    }
}

fn intersection(left: Rect, right: Rect) -> (i64, i64) {
    let x = (left.x as i64 + left.width as i64).min(right.x as i64 + right.width as i64)
        - (left.x as i64).max(right.x as i64);
    let y = (left.y as i64 + left.height as i64).min(right.y as i64 + right.height as i64)
        - (left.y as i64).max(right.y as i64);
    (x.max(0), y.max(0))
}

fn is_visible(rect: Rect, monitors: &[Rect]) -> bool {
    monitors.iter().any(|monitor| {
        let (width, height) = intersection(rect, *monitor);
        width >= MIN_VISIBLE_WIDTH && height >= MIN_VISIBLE_HEIGHT
    })
}

fn fit_to_monitor(rect: Rect, monitor: Rect) -> Rect {
    let width = rect
        .width
        .min(monitor.width.saturating_sub(EDGE_MARGIN).max(1));
    let height = rect
        .height
        .min(monitor.height.saturating_sub(EDGE_MARGIN).max(1));
    Rect {
        x: monitor.x + (monitor.width.saturating_sub(width) / 2) as i32,
        y: monitor.y + (monitor.height.saturating_sub(height) / 2) as i32,
        width,
        height,
    }
}

/// 窗口完全落在所有显示器之外时，移回主显示器并收缩到可见范围。
pub fn ensure_main_window_visible(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Ok(position) = window.outer_position() else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let Ok(monitors) = window.available_monitors() else {
        return;
    };
    if monitors.is_empty() {
        return;
    }
    let rects: Vec<Rect> = monitors.iter().map(monitor_rect).collect();
    let current = Rect {
        x: position.x,
        y: position.y,
        width: size.width,
        height: size.height,
    };
    if is_visible(current, &rects) {
        return;
    }
    let target = window
        .primary_monitor()
        .ok()
        .flatten()
        .as_ref()
        .map(monitor_rect)
        .or_else(|| rects.first().copied());
    let Some(target) = target else {
        return;
    };
    let maximized = window.is_maximized().unwrap_or(false);
    if maximized {
        let _ = window.unmaximize();
    }
    let fitted = fit_to_monitor(current, target);
    if fitted.width != current.width || fitted.height != current.height {
        let _ = window.set_size(PhysicalSize::new(fitted.width, fitted.height));
    }
    let _ = window.set_position(PhysicalPosition::new(fitted.x, fitted.y));
    if maximized {
        let _ = window.maximize();
    }
    log::info!("窗口不在任何显示器内，已移回主显示器");
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONITOR: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    #[test]
    fn fully_inside_and_partially_visible_are_kept() {
        assert!(is_visible(
            Rect {
                x: 100,
                y: 80,
                width: 1200,
                height: 800,
            },
            &[MONITOR],
        ));
        assert!(is_visible(
            Rect {
                x: -1000,
                y: 0,
                width: 1200,
                height: 800,
            },
            &[MONITOR],
        ));
    }

    #[test]
    fn mostly_offscreen_or_tiny_intersection_is_not_visible() {
        assert!(!is_visible(
            Rect {
                x: 1900,
                y: 0,
                width: 800,
                height: 600,
            },
            &[MONITOR],
        ));
        assert!(!is_visible(
            Rect {
                x: -700,
                y: -500,
                width: 800,
                height: 600,
            },
            &[MONITOR],
        ));
    }

    #[test]
    fn oversized_window_is_clamped_and_centered() {
        let fitted = fit_to_monitor(
            Rect {
                x: -500,
                y: -500,
                width: 3000,
                height: 2000,
            },
            MONITOR,
        );
        assert_eq!(fitted.width, 1880);
        assert_eq!(fitted.height, 1040);
        assert_eq!(fitted.x, 20);
        assert_eq!(fitted.y, 20);
    }
}
