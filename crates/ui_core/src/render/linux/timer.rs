use ash::vk;

use crate::{
    error::EngineError,
    render::{
        frame::MAX_FRAMES_IN_FLIGHT,
        linux::device::{SharedGpu, init_error},
        timer::{GpuTimerSample, Section},
    },
};

const QUERIES_PER_FRAME: u32 = 2;

pub(crate) struct GpuTimer {
    device: ash::Device,
    pool: vk::QueryPool,
    period_ns: f64,
}

impl GpuTimer {
    pub(crate) fn new(shared: &SharedGpu) -> Result<Option<Self>, EngineError> {
        let (period, valid_bits) = shared.timestamps();
        if valid_bits == 0 {
            return Ok(None);
        }
        let pool = unsafe {
            shared.raw().create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(QUERIES_PER_FRAME * MAX_FRAMES_IN_FLIGHT as u32),
                None,
            )
        }
        .map_err(|error| init_error("create_query_pool", format!("{error:?}")))?;
        Ok(Some(Self {
            device: shared.raw().clone(),
            pool,
            period_ns: f64::from(period),
        }))
    }

    pub(crate) fn begin(&self, command: vk::CommandBuffer, slot: usize) {
        unsafe {
            self.device
                .cmd_reset_query_pool(command, self.pool, index(slot), QUERIES_PER_FRAME);
            self.device.cmd_write_timestamp(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                self.pool,
                index(slot),
            );
        }
    }

    pub(crate) fn end(&self, command: vk::CommandBuffer, slot: usize) {
        unsafe {
            self.device.cmd_write_timestamp(
                command,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                self.pool,
                index(slot) + 1,
            );
        }
    }

    pub(crate) fn resolve(&self, slot: usize) -> Option<GpuTimerSample> {
        let mut values = [0u64; 2];
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
        unsafe {
            self.device.destroy_query_pool(self.pool, None);
        }
    }
}

fn index(slot: usize) -> u32 {
    slot as u32 * QUERIES_PER_FRAME
}
