use std::time::Duration;

use gpui_kit::base::PresenceSample;

pub const ANIMATION_TICK: Duration = Duration::from_millis(150);

/// Whether a popup that `Presence` is animating belongs in this frame.
///
/// A popup has something to draw only while it is open, or while it is still fading
/// out after having been open. [`PresenceSample::should_render`] alone cannot answer
/// that: the first sample of an element that has never been present comes back as
/// `Exiting` with no progress, because the transition reports `Running` for zero
/// elapsed time. Drawing that frame would leave an invisible popup in the tree, and a
/// popup occludes the pointer, so it would swallow clicks meant for whatever is behind
/// it — for as long as the animation clock does not advance, which is always in a test.
pub fn popup_should_render(opened: bool, sample: PresenceSample) -> bool {
    opened || sample.progress > 0.0
}
