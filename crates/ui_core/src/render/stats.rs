//! 一次 GPU 提交的实际绘制统计（P17 Debug HUD）。
//!
//! 数值由三平台后端在录制命令时逐次累加，**不是**从 `Scene`/`InstanceBuffer` 反推：
//! 局部 damage 会按 scissor 重复执行全部 draw op，HUD 必须看到真实发出的 API 调用数。

/// 最近一次提交的绘制统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderStats {
    /// 本帧实际发出的 draw call 数（含按 damage scissor 重复的 pass）。
    pub draw_calls: u32,
    /// 本帧实例缓冲的条目数（不按 damage 重复）。
    pub instances: u32,
}

#[cfg(test)]
mod tests {
    use super::RenderStats;

    #[test]
    fn default_stats_are_zero() {
        let stats = RenderStats::default();
        assert_eq!(stats.draw_calls, 0);
        assert_eq!(stats.instances, 0);
    }
}
