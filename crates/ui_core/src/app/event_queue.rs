use std::collections::VecDeque;

use crate::{event::Event, window::WindowId};

pub(crate) const EVENT_QUEUE_CAPACITY: usize = 1024;

#[derive(Debug)]
pub(crate) struct EventQueue {
    items: VecDeque<(WindowId, Event)>,
    dropped: u64,
}

impl EventQueue {
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(EVENT_QUEUE_CAPACITY),
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, window: WindowId, event: Event) {
        if self.items.len() >= EVENT_QUEUE_CAPACITY {
            self.items.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.items.push_back((window, event));
    }

    pub(crate) fn drain(&mut self) -> Vec<(WindowId, Event)> {
        self.items.drain(..).collect()
    }

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

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

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
