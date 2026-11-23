use crate::math::{Point, Size};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Area {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Area {
    pub(crate) const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) const fn x(self) -> f64 {
        self.x
    }

    pub(crate) const fn max_x(self) -> f64 {
        self.x + self.width
    }

    pub(crate) const fn max_y(self) -> f64 {
        self.y + self.height
    }

    fn intersection_area(self, other: Self) -> f64 {
        let width = (self.max_x().min(other.max_x()) - self.x.max(other.x)).max(0.0);
        let height = (self.max_y().min(other.max_y()) - self.y.max(other.y)).max(0.0);
        width * height
    }

    fn distance_sq_to(self, other: Self) -> f64 {
        let dx = (other.x - self.max_x())
            .max(self.x - other.max_x())
            .max(0.0);
        let dy = (other.y - self.max_y())
            .max(self.y - other.max_y())
            .max(0.0);
        dx * dx + dy * dy
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenArea {
    frame: Area,
    visible: Area,
}

impl ScreenArea {
    #[cfg(target_os = "macos")]
    pub(crate) const fn new(frame: Area, visible: Area) -> Self {
        Self { frame, visible }
    }

    #[cfg(target_os = "macos")]
    pub(crate) const fn visible(self) -> Area {
        self.visible
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Desktop {
    min_x: f64,
    max_y: f64,
}

impl Desktop {
    pub(crate) fn from_screens(screens: &[ScreenArea]) -> Option<Self> {
        let first = screens.first()?;
        Some(Self {
            min_x: screens
                .iter()
                .fold(first.frame.x, |min, screen| min.min(screen.frame.x)),
            max_y: screens.iter().fold(first.frame.max_y(), |max, screen| {
                max.max(screen.frame.max_y())
            }),
        })
    }

    fn to_appkit(self, point: Point) -> (f64, f64) {
        (
            self.min_x + f64::from(point.x),
            self.max_y - f64::from(point.y),
        )
    }

    fn to_engine(self, x: f64, top: f64) -> Point {
        Point::new((x - self.min_x) as f32, (self.max_y - top) as f32)
    }
}

pub(crate) fn content_origin_for_position(
    position: Point,
    size: Size,
    screens: &[ScreenArea],
) -> (f64, f64) {
    let width = f64::from(size.width.max(1.0));
    let height = f64::from(size.height.max(1.0));
    let Some(desktop) = Desktop::from_screens(screens) else {
        return (f64::from(position.x), -f64::from(position.y) - height);
    };
    let (left, top) = desktop.to_appkit(position);
    clamp_origin(Area::new(left, top - height, width, height), screens)
}

pub(crate) fn position_from_content_rect(content: Area, screens: &[ScreenArea]) -> Point {
    match Desktop::from_screens(screens) {
        Some(desktop) => desktop.to_engine(content.x, content.max_y()),
        None => Point::new(content.x as f32, (-content.max_y()) as f32),
    }
}

pub(crate) fn cascade_position(desktop: Desktop, main_visible: Area, index: usize) -> Point {
    let step = CASCADE_STEP * (index % CASCADE_WRAP) as f64;
    let x = main_visible.x - desktop.min_x + CASCADE_MARGIN + step;
    let y = desktop.max_y - main_visible.max_y() + CASCADE_MARGIN + step;
    Point::new(x as f32, y as f32)
}

const CASCADE_STEP: f64 = 26.0;
const CASCADE_WRAP: usize = 8;
const CASCADE_MARGIN: f64 = 80.0;

pub(crate) fn content_size_bounds(
    min: Option<Size>,
    max: Option<Size>,
    platform_min: Size,
) -> (Size, Option<Size>) {
    let max = max.map(|size| Size::new(size.width.max(1.0), size.height.max(1.0)));
    let min = min.map_or(platform_min, |size| {
        Size::new(size.width.max(1.0), size.height.max(1.0))
    });
    let min = max.map_or(min, |max| {
        Size::new(min.width.min(max.width), min.height.min(max.height))
    });
    (min, max)
}

fn clamp_origin(rect: Area, screens: &[ScreenArea]) -> (f64, f64) {
    let Some(visible) = target_work_area(rect, screens) else {
        return (rect.x, rect.y);
    };
    let x = if rect.width >= visible.width {
        visible.x
    } else {
        rect.x.clamp(visible.x, visible.max_x() - rect.width)
    };
    let y = if rect.height >= visible.height {
        visible.max_y() - rect.height
    } else {
        rect.y.clamp(visible.y, visible.max_y() - rect.height)
    };
    (x, y)
}

fn target_work_area(rect: Area, screens: &[ScreenArea]) -> Option<Area> {
    let mut best: Option<(f64, Area)> = None;
    for screen in screens {
        let area = rect.intersection_area(screen.visible);
        if area > 0.0 && best.is_none_or(|(best_area, _)| area > best_area) {
            best = Some((area, screen.visible));
        }
    }
    if let Some((_, visible)) = best {
        return Some(visible);
    }
    screens
        .iter()
        .map(|screen| screen.visible)
        .min_by(|a, b| rect.distance_sq_to(*a).total_cmp(&rect.distance_sq_to(*b)))
}

#[cfg(test)]
mod tests {
    use super::{
        Area, Desktop, ScreenArea, cascade_position, content_origin_for_position,
        content_size_bounds, position_from_content_rect,
    };
    use crate::math::{Point, Size};

    fn main_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(0.0, 0.0, 1920.0, 1080.0),
            visible: Area::new(0.0, 60.0, 1920.0, 995.0),
        }
    }

    fn left_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(-1280.0, 0.0, 1280.0, 1024.0),
            visible: Area::new(-1280.0, 0.0, 1280.0, 1024.0),
        }
    }

    fn upper_screen() -> ScreenArea {
        ScreenArea {
            frame: Area::new(0.0, 1080.0, 1920.0, 1200.0),
            visible: Area::new(0.0, 1080.0, 1920.0, 1200.0),
        }
    }

    fn desktop_of(screens: &[ScreenArea]) -> Desktop {
        Desktop::from_screens(screens).expect("至少一块屏幕")
    }

    #[test]
    fn engine_position_is_flipped_into_appkit_content_origin() {
        let screens = [main_screen()];
        assert_eq!(
            content_origin_for_position(Point::new(100.0, 50.0), Size::new(800.0, 600.0), &screens),
            (100.0, 430.0)
        );
        assert_eq!(
            content_origin_for_position(Point::new(0.0, 0.0), Size::new(800.0, 600.0), &screens),
            (0.0, 455.0)
        );
    }

    #[test]
    fn roundtrip_between_engine_position_and_appkit_content_rect() {
        let screens = [main_screen()];
        for (x, y, w, h) in [
            (100.0_f32, 50.0_f32, 800.0_f32, 600.0_f32),
            (200.0, 200.0, 320.0, 200.0),
            (1500.0, 300.0, 400.0, 640.0),
        ] {
            let (left, bottom) =
                content_origin_for_position(Point::new(x, y), Size::new(w, h), &screens);
            let content = Area::new(left, bottom, f64::from(w), f64::from(h));
            assert_eq!(
                position_from_content_rect(content, &screens),
                Point::new(x, y)
            );
        }
    }

    #[test]
    fn positions_are_clamped_into_the_visible_frame() {
        let screens = [main_screen()];
        assert_eq!(
            content_origin_for_position(
                Point::new(5000.0, 50.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (1120.0, 430.0)
        );
        assert_eq!(
            content_origin_for_position(
                Point::new(-50.0, -20.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (0.0, 455.0)
        );
        assert_eq!(
            content_origin_for_position(
                Point::new(100.0, 900.0),
                Size::new(800.0, 600.0),
                &screens
            ),
            (100.0, 60.0)
        );
    }

    #[test]
    fn a_window_larger_than_the_work_area_keeps_its_top_left_visible() {
        let screens = [main_screen()];
        assert_eq!(
            content_origin_for_position(
                Point::new(100.0, 100.0),
                Size::new(2500.0, 1500.0),
                &screens
            ),
            (0.0, -445.0)
        );
    }

    #[test]
    fn multi_screen_negative_origin_and_upper_screen_flip() {
        let screens = [main_screen(), left_screen()];
        let desktop = desktop_of(&screens);
        assert_eq!(desktop.min_x, -1280.0);
        assert_eq!(desktop.max_y, 1080.0);
        assert_eq!(
            content_origin_for_position(Point::new(0.0, 100.0), Size::new(800.0, 600.0), &screens),
            (-1280.0, 380.0)
        );

        let screens = [main_screen(), upper_screen()];
        let desktop = desktop_of(&screens);
        assert_eq!(desktop.max_y, 2280.0);
        assert_eq!(
            content_origin_for_position(Point::new(100.0, 50.0), Size::new(800.0, 600.0), &screens),
            (100.0, 1630.0)
        );
    }

    #[test]
    fn clamp_picks_the_screen_with_the_largest_intersection() {
        let screens = [main_screen(), left_screen()];
        let (left, bottom) = content_origin_for_position(
            Point::new(680.0, 100.0),
            Size::new(800.0, 600.0),
            &screens,
        );
        assert_eq!((left, bottom), (-800.0, 380.0));
    }

    #[test]
    fn far_off_screen_position_snaps_to_the_nearest_screen() {
        let screens = [main_screen(), left_screen()];
        let (left, bottom) = content_origin_for_position(
            Point::new(-9000.0, 300.0),
            Size::new(800.0, 600.0),
            &screens,
        );
        assert_eq!((left, bottom), (-1280.0, 180.0));
    }

    #[test]
    fn menu_bar_and_dock_shrink_the_work_area() {
        let screens = [main_screen()];
        let visible = screens[0].visible;
        for engine_y in [0.0_f32, 500.0, 1000.0] {
            let (_, bottom) = content_origin_for_position(
                Point::new(0.0, engine_y),
                Size::new(400.0, 300.0),
                &screens,
            );
            assert!(
                bottom >= visible.y - f64::EPSILON
                    && bottom + 300.0 <= visible.max_y() + f64::EPSILON,
                "engine_y={engine_y} 的窗口必须完整落在 visibleFrame 内：bottom={bottom}"
            );
        }
    }

    #[test]
    fn cascade_defaults_step_down_right_and_wrap() {
        let screens = [main_screen()];
        let desktop = desktop_of(&screens);
        let visible = screens[0].visible;
        let first = cascade_position(desktop, visible, 0);
        assert_eq!(first, Point::new(80.0, 105.0));
        let second = cascade_position(desktop, visible, 1);
        assert_eq!(second, Point::new(106.0, 131.0));
        assert_eq!(cascade_position(desktop, visible, 8), first);
    }

    #[test]
    fn size_bounds_use_platform_minimum_and_clamp_min_into_max() {
        let platform_min = Size::new(320.0, 200.0);
        assert_eq!(
            content_size_bounds(None, None, platform_min),
            (platform_min, None)
        );
        assert_eq!(
            content_size_bounds(None, Some(Size::new(200.0, 100.0)), platform_min),
            (Size::new(200.0, 100.0), Some(Size::new(200.0, 100.0)))
        );
        assert_eq!(
            content_size_bounds(
                Some(Size::new(900.0, 700.0)),
                Some(Size::new(800.0, 600.0)),
                platform_min
            ),
            (Size::new(800.0, 600.0), Some(Size::new(800.0, 600.0)))
        );
        assert_eq!(
            content_size_bounds(Some(Size::new(640.0, 480.0)), None, platform_min),
            (Size::new(640.0, 480.0), None)
        );
    }

    #[test]
    fn without_screens_the_mapping_only_flips_the_y_axis() {
        assert_eq!(
            content_origin_for_position(Point::new(10.0, 20.0), Size::new(100.0, 50.0), &[]),
            (10.0, -70.0)
        );
        assert_eq!(
            position_from_content_rect(Area::new(10.0, -70.0, 100.0, 50.0), &[]),
            Point::new(10.0, 20.0)
        );
    }
}
