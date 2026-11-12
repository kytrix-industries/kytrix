//! GPU 计时：`vkCmdWriteTimestamp` + query pool（`design/phase-12-macos-linux.md` §6.8）。

use ash::vk;

use crate::{
    error::EngineError,
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::device::{Device, init_error},
        timer::{GpuTimerSample, Section},
    },
};

/// 每帧两个时间戳（begin / end）。
const QUERIES_PER_FRAME: u32 = 2;

/// 时间戳查询池。
pub(crate) struct GpuTimer {
    device: ash::Device,
    pool: vk::QueryPool,
    /// ns / tick（`timestampPeriod`）。
    period_ns: f64,
}

impl GpuTimer {
    /// 队列不支持时间戳时返回 `Ok(None)`（**不是错误**：计时是可选能力）。
    pub(crate) fn new(device: &Device) -> Result<Option<Self>, EngineError> {
        let (period, valid_bits) = device.timestamps();
        if valid_bits == 0 {
            return Ok(None);
        }
        // SAFETY: 句柄归本结构独占；`Drop` 里销毁。
        let pool = unsafe {
            device.raw().create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(QUERIES_PER_FRAME * MAX_FRAMES_IN_FLIGHT as u32),
                None,
            )
        }
        .map_err(|error| init_error("create_query_pool", format!("{error:?}")))?;
        Ok(Some(Self {
            device: device.raw().clone(),
            pool,
            period_ns: f64::from(period),
        }))
    }

    pub(crate) fn begin(&self, command: vk::CommandBuffer, slot: usize) {
        // SAFETY: query 索引在池容量内（2 × 槽位）。
        unsafe {
            self.device
                .cmd_reset_query_pool(command, self.pool, index(slot), 1);
            self.device.cmd_write_timestamp(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.pool,
                index(slot),
            );
        }
    }

    pub(crate) fn end(&self, command: vk::CommandBuffer, slot: usize) {
        // SAFETY: 同上。
        unsafe {
            self.device.cmd_write_timestamp(
                command,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.pool,
                index(slot) + 1,
            );
        }
    }

    /// fence 到达后读两个 u64；失败 ⇒ `None`（不 panic）。
    pub(crate) fn resolve(&self, slot: usize) -> Option<GpuTimerSample> {
        let mut values = [0u64; 2];
        // SAFETY: 池容量与查询数一致；`WAIT` 之外不加同步（调用方已等 fence）。
        unsafe {
            self.device
                .get_query_pool_results(
                    self.pool,
                    index(slot),
                    &mut values,
                    vk::QueryResultFlags::TYPE_64,
                )
                .ok()?;
        }
        let ticks = values[1].saturating_sub(values[0]);
        Some(GpuTimerSample {
            section: Section::Gpu,
            cpu_ns: 0,
            gpu_ns: (ticks as f64 * self.period_ns) as u64,
        })
    }
}

impl Drop for GpuTimer {
    fn drop(&mut self) {
        // SAFETY: 句柄归本结构独占；调用方先 `wait_idle`。
        unsafe {
            self.device.destroy_query_pool(self.pool, None);
        }
    }
}

fn index(slot: usize) -> u32 {
    slot as u32 * QUERIES_PER_FRAME
}
