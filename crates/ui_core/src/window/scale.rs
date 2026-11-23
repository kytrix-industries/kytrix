use super::{Window, surface::covering_surface};
use crate::math::Scale;

pub(crate) fn scale(window: &Window) -> Scale {
    window.scale
}

pub(crate) fn set_scale(window: &mut Window, scale: Scale) {
    window.scale = scale;
    window.scale_epoch = window.scale_epoch.wrapping_add(1);
    window.full_damage_pending = true;
    window
        .tree
        .set_device_scale((scale.factor() != 1.0).then_some(scale.factor()));
    let root = window.tree.root();
    window.tree.invalidate_layout(root);
    window.tree.mark_all_text_dirty();
    window.a11y.note_bounds_changed();
    if !window.surface_from_platform {
        window.surface = covering_surface(window.size(), window.scale);
    }
}

pub(crate) fn scale_epoch(window: &Window) -> u32 {
    window.scale_epoch
}
