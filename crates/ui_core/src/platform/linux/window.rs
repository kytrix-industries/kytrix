use crate::math::{LogicalSize, px};

pub(crate) const MIN_SIZE_LOGICAL: LogicalSize = LogicalSize::new(px(320.0), px(200.0));

pub(crate) const SHADOW_MARGIN: f32 = 0.0;

const _: () = assert!(
    SHADOW_MARGIN.to_bits() == 0,
    "SHADOW_MARGIN 改成非 0 时必须同时实现 wl_surface.set_input_region（见本模块文档「wl_region 的已知取舍」）"
);

pub(crate) const fn unsupported_window_control(
    _control: crate::window::WindowControl,
) -> Option<&'static str> {
    None
}
