use super::{Window, surface::covering_surface};
use crate::math::DevicePixelRatio;

pub(crate) fn scale(window: &Window) -> DevicePixelRatio {
    window.scale
}

pub(crate) fn set_scale(window: &mut Window, scale: DevicePixelRatio) {
    window.scale = scale;
    window.scale_epoch = window.scale_epoch.wrapping_add(1);
    window.full_damage_pending = true;
    window.tree.mark_all_text_dirty();
    window.a11y.note_bounds_changed();
    if !window.surface_from_platform {
        window.surface = covering_surface(window.size(), window.scale);
    }
}

pub(crate) fn scale_epoch(window: &Window) -> u32 {
    window.scale_epoch
}
