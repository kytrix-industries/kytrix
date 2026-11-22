//! 有界事件环形队列（R02-5）：`App::events` 的存储。
//!
//! 旧实现是 `Vec<(WindowId, Event)>`，只进不出：唯一的清理点是应用主动调用的
//! `drain_events()`。完全用 action / 信号驱动的应用（或忘了在 `on_frame` 里 drain 的应用）
//! 会让它单调增长 —— 1000 Hz 鼠标下约 40–60 KB/s，数小时即到 GB 级，`Vec` 反复扩容还会
//! 带来周期性 O(N) memcpy。
//!
//! 现在的语义与成熟实现一致（winit 的 `AboutToWait` 之后事件即失效、Qt 每轮清空 posted
//! 队列、Flutter engine 每帧丢弃已消费的 `PointerDataPacket`）：**容量固定**，超限丢最旧并
//! 计数（[`crate::app::App::events_dropped`]），`App::frame` 入口再做一次 O(1) 兜底裁剪。
//! 事件是「观测面」而不是可靠传输通道：引擎的交互分发在同一时刻就已经完成（见
//! `app::resources::dispatch`），丢最旧的事件不会让交互状态错乱。

use std::collections::VecDeque;

use crate::{event::Event, window::WindowId};

/// 队列容量（条）。
///
/// 1 条 ≈ 一个 `InputEvent`（指针事件 ~56 B）。1024 条在 1000 Hz 鼠标下也覆盖一整秒的输入，
/// 而按 60 fps 每帧 drain 一次的现实用量只有几十条；这个容量是「应用几帧没 drain 也不会丢」
/// 与「忘了 drain 也不会涨」之间的折中。
pub(crate) const EVENT_QUEUE_CAPACITY: usize = 1024;

/// 有界事件队列。
#[derive(Debug)]
pub(crate) struct EventQueue {
    /// 队列本体（`VecDeque` 的 `pop_front` 是 O(1)，`Vec` 的 `remove(0)` 是 O(N)）。
    items: VecDeque<(WindowId, Event)>,
    /// 累计丢弃条数（诊断；单调递增）。
    dropped: u64,
}

impl EventQueue {
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(EVENT_QUEUE_CAPACITY),
            dropped: 0,
        }
    }

    /// 入队；已满则丢**最旧**的一条并计数。
    pub(crate) fn push(&mut self, window: WindowId, event: Event) {
        if self.items.len() >= EVENT_QUEUE_CAPACITY {
            self.items.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.items.push_back((window, event));
    }

    /// 取走全部事件（顺序即产生顺序）。
    pub(crate) fn drain(&mut self) -> Vec<(WindowId, Event)> {
        self.items.drain(..).collect()
    }

    /// 取走 `keep` 认可的事件；其余**留在队列里**（顺序不变，丢弃计数不变）。
    ///
    /// 这是「事件属于谁」的引擎侧口径：一个只拥有自己窗口一部分事件的消费者（模态对话框把
    /// 文本 / 按键 / IME 交给自己的输入框）取走它那部分，应用的观测面仍然完整。
    pub(crate) fn drain_where(
        &mut self,
        mut keep: impl FnMut(WindowId, &Event) -> bool,
    ) -> Vec<(WindowId, Event)> {
        let mut taken = Vec::new();
        self.items.retain(|(window, event)| {
            if keep(*window, event) {
                taken.push((*window, event.clone()));
                false
            } else {
                true
            }
        });
        taken
    }

    /// 当前条数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// 累计丢弃条数。
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 帧入口的兜底裁剪（O(1) 判定）：正常情况下 `push` 已经保证不超容量，这里只覆盖
    /// 「容量常量被调小 / 队列被外部塞满」的病态情况。
    pub(crate) fn trim(&mut self) {
        while self.items.len() > EVENT_QUEUE_CAPACITY {
            self.items.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EVENT_QUEUE_CAPACITY, EventQueue};
    use crate::{event::Event, window::WindowId};

    fn window() -> WindowId {
        WindowId::for_test(1)
    }

    #[test]
    fn events_come_out_in_production_order() {
        let mut queue = EventQueue::new();
        queue.push(window(), Event::FocusChanged { focused: true });
        queue.push(window(), Event::FocusChanged { focused: false });

        assert_eq!(
            queue.drain(),
            vec![
                (window(), Event::FocusChanged { focused: true }),
                (window(), Event::FocusChanged { focused: false }),
            ]
        );
        assert_eq!(queue.len(), 0, "drain 之后回到空态");
    }

    /// R02-5 回归：应用忘记 drain 时队列必须有界（丢最旧并计数，而不是无界增长）。
    #[test]
    fn the_queue_is_bounded_and_counts_what_it_dropped() {
        let mut queue = EventQueue::new();
        for index in 0..EVENT_QUEUE_CAPACITY + 10 {
            queue.push(
                window(),
                Event::Resized {
                    size: crate::math::Size::new(index as f32, 1.0),
                },
            );
        }

        assert_eq!(queue.len(), EVENT_QUEUE_CAPACITY, "容量是硬上限");
        assert_eq!(queue.dropped(), 10);
        let drained = queue.drain();
        assert_eq!(drained.len(), EVENT_QUEUE_CAPACITY);
        // 丢的是**最旧**的 10 条：队首是第 11 条。
        assert_eq!(
            drained.first().map(|(_, event)| event.clone()),
            Some(Event::Resized {
                size: crate::math::Size::new(10.0, 1.0)
            })
        );
        assert_eq!(queue.dropped(), 10, "drain 不清丢弃计数（诊断是累计值）");
    }

    #[test]
    fn trim_is_a_no_op_while_within_capacity() {
        let mut queue = EventQueue::new();
        queue.push(window(), Event::CloseRequested);
        queue.trim();

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.dropped(), 0);
    }

    #[test]
    fn a_filtered_drain_leaves_the_rest_queued_in_order() {
        let mut queue = EventQueue::new();
        let other = WindowId::for_test(2);
        queue.push(window(), Event::FocusChanged { focused: true });
        queue.push(other, Event::CloseRequested);
        queue.push(
            window(),
            Event::Resized {
                size: crate::math::Size::new(8.0, 8.0),
            },
        );

        let taken = queue.drain_where(|target, _| target == window());

        assert_eq!(
            taken.len(),
            2,
            "only the events addressed to the asked-for window are taken"
        );
        assert_eq!(
            queue.drain(),
            vec![(other, Event::CloseRequested)],
            "the rest stays queued, so the application still observes it"
        );
        assert_eq!(queue.dropped(), 0, "filtering is not dropping");
    }
}
