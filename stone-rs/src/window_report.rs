//! Window and display measurement report.
//!
//! Pure data assembly: the SDL queries live in `lib.rs`, everything here is
//! side-effect-free so the FIT derivation and JSON assembly are unit-testable.
//! Units are labelled explicitly -- SDL display bounds and window size/position
//! are POINTS on macOS (logical coords), `drawable_size` is PIXELS.

#[derive(Clone, Debug)]
pub struct DisplayReport {
    pub index: i32,
    pub name: String,
    /// x, y, w, h in points.
    pub bounds: (i32, i32, i32, i32),
    /// x, y, w, h in points (menu bar / taskbar subtracted, NOT the title bar).
    pub usable: (i32, i32, i32, i32),
    /// diagonal, horizontal, vertical dpi.
    pub dpi: (f32, f32, f32),
    /// w, h, refresh_rate of the current mode.
    pub current_mode: Option<(i32, i32, i32)>,
    pub mode_count: i32,
    /// full enumerated mode list (w, h, refresh_rate).
    pub modes: Vec<(i32, i32, i32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Borders {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

#[derive(Clone, Debug)]
pub struct WindowReport {
    pub displays: Vec<DisplayReport>,
    /// requested content size in points (as passed to the window builder).
    pub requested_size: (i32, i32),
    /// requested position in points; may be the SDL_WINDOWPOS_CENTERED sentinel.
    pub requested_pos: (i32, i32),
    pub actual_size: (i32, i32),
    pub actual_pos: (i32, i32),
    pub landed_display: i32,
    /// drawable size in PIXELS.
    pub drawable_size: (i32, i32),
    pub dpi_scale: f32,
    pub borders: Borders,
}

/// A window the engine is about to place. Pure input to
/// `resolve_window_placement` -- no SDL types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowRequest {
    /// requested content size in points.
    pub size: (i32, i32),
    /// requested content top-left in points.
    pub pos: (i32, i32),
    /// explicit target display index, or None to select by position.
    pub display: Option<i32>,
}

/// The fitted result: never larger than the target display's usable area
/// minus borders, and positioned so the whole frame stays on-screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPlacement {
    pub size: (i32, i32),
    pub pos: (i32, i32),
    pub display: i32,
}

/// Pure fit: pick a display, clamp size to `usable - borders`, clamp position
/// so the whole frame (content + borders) lies inside `usable_bounds`. No SDL,
/// no I/O -- unit-tested below. Empty `displays` returns the request untouched
/// (nothing to clamp against).
pub fn resolve_window_placement(
    req: WindowRequest,
    displays: &[DisplayReport],
    borders: Borders,
) -> WindowPlacement {
    let Some(target) = select_display(&req, displays) else {
        return WindowPlacement {
            size: req.size,
            pos: req.pos,
            display: req.display.unwrap_or(0),
        };
    };
    let (ux, uy, uw, uh) = target.usable;

    let max_w = (uw - borders.left - borders.right).max(1);
    let max_h = (uh - borders.top - borders.bottom).max(1);
    let w = req.size.0.min(max_w).max(1);
    let h = req.size.1.min(max_h).max(1);

    let min_x = ux + borders.left;
    let max_x = ux + uw - w - borders.right;
    let min_y = uy + borders.top;
    let max_y = uy + uh - h - borders.bottom;
    let x = req.pos.0.clamp(min_x, max_x.max(min_x));
    let y = req.pos.1.clamp(min_y, max_y.max(min_y));

    WindowPlacement {
        size: (w, h),
        pos: (x, y),
        display: target.index,
    }
}

fn select_display<'a>(
    req: &WindowRequest,
    displays: &'a [DisplayReport],
) -> Option<&'a DisplayReport> {
    if let Some(idx) = req.display {
        if let Some(d) = displays.iter().find(|d| d.index == idx) {
            return Some(d);
        }
    }
    if let Some(d) = displays.iter().find(|d| point_in_bounds(req.pos, d.bounds)) {
        return Some(d);
    }
    displays.first()
}

fn point_in_bounds(p: (i32, i32), bounds: (i32, i32, i32, i32)) -> bool {
    let (bx, by, bw, bh) = bounds;
    p.0 >= bx && p.0 < bx + bw && p.1 >= by && p.1 < by + bh
}

