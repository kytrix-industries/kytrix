use std::{sync::Arc, time::Instant};

use wayland_client::{
    QueueHandle,
    protocol::{wl_registry, wl_surface},
};
use wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation;

use super::surface::WaylandState;
use crate::{
    platform::wayland::presentation::{PresentationValidity, SharedPresentationValidity},
    render::present_evidence::presented_event_instant,
    window::{FrameIndex, WindowId},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct FeedbackData {
    window: WindowId,
    frame: FrameIndex,
}

impl FeedbackData {
    pub(crate) const fn new(window: WindowId, frame: FrameIndex) -> Self {
        Self { window, frame }
    }

    pub(crate) const fn window(self) -> WindowId {
        self.window
    }

    pub(crate) const fn frame(self) -> FrameIndex {
        self.frame
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Exemption {
    ProtocolMissing,
    ClockUnavailable,
    Rejected,
    Unmatched,
    ProtocolRemoved,
}

#[derive(Debug, Default)]
pub(crate) struct Exemptions {
    protocol_missing: u64,
    clock_unavailable: u64,
    rejected: u64,
    unmatched: u64,
    protocol_removed: u64,
}

impl Exemptions {
    pub(crate) fn note(&mut self, kind: Exemption) {
        let count = match kind {
            Exemption::ProtocolMissing => &mut self.protocol_missing,
            Exemption::ClockUnavailable => &mut self.clock_unavailable,
            Exemption::Rejected => &mut self.rejected,
            Exemption::Unmatched => &mut self.unmatched,
            Exemption::ProtocolRemoved => &mut self.protocol_removed,
        };
        *count = count.saturating_add(1);
        if *count == 1 {
            match kind {
                Exemption::ProtocolMissing => eprintln!(
                    "kytrix: compositor has no wp_presentation; presented_at stays None on this \
                     connection (exemption counted, no fabricated timestamps)"
                ),
                Exemption::ClockUnavailable => eprintln!(
                    "kytrix: clock_gettime for the presentation clock failed; presented_at stays \
                     None for the affected frames (exemption counted)"
                ),
                Exemption::Rejected => eprintln!(
                    "kytrix: wp_presentation timestamp rejected by the evidence contract \
                     (unsupported clock domain / invalid nsec / implausible delta); \
                     presented_at stays None for the affected frames"
                ),
                Exemption::Unmatched => eprintln!(
                    "kytrix: wp_presentation presented event is out of submission order \
                     (stale binding); evidence dropped for that frame"
                ),
                Exemption::ProtocolRemoved => eprintln!(
                    "kytrix: wp_presentation global was removed at runtime; existing presented \
                     feedback handles are strictly invalidated (presented_at stays None for \
                     windows rendered before the removal; recreate the window renderer to retry)"
                ),
            }
        }
    }
}

pub(crate) struct Presentation {
    proxy: Option<WpPresentation>,
    global_name: Option<u32>,
    clock_id: Option<i32>,
    validity: Option<SharedPresentationValidity>,
    exemptions: Exemptions,
}

impl Presentation {
    pub(crate) fn new() -> Self {
        Self {
            proxy: None,
            global_name: None,
            clock_id: None,
            validity: None,
            exemptions: Exemptions::default(),
        }
    }

    pub(crate) fn bind(
        &mut self,
        registry: &wl_registry::WlRegistry,
        queue: &QueueHandle<WaylandState>,
        name: u32,
        version: u32,
    ) {
        if self.proxy.is_some() || version == 0 {
            return;
        }
        let proxy = registry.bind::<WpPresentation, _, _>(name, version.min(1), queue, ());
        self.proxy = Some(proxy);
        self.global_name = Some(name);
        self.validity = None;
    }

    pub(crate) fn unbind(&mut self, name: u32) {
        if self.global_name != Some(name) {
            return;
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.destroy();
            if let Some(validity) = self.validity.take()
                && validity.invalidate()
            {
                self.exemptions.note(Exemption::ProtocolRemoved);
            }
        }
        self.global_name = None;
    }

    pub(crate) fn note_clock_id(&mut self, clock_id: u32) {
        self.clock_id = i32::try_from(clock_id).ok();
    }

    pub(crate) fn handle(
        &mut self,
        window: WindowId,
        queue: &QueueHandle<WaylandState>,
        surface: wl_surface::WlSurface,
    ) -> Option<PresentationHandle> {
        let Some(proxy) = self.proxy.clone() else {
            self.exemptions.note(Exemption::ProtocolMissing);
            return None;
        };
        let validity = self
            .validity
            .get_or_insert_with(|| Arc::new(PresentationValidity::new()))
            .clone();
        Some(PresentationHandle {
            proxy,
            queue: queue.clone(),
            window,
            surface,
            validity,
        })
    }

    pub(crate) fn presented_instant(
        &mut self,
        tv_sec_hi: u32,
        tv_sec_lo: u32,
        tv_nsec: u32,
    ) -> Option<Instant> {
        let Some(clock_id) = self.clock_id else {
            self.exemptions.note(Exemption::Rejected);
            return None;
        };
        if !crate::platform::wayland::presentation::presentation_clock_supported(clock_id) {
            self.exemptions.note(Exemption::Rejected);
            return None;
        }
        let anchor = Instant::now();
        let Some(clock_secs) = clock_seconds(clock_id) else {
            self.exemptions.note(Exemption::ClockUnavailable);
            return None;
        };
        let now = Instant::now();
        let instant = presented_event_instant(
            tv_sec_hi, tv_sec_lo, tv_nsec, clock_id, clock_secs, anchor, now,
        );
        if instant.is_none() {
            self.exemptions.note(Exemption::Rejected);
        }
        instant
    }

    pub(crate) fn note_unmatched_feedback(&mut self) {
        self.exemptions.note(Exemption::Unmatched);
    }
}

pub(crate) struct PresentationHandle {
    proxy: WpPresentation,
    queue: QueueHandle<WaylandState>,
    window: WindowId,
    surface: wl_surface::WlSurface,
    validity: SharedPresentationValidity,
}

impl PresentationHandle {
    pub(crate) fn request_feedback(&self, frame: FrameIndex) -> bool {
        if !self.validity.is_valid() {
            return false;
        }
        let _feedback = self.proxy.feedback(
            &self.surface,
            &self.queue,
            FeedbackData::new(self.window, frame),
        );
        true
    }
}

fn clock_seconds(clock_id: i32) -> Option<f64> {
    let mut timespec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(clock_id, &mut timespec) };
    if result != 0 {
        return None;
    }
    if !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return None;
    }
    Some(timespec.tv_sec as f64 + timespec.tv_nsec as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use super::{Exemption, Exemptions, clock_seconds};
    use crate::render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME};

    #[test]
    fn the_presentation_clock_is_read_through_libc() {
        let first = clock_seconds(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC 必须可用");
        let second = clock_seconds(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC 必须可用");
        assert!(first.is_finite(), "读数必须是有限值");
        assert!(second >= first, "单调时钟不得回退");
        assert!(
            first > 0.0,
            "单调时钟自开机起计数 ⇒ 读数应为正（布局/字段错位会读出 0）"
        );
        let realtime = clock_seconds(CLOCK_REALTIME).expect("CLOCK_REALTIME 必须可用");
        assert!(realtime > 1.5e9, "REALTIME 读数应在 Unix 纪元量级");
        assert!(clock_seconds(-1).is_none(), "非法 clockid 必须失败");
    }

    #[test]
    fn exemption_counters_are_monotonic_and_do_not_forge_evidence() {
        let mut exemptions = Exemptions::default();
        exemptions.note(Exemption::ProtocolMissing);
        exemptions.note(Exemption::ProtocolMissing);
        exemptions.note(Exemption::Rejected);
        let debug = format!("{exemptions:?}");
        assert!(
            debug.contains("protocol_missing: 2"),
            "同类重复计数: {debug}"
        );
        assert!(debug.contains("rejected: 1"), "分类计数: {debug}");
        assert!(debug.contains("clock_unavailable: 0"), "未发生的类保持 0");
    }
}
