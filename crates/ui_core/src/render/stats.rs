#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderStats {
    pub draw_calls: u32,
    pub instances: u32,
    pub geometry_uploaded_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::RenderStats;

    #[test]
    fn default_stats_are_zero() {
        let stats = RenderStats::default();
        assert_eq!(stats.draw_calls, 0);
        assert_eq!(stats.instances, 0);
        assert_eq!(stats.geometry_uploaded_bytes, 0);
    }
}