pub fn closest_display_mode(
    modes: &[(i32, i32, i32)],
    target_w: i32,
    target_h: i32,
) -> Option<(i32, i32, i32)> {
    modes.iter().copied().min_by_key(|&(w, h, refresh)| {
        let area_diff = ((w as i64 * h as i64) - (target_w as i64 * target_h as i64)).abs();
        (area_diff, -(refresh as i64))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub max_content_w: i32,
    pub max_content_h: i32,
    /// requested - max_content; positive means the request exceeds the fit.
    pub over_w: i32,
    pub over_h: i32,
    pub exceeds: bool,
}

impl WindowReport {
    pub fn landed(&self) -> Option<&DisplayReport> {
        self.displays
            .iter()
            .find(|d| d.index == self.landed_display)
    }

    /// max_content = usable - borders on the landed display; the FIT
    /// derivation is logged but never acted on.
    pub fn fit(&self) -> Option<Fit> {
        let d = self.landed()?;
        let (_, _, uw, uh) = d.usable;
        let max_content_w = uw - self.borders.left - self.borders.right;
        let max_content_h = uh - self.borders.top - self.borders.bottom;
        let over_w = self.requested_size.0 - max_content_w;
        let over_h = self.requested_size.1 - max_content_h;
        Some(Fit {
            max_content_w,
            max_content_h,
            over_w,
            over_h,
            exceeds: over_w > 0 || over_h > 0,
        })
    }

    /// One human-readable INFO block (multi-line, logged as a single call).
    pub fn render_human(&self) -> String {
        let mut s = String::new();
        s.push_str("window/display report (phase 1; units: pt=points, px=pixels):\n");
        s.push_str(&format!("  displays={}\n", self.displays.len()));
        for d in &self.displays {
            let (bx, by, bw, bh) = d.bounds;
            let (ux, uy, uw, uh) = d.usable;
            let (ddpi, hdpi, vdpi) = d.dpi;
            let mode = match d.current_mode {
                Some((w, h, r)) => format!("{}x{}@{}", w, h, r),
                None => "?".to_string(),
            };
            s.push_str(&format!(
                "  display[{}] name='{}' bounds=({},{} {}x{})pt usable=({},{} {}x{})pt dpi=(d{:.1} h{:.1} v{:.1}) mode={} mode_count={}{}\n",
                d.index, d.name, bx, by, bw, bh, ux, uy, uw, uh,
                ddpi, hdpi, vdpi, mode, d.mode_count, render_modes_summary(&d.modes),
            ));
        }
        s.push_str(&format!(
            "  requested: size={}x{}pt pos=({},{})pt\n",
            self.requested_size.0,
            self.requested_size.1,
            self.requested_pos.0,
            self.requested_pos.1,
        ));
        s.push_str(&format!(
            "  actual:    size={}x{}pt pos=({},{})pt display[{}]\n",
            self.actual_size.0,
            self.actual_size.1,
            self.actual_pos.0,
            self.actual_pos.1,
            self.landed_display,
        ));
        s.push_str(&format!(
            "  drawable={}x{}px dpi_scale={:.2}\n",
            self.drawable_size.0, self.drawable_size.1, self.dpi_scale,
        ));
        s.push_str(&format!(
            "  borders(pt): top={} left={} bottom={} right={}\n",
            self.borders.top, self.borders.left, self.borders.bottom, self.borders.right,
        ));
        match self.fit() {
            Some(f) => {
                s.push_str(&format!(
                    "  max_content={}x{}pt (usable - borders on display[{}])\n",
                    f.max_content_w, f.max_content_h, self.landed_display,
                ));
                if f.exceeds {
                    s.push_str(&format!(
                        "  FIT: request EXCEEDS max_content by ({}, {})pt -- NOT acted on (phase 1)",
                        f.over_w.max(0), f.over_h.max(0),
                    ));
                } else {
                    s.push_str(&format!(
                        "  FIT: request fits (headroom ({}, {})pt)",
                        (-f.over_w).max(0),
                        (-f.over_h).max(0),
                    ));
                }
            }
            None => {
                s.push_str(&format!(
                    "  FIT: landed display[{}] not found in enumeration -- cannot derive",
                    self.landed_display,
                ));
            }
        }
        s
    }
}

/// Compact deduped resolution summary. Empty when the mode list is empty.
fn render_modes_summary(modes: &[(i32, i32, i32)]) -> String {
    if modes.is_empty() {
        return String::new();
    }
    let mut res: Vec<(i32, i32)> = modes.iter().map(|&(w, h, _)| (w, h)).collect();
    res.sort_unstable();
    res.dedup();
    const MAX: usize = 8;
    let shown: Vec<String> = res
        .iter()
        .take(MAX)
        .map(|&(w, h)| format!("{}x{}", w, h))
        .collect();
    let more = res.len().saturating_sub(MAX);
    if more > 0 {
        format!(" res=[{}, +{} more]", shown.join(", "), more)
    } else {
        format!(" res=[{}]", shown.join(", "))
    }
}

/// A mode's refresh rate when it is known. SDL reports 0 when the driver does
/// not know the rate; a negative value is not a rate either.
pub fn known_refresh_rate(refresh_rate: i32) -> Option<i32> {
    (refresh_rate > 0).then_some(refresh_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn title_bar(top: i32) -> Borders {
        Borders {
            top,
            left: 0,
            bottom: 0,
            right: 0,
        }
    }

    #[test]
    fn known_refresh_rate_treats_zero_as_unknown() {
        assert_eq!(known_refresh_rate(144), Some(144));
        assert_eq!(known_refresh_rate(60), Some(60));
        assert_eq!(known_refresh_rate(0), None);
        assert_eq!(known_refresh_rate(-1), None);
    }

    fn display0() -> DisplayReport {
        DisplayReport {
            index: 0,
            name: "Built-in".to_string(),
            bounds: (0, 0, 1440, 900),
            usable: (0, 30, 1440, 870),
            dpi: (226.7, 226.7, 226.7),
            current_mode: Some((1440, 900, 60)),
            mode_count: 1,
            modes: vec![(1440, 900, 60)],
        }
    }

    fn report(req: (i32, i32), borders: Borders) -> WindowReport {
        WindowReport {
            displays: vec![display0()],
            requested_size: req,
            requested_pos: (100, 100),
            actual_size: req,
            actual_pos: (100, 100),
            landed_display: 0,
            drawable_size: (req.0 * 2, req.1 * 2),
            dpi_scale: 2.0,
            borders,
        }
    }

    #[test]
    fn fit_exceeds_reports_overflow() {
        let r = report((2560, 1600), title_bar(28));
        let f = r.fit().unwrap();
        assert_eq!(f.max_content_w, 1440);
        assert_eq!(f.max_content_h, 842);
        assert_eq!(f.over_w, 1120);
        assert_eq!(f.over_h, 758);
        assert!(f.exceeds);
        assert!(r
            .render_human()
            .contains("FIT: request EXCEEDS max_content by (1120, 758)pt"));
    }

    #[test]
    fn fit_within_reports_headroom() {
        let r = report((800, 700), title_bar(28));
        let f = r.fit().unwrap();
        assert!(!f.exceeds);
        assert_eq!(f.over_w, 800 - 1440);
        assert_eq!(f.over_h, 700 - 842);
        assert!(r
            .render_human()
            .contains("FIT: request fits (headroom (640, 142)pt)"));
    }

    #[test]
    fn fit_none_when_landed_display_missing() {
        let mut r = report((800, 700), title_bar(0));
        r.landed_display = 5;
        assert!(r.fit().is_none());
        assert!(r.render_human().contains("cannot derive"));
    }

    fn display_at(
        index: i32,
        bounds: (i32, i32, i32, i32),
        usable: (i32, i32, i32, i32),
    ) -> DisplayReport {
        DisplayReport {
            index,
            name: format!("d{}", index),
            bounds,
            usable,
            dpi: (100.0, 100.0, 100.0),
            current_mode: None,
            mode_count: 0,
            modes: vec![],
        }
    }

    #[test]
    fn placement_fits_the_request_to_its_display() {
        let displays = [
            display_at(0, (0, 0, 1440, 900), (0, 30, 1440, 870)),
            display_at(1, (1440, 0, 2560, 1440), (1440, 0, 2560, 1440)),
        ];
        let place = |n: usize, borders, size, pos, display| {
            let p = resolve_window_placement(
                WindowRequest { size, pos, display },
                &displays[..n],
                borders,
            );
            (p.size, p.pos, p.display)
        };
        let none = title_bar(0);
        let framed = Borders {
            top: 28,
            left: 4,
            bottom: 2,
            right: 4,
        };
        // 870 usable - 28 title
        assert_eq!(
            place(1, title_bar(28), (2560, 1600), (0, 30), None).0,
            (1440, 842)
        );
        assert_eq!(
            place(1, none, (800, 700), (100, 100), None),
            ((800, 700), (100, 100), 0)
        );
        assert_eq!(
            place(1, none, (800, 700), (2560, 0), None),
            ((800, 700), (640, 30), 0)
        );
        assert_eq!(place(1, framed, (800, 700), (2560, 0), None).1, (636, 58));
        assert_eq!(
            place(2, none, (800, 700), (100, 100), Some(1)),
            ((800, 700), (1440, 100), 1)
        );
        assert_eq!(
            place(2, none, (800, 700), (2000, 200), None),
            ((800, 700), (2000, 200), 1)
        );
        assert_eq!(place(1, none, (800, 700), (100, 100), Some(7)).2, 0);
        assert_eq!(
            place(0, none, (800, 700), (2560, 0), Some(3)),
            ((800, 700), (2560, 0), 3)
        );
    }

    #[test]
    fn closest_mode_prefers_exact_then_nearest_area_then_refresh() {
        type Mode = (i32, i32, i32);
        let cases: [(&[Mode], Option<Mode>, &str); 4] = [
            (
                &[(1920, 1080, 60), (2560, 1440, 60), (1280, 720, 60)],
                Some((1920, 1080, 60)),
                "exact",
            ),
            (
                &[(1280, 720, 60), (2560, 1440, 60)],
                Some((1280, 720, 60)),
                "nearest area",
            ),
            (
                &[(1920, 1080, 60), (1920, 1080, 120)],
                Some((1920, 1080, 120)),
                "refresh tie",
            ),
            (&[], None, "empty"),
        ];
        for (modes, expected, name) in cases {
            assert_eq!(closest_display_mode(modes, 1920, 1080), expected, "{name}");
        }
    }

    #[test]
    fn units_are_labelled() {
        let r = report((800, 700), title_bar(28));
        let h = r.render_human();
        assert!(h.contains("pt=points, px=pixels"));
        assert!(h.contains("drawable=1600x1400px"));
        assert!(h.contains("size=800x700pt"));
    }
}
