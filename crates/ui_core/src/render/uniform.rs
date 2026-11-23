use crate::math::Transform;

pub(crate) const ROOT_UNIFORM_DWORDS: usize = 28;

const INSTANCE_BASE_DWORD: usize = 25;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RootUniform {
    words: [f32; ROOT_UNIFORM_DWORDS],
}

impl RootUniform {
    pub(crate) fn frame(width: f32, height: f32, scale: f32) -> Self {
        let mut words = [0.0f32; ROOT_UNIFORM_DWORDS];
        words[0] = width;
        words[1] = height;
        words[2] = if width > 0.0 { 2.0 / width } else { 0.0 };
        words[3] = if height > 0.0 { 2.0 / height } else { 0.0 };
        words[4] = scale;
        words[8] = 1.0;
        words[11] = 1.0;
        words[16] = 1.0;
        words[17] = 1.0;
        words[18] = 1.0;
        words[19] = 1.0;
        Self { words }
    }

    pub(crate) fn geometry(
        frame: Self,
        transform: Transform,
        color: [f32; 4],
        clip_index: u32,
        clip_rect: [f32; 4],
    ) -> Self {
        let mut words = frame.words;
        let matrix = transform.to_array();
        words[8..12].copy_from_slice(&matrix[..4]);
        words[12..14].copy_from_slice(&matrix[4..6]);
        words[16..20].copy_from_slice(&color);
        words[20..24].copy_from_slice(&clip_rect);
        words[24] = clip_index as f32;
        words[INSTANCE_BASE_DWORD] = 0.0;
        Self { words }
    }

    pub(crate) fn with_instance_base(mut self, start: u32) -> Self {
        self.words[INSTANCE_BASE_DWORD] = start as f32;
        self
    }

    pub(crate) fn with_scroll(mut self, offset: [f32; 2]) -> Self {
        self.words[5] = offset[0];
        self.words[6] = offset[1];
        self
    }

    pub(crate) fn words(&self) -> &[f32; ROOT_UNIFORM_DWORDS] {
        &self.words
    }
}

#[cfg(test)]
mod tests {
    use super::{INSTANCE_BASE_DWORD, ROOT_UNIFORM_DWORDS, RootUniform};
    use crate::math::{Transform, px};

    #[test]
    fn layout_is_112_bytes() {
        assert_eq!(ROOT_UNIFORM_DWORDS * 4, 112);
        assert_eq!(std::mem::size_of::<RootUniform>(), 112);
    }

    #[test]
    fn frame_block_matches_the_contract() {
        let uniform = RootUniform::frame(800.0, 600.0, 2.0);
        let words = uniform.words();
        assert_eq!(words[0], 800.0);
        assert_eq!(words[1], 600.0);
        assert_eq!(words[2], 2.0 / 800.0);
        assert_eq!(words[3], 2.0 / 600.0);
        assert_eq!(words[4], 2.0, "frame.params.x 是 DPI scale");
    }

    #[test]
    fn zero_sized_viewport_does_not_divide_by_zero() {
        let uniform = RootUniform::frame(0.0, 0.0, 1.0);
        assert_eq!(uniform.words()[2], 0.0);
        assert_eq!(uniform.words()[3], 0.0);
        assert!(uniform.words().iter().all(|value| value.is_finite()));
    }

    #[test]
    fn the_scroll_offset_lands_in_frame_params_yz() {
        let uniform = RootUniform::frame(64.0, 64.0, 2.0).with_scroll([0.0, -30.0]);
        assert_eq!(uniform.words()[4], 2.0, "DPI 不受影响");
        assert_eq!(uniform.words()[5], 0.0);
        assert_eq!(uniform.words()[6], -30.0);
        assert_eq!(uniform.words()[7], 0.0, "保留恒 0");
        assert_eq!(
            &RootUniform::frame(64.0, 64.0, 2.0).words()[5..8],
            &[0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn instance_base_lands_in_draw_clip_y() {
        let uniform = RootUniform::frame(64.0, 64.0, 1.0).with_instance_base(37);
        assert_eq!(uniform.words()[INSTANCE_BASE_DWORD], 37.0);
        assert_eq!(uniform.words()[24], 0.0, "clip_index 不受影响");
    }

    #[test]
    fn geometry_overwrites_the_draw_block() {
        let frame = RootUniform::frame(800.0, 600.0, 1.5);
        let transform = Transform::scale(2.0, 5.0).then(Transform::translate(px(6.0), px(7.0)));
        let uniform = RootUniform::geometry(
            frame,
            transform,
            [0.1, 0.2, 0.3, 0.4],
            3,
            [4.0, 5.0, 100.0, 120.0],
        );
        let words = uniform.words();
        assert_eq!(&words[..8], &frame.words()[..8], "frame 块逐字保留");
        assert_eq!(&words[8..12], &[2.0, 0.0, 0.0, 5.0]);
        assert_eq!(&words[12..14], &[6.0, 7.0]);
        assert_eq!(&words[16..20], &[0.1, 0.2, 0.3, 0.4]);
        assert_eq!(&words[20..24], &[4.0, 5.0, 100.0, 120.0]);
        assert_eq!(words[24], 3.0);
        assert_eq!(words[INSTANCE_BASE_DWORD], 0.0, "几何绘制不吃实例基址");
    }
}
