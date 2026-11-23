use crate::event::ScrollPhase;

const CONTINUOUS_AXIS_UNITS_PER_LINE: f32 = 10.0;

const VALUE120_UNITS_PER_LINE: f32 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScrollSource {
    #[default]
    Unknown,
    Wheel,
    Finger,
    Continuous,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScrollEvent {
    Source(ScrollSource),
    Discrete { axis: ScrollAxis, notches: i32 },
    Value120 { axis: ScrollAxis, value120: i32 },
    Continuous { axis: ScrollAxis, value: f32 },
    Stop { axis: ScrollAxis },
    Frame,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollOutput {
    pub delta: (f32, f32),
    pub finger: bool,
    pub phase: ScrollPhase,
}

#[derive(Clone, Copy, Debug, Default)]
struct ScrollAccum {
    notches_x: i32,
    notches_y: i32,
    value120_x: i32,
    value120_y: i32,
    continuous_x: f32,
    continuous_y: f32,
}

impl ScrollAccum {
    fn note_notches(&mut self, axis: ScrollAxis, notches: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.notches_x,
            ScrollAxis::Vertical => &mut self.notches_y,
        };
        *slot = slot.saturating_add(notches);
    }

    fn note_value120(&mut self, axis: ScrollAxis, value120: i32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.value120_x,
            ScrollAxis::Vertical => &mut self.value120_y,
        };
        *slot = slot.saturating_add(value120);
    }

    fn note_continuous(&mut self, axis: ScrollAxis, value: f32) {
        let slot = match axis {
            ScrollAxis::Horizontal => &mut self.continuous_x,
            ScrollAxis::Vertical => &mut self.continuous_y,
        };
        *slot += value;
    }

    fn has_discrete(self) -> bool {
        self.notches_x != 0 || self.notches_y != 0 || self.value120_x != 0 || self.value120_y != 0
    }

    fn is_empty(self) -> bool {
        !self.has_discrete() && self.continuous_x == 0.0 && self.continuous_y == 0.0
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn wheel_lines(self) -> (f32, f32) {
        (
            lines_for(self.notches_x, self.value120_x, self.continuous_x),
            lines_for(self.notches_y, self.value120_y, self.continuous_y),
        )
    }

    fn finger_pixels(self) -> (f32, f32) {
        (self.continuous_x, self.continuous_y)
    }
}

fn lines_for(notches: i32, value120: i32, continuous: f32) -> f32 {
    if notches != 0 {
        notches as f32
    } else if value120 != 0 {
        value120 as f32 / VALUE120_UNITS_PER_LINE
    } else {
        continuous / CONTINUOUS_AXIS_UNITS_PER_LINE
    }
}

#[derive(Default)]
pub(crate) struct PointerScroll {
    accum: ScrollAccum,
    source: ScrollSource,
    stop_pending: bool,
    active: bool,
    gesture_open: bool,
    frame_has_data: bool,
    horizontal_active: bool,
    vertical_active: bool,
}

impl PointerScroll {
    pub(crate) fn apply(&mut self, event: ScrollEvent) -> Option<ScrollOutput> {
        match event {
            ScrollEvent::Source(source) => {
                let changed = source != self.source;
                let closes_previous = changed && self.gesture_open && !self.frame_has_data;
                let output = if closes_previous {
                    self.active = false;
                    self.gesture_open = false;
                    self.horizontal_active = false;
                    self.vertical_active = false;
                    Some(ScrollOutput {
                        delta: (0.0, 0.0),
                        finger: self.frame_is_finger(),
                        phase: ScrollPhase::End,
                    })
                } else {
                    None
                };
                self.stop_pending = false;
                self.source = source;
                output
            }
            ScrollEvent::Discrete { axis, notches } => {
                self.accum.note_notches(axis, notches);
                self.note_axis_data(axis);
                None
            }
            ScrollEvent::Value120 { axis, value120 } => {
                self.accum.note_value120(axis, value120);
                self.note_axis_data(axis);
                None
            }
            ScrollEvent::Continuous { axis, value } => {
                self.accum.note_continuous(axis, value);
                self.note_axis_data(axis);
                None
            }
            ScrollEvent::Stop { axis } => {
                match axis {
                    ScrollAxis::Horizontal => self.horizontal_active = false,
                    ScrollAxis::Vertical => self.vertical_active = false,
                }
                if self.horizontal_active || self.vertical_active {
                    return None;
                }
                if !self.gesture_open {
                    return None;
                }
                self.active = false;
                self.gesture_open = false;
                self.stop_pending = true;
                Some(ScrollOutput {
                    delta: (0.0, 0.0),
                    finger: self.frame_is_finger(),
                    phase: ScrollPhase::End,
                })
            }
            ScrollEvent::Frame => self.flush_frame(),
        }
    }

    fn flush_frame(&mut self) -> Option<ScrollOutput> {
        let stop_pending = std::mem::take(&mut self.stop_pending);
        let finger = self.frame_is_finger();
        let output = if self.accum.is_empty() {
            None
        } else {
            let (x, y) = if finger {
                self.accum.finger_pixels()
            } else {
                self.accum.wheel_lines()
            };
            let phase = if finger {
                let phase = if self.active {
                    ScrollPhase::Update
                } else {
                    ScrollPhase::Begin
                };
                self.active = true;
                phase
            } else {
                ScrollPhase::Discrete
            };
            Some(ScrollOutput {
                delta: (-x, -y),
                finger,
                phase,
            })
        };
        self.accum.clear();
        self.frame_has_data = false;
        if stop_pending {
            self.source = ScrollSource::Unknown;
            self.horizontal_active = false;
            self.vertical_active = false;
            self.active = false;
            self.gesture_open = false;
        }
        output
    }

    fn note_axis_data(&mut self, axis: ScrollAxis) {
        match axis {
            ScrollAxis::Horizontal => self.horizontal_active = true,
            ScrollAxis::Vertical => self.vertical_active = true,
        }
        self.gesture_open = true;
        self.frame_has_data = true;
    }

    fn frame_is_finger(&self) -> bool {
        !self.accum.has_discrete() && matches!(self.source, ScrollSource::Finger)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTINUOUS_AXIS_UNITS_PER_LINE, PointerScroll, ScrollAccum, ScrollAxis, ScrollEvent,
        ScrollPhase, ScrollSource,
    };

    fn finger_frame(value: f32) -> Vec<ScrollEvent> {
        vec![
            ScrollEvent::Source(ScrollSource::Finger),
            ScrollEvent::Continuous {
                axis: ScrollAxis::Vertical,
                value,
            },
            ScrollEvent::Frame,
        ]
    }

    #[test]
    fn discrete_scroll_is_one_line_per_120_units() {
        let mut one_notch = ScrollAccum::default();
        one_notch.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(one_notch.wheel_lines().1, 1.0, "一格 = 120/120 = 1 行");

        let mut two_notches = ScrollAccum::default();
        two_notches.note_value120(ScrollAxis::Vertical, 240);
        assert_eq!(two_notches.wheel_lines().1, 2.0, "两格 = 2 行");
    }

    #[test]
    fn a_v5_server_that_only_sends_axis_still_yields_one_line() {
        let mut one_notch = ScrollAccum::default();
        one_notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            one_notch.wheel_lines().1,
            1.0,
            "一格连续值 = 1 行（旧实现是 0.083）"
        );
    }

    #[test]
    fn axis_discrete_notches_win_over_the_continuous_approximation() {
        let mut notch = ScrollAccum::default();
        notch.note_notches(ScrollAxis::Vertical, 1);
        notch.note_continuous(ScrollAxis::Vertical, 15.0);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "axis_discrete 的 1 是**一格**，不是 1 个 surface 单位；axis = 15 是同一格的近似值"
        );
    }

    #[test]
    fn a_v5_server_that_only_sends_axis_discrete_still_yields_lines() {
        let mut accum = ScrollAccum::default();
        accum.note_notches(ScrollAxis::Vertical, 2);
        accum.note_notches(ScrollAxis::Horizontal, -1);
        assert_eq!(
            accum.wheel_lines(),
            (-1.0, 2.0),
            "两格 = 2 行；反向一格 = -1 行（轴方向保留）"
        );
    }

    #[test]
    fn the_three_representations_never_add_up() {
        let mut notch = ScrollAccum::default();
        notch.note_notches(ScrollAxis::Vertical, 1);
        notch.note_value120(ScrollAxis::Vertical, 120);
        notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "格数是精确值：不得加上 value120 与连续值的表示"
        );
    }

    #[test]
    fn wheel_both_channels_yield_one_line() {
        let mut notch = ScrollAccum::default();
        notch.note_value120(ScrollAxis::Vertical, 120);
        notch.note_continuous(ScrollAxis::Vertical, CONTINUOUS_AXIS_UNITS_PER_LINE);
        assert_eq!(
            notch.wheel_lines().1,
            1.0,
            "两条通路是同一格滚动的两种表示，必须二选一而不是相加"
        );
    }

    #[test]
    fn high_resolution_value120_and_continuous_agree() {
        let mut from_discrete = ScrollAccum::default();
        from_discrete.note_value120(ScrollAxis::Vertical, 30);
        let mut from_continuous = ScrollAccum::default();
        from_continuous.note_continuous(ScrollAxis::Vertical, 2.5);
        let mut both = ScrollAccum::default();
        both.note_value120(ScrollAxis::Vertical, 30);
        both.note_continuous(ScrollAxis::Vertical, 2.5);

        assert_eq!(from_discrete.wheel_lines().1, 0.25);
        assert_eq!(from_continuous.wheel_lines().1, 0.25);
        assert_eq!(
            both.wheel_lines().1,
            0.25,
            "两路同时到达也不得叠加（旧实现是 0.5）"
        );
    }

    #[test]
    fn per_axis_mixing_uses_each_axiss_available_channel() {
        let mut mixed = ScrollAccum::default();
        mixed.note_continuous(ScrollAxis::Horizontal, CONTINUOUS_AXIS_UNITS_PER_LINE);
        mixed.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(
            mixed.wheel_lines(),
            (1.0, 1.0),
            "水平连续 / 垂直离散必须逐轴判定，整帧二选一会丢掉其中一根轴"
        );

        let mut notched = ScrollAccum::default();
        notched.note_notches(ScrollAxis::Horizontal, 1);
        notched.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(
            notched.wheel_lines(),
            (1.0, 1.0),
            "axis_discrete 与 axis_value120 也必须逐轴各取其一"
        );
    }

    #[test]
    fn finger_pixels_are_the_continuous_values() {
        let mut touchpad = ScrollAccum::default();
        touchpad.note_continuous(ScrollAxis::Vertical, -24.0);
        assert_eq!(
            touchpad.finger_pixels().1,
            -24.0,
            "触控板按像素 1:1（不是行）"
        );
        touchpad.note_value120(ScrollAxis::Vertical, 120);
        assert_eq!(touchpad.wheel_lines().1, 1.0, "滚轮通路用离散值 = 1 行");
    }

    #[test]
    fn value120_events_accumulate_until_the_frame_is_flushed() {
        let mut scroll = ScrollAccum::default();
        scroll.note_value120(ScrollAxis::Vertical, 60);
        scroll.note_value120(ScrollAxis::Vertical, 60);
        assert_eq!(scroll.wheel_lines().1, 1.0, "两条 60 合起来是一格");
        assert!(!scroll.is_empty());
        let mut device = PointerScroll::default();
        assert_eq!(
            device.apply(ScrollEvent::Frame),
            None,
            "没有分量时不产出事件"
        );
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后下一帧从零开始");
        assert_eq!(scroll.wheel_lines(), (0.0, 0.0));
        assert_eq!(scroll.finger_pixels(), (0.0, 0.0));
    }

    #[test]
    fn per_axis_discrete_notches_are_single_events_by_protocol() {
        let mut scroll = ScrollAccum::default();
        scroll.note_notches(ScrollAxis::Vertical, 1);
        scroll.note_notches(ScrollAxis::Horizontal, -1);
        assert_eq!(
            scroll.wheel_lines(),
            (-1.0, 1.0),
            "两根轴各一条（合法输入）"
        );
    }

    #[test]
    fn an_empty_accumulator_reports_empty() {
        let mut scroll = ScrollAccum::default();
        assert!(scroll.is_empty());
        scroll.note_continuous(ScrollAxis::Vertical, -3.0);
        assert!(!scroll.is_empty());
        scroll.clear();
        assert!(scroll.is_empty(), "clear 之后必须回到空态");
    }

    #[test]
    fn a_finger_frame_yields_pixels_and_keeps_the_gesture_open() {
        let mut scroll = PointerScroll::default();
        let mut outputs = Vec::new();
        for event in finger_frame(-24.0) {
            if let Some(output) = scroll.apply(event) {
                outputs.push(output);
            }
        }
        assert_eq!(outputs.len(), 1, "一个 frame 只产出一条滚动事件");
        assert_eq!(
            outputs[0].delta,
            (0.0, 24.0),
            "手指按像素 1:1，方向取反（协议正方向 ↔ 引擎 delta）"
        );
        assert!(outputs[0].finger, "finger 源 ⇒ 像素单位");
        assert_eq!(outputs[0].phase, ScrollPhase::Begin);

        assert_eq!(
            scroll.apply(ScrollEvent::Continuous {
                axis: ScrollAxis::Vertical,
                value: -12.0,
            }),
            None
        );
        let update = scroll
            .apply(ScrollEvent::Frame)
            .expect("第二帧必须产出滚动事件");
        assert_eq!(update.delta, (0.0, 12.0));
        assert!(update.finger, "手势内没有 source 的帧仍按 finger 处理");
        assert_eq!(update.phase, ScrollPhase::Update);
    }

    #[test]
    fn a_value120_frame_is_a_wheel_even_inside_a_finger_gesture() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        assert_eq!(
            scroll.apply(ScrollEvent::Value120 {
                axis: ScrollAxis::Vertical,
                value120: 120,
            }),
            None
        );
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("离散帧必须产出滚动事件");
        assert_eq!(output.delta, (0.0, -1.0), "120/120 = 1 行");
        assert!(
            !output.finger,
            "离散通道是滚轮的充分证据：不得沿用上一手势的 finger"
        );
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn an_axis_discrete_frame_is_a_wheel_and_carries_its_notches() {
        let mut scroll = PointerScroll::default();
        assert_eq!(
            scroll.apply(ScrollEvent::Discrete {
                axis: ScrollAxis::Vertical,
                notches: 1,
            }),
            None
        );
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 15.0,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("一格滚轮必须产出一条滚动事件");
        assert_eq!(output.delta, (0.0, -1.0), "axis_discrete = 1 格 = 1 行");
        assert!(!output.finger);
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn a_sourceless_continuous_frame_keeps_the_last_known_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 30.0,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert!(first.finger);

        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -20.0,
        });
        let second = scroll.apply(ScrollEvent::Frame).expect("第二帧必须有输出");
        assert!(
            second.finger,
            "纯连续帧没有新 source ⇒ 沿用最后一次已知源（finger），不得退化成滚轮"
        );
        assert_eq!(second.delta, (0.0, 20.0));
        assert_eq!(second.phase, ScrollPhase::Update);
    }

    #[test]
    fn a_single_axis_gesture_ends_with_one_end_and_resets_the_source() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("最后一根轴停止 ⇒ 当场产出 End");
        assert_eq!(end.delta, (0.0, 0.0), "End 是零增量事件");
        assert!(end.finger, "finger 手势的 End 仍是像素单位");
        assert_eq!(end.phase, ScrollPhase::End);
        assert_eq!(scroll.apply(ScrollEvent::Frame), None, "stop 帧没有数据");

        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: 15.0,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("滚轮帧必须产出事件");
        assert!(!wheel.finger, "全部轴 stop 之后源复位为 Unknown（按滚轮）");
        assert_eq!(wheel.delta, (0.0, -1.5), "15 / 约定 10 = 1.5 行");
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn stopping_one_axis_keeps_the_two_axis_gesture_alive() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -24.0,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert!(first.finger);
        assert_eq!(first.delta, (12.0, 24.0));

        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            }),
            None,
            "还有轴在动：stop 不得产出 End、也不得复位源"
        );
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -6.0,
        });
        let continuing = scroll
            .apply(ScrollEvent::Frame)
            .expect("继续轴必须产出事件");
        assert!(
            continuing.finger,
            "继续的那根轴必须仍按手势源（finger）解释，不得掉回滚轮"
        );
        assert_eq!(continuing.delta, (6.0, 0.0));
        assert_eq!(continuing.phase, ScrollPhase::Update, "手势仍在进行");

        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            })
            .expect("全部轴停止 ⇒ 产出 End");
        assert_eq!(end.phase, ScrollPhase::End);
        assert!(end.finger);
    }

    #[test]
    fn stopping_both_axes_produces_exactly_one_end_and_resets_the_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -10.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -20.0,
        });
        scroll.apply(ScrollEvent::Frame);

        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "还剩垂直轴 ⇒ 不产 End"
        );
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("最后一根轴停止 ⇒ 产出一条 End");
        assert_eq!(end.phase, ScrollPhase::End);
        assert_eq!(scroll.apply(ScrollEvent::Frame), None, "End 不重复");

        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -15.0,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("后续帧必须产出事件");
        assert!(!wheel.finger, "全部轴 stop 之后源复位 ⇒ 按滚轮");
        assert_eq!(wheel.delta, (0.0, 1.5));
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn axis_stop_with_a_continuing_axis_in_the_same_frame_keeps_the_pre_stop_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -9.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -4.0,
        });
        scroll.apply(ScrollEvent::Frame);

        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "垂直轴仍活跃 ⇒ 不产 End"
        );
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -18.0,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("继续轴必须产出事件");
        assert!(
            output.finger,
            "axis_stop 同帧的 axis 属于停止前的手势（不得错判成滚轮）"
        );
        assert_eq!(output.delta, (0.0, 18.0));
        assert_eq!(output.phase, ScrollPhase::Update);
    }

    #[test]
    fn a_new_source_closes_a_gesture_whose_remaining_axis_never_stops() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Horizontal,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -12.0,
        });
        scroll.apply(ScrollEvent::Frame);
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None
        );
        let end = scroll
            .apply(ScrollEvent::Source(ScrollSource::Wheel))
            .expect("换源必须收尾旧手势（零增量 End）");
        assert_eq!(end.phase, ScrollPhase::End);
        assert!(end.finger, "旧手势是 finger ⇒ End 仍按像素单位");

        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 120,
        });
        let wheel = scroll
            .apply(ScrollEvent::Frame)
            .expect("新序列必须产出事件");
        assert!(!wheel.finger);
        assert_eq!(wheel.delta, (0.0, -1.0));
        assert_eq!(wheel.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn an_axis_stop_for_an_axis_that_never_moved_does_not_end_the_gesture() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "水平轴从未活跃：它的 stop 不得结束垂直轴的手势"
        );
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("垂直轴停止才结束手势");
        assert_eq!(end.phase, ScrollPhase::End);
    }

    #[test]
    fn frames_do_not_accumulate_across_the_frame_boundary() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 120,
        });
        let first = scroll.apply(ScrollEvent::Frame).expect("第一帧必须有输出");
        assert_eq!(first.delta, (0.0, -1.0));

        scroll.apply(ScrollEvent::Value120 {
            axis: ScrollAxis::Vertical,
            value120: 240,
        });
        let second = scroll.apply(ScrollEvent::Frame).expect("第二帧必须有输出");
        assert_eq!(
            second.delta,
            (0.0, -2.0),
            "第二帧只含自己的 240/120 = 2 行（若 Frame 忘了清空会是 3 行）"
        );
    }

    #[test]
    fn an_unknown_source_is_a_wheel_not_a_finger() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -CONTINUOUS_AXIS_UNITS_PER_LINE,
        });
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("滚轮帧必须产出事件");
        assert!(!output.finger, "未知源按滚轮处理");
        assert_eq!(output.delta, (0.0, 1.0));
        assert_eq!(output.phase, ScrollPhase::Discrete);
    }

    #[test]
    fn explicit_non_finger_sources_are_lines() {
        for source in [ScrollSource::Wheel, ScrollSource::Continuous] {
            let mut scroll = PointerScroll::default();
            scroll.apply(ScrollEvent::Source(source));
            scroll.apply(ScrollEvent::Continuous {
                axis: ScrollAxis::Horizontal,
                value: CONTINUOUS_AXIS_UNITS_PER_LINE,
            });
            let output = scroll
                .apply(ScrollEvent::Frame)
                .expect("非手指源必须产出事件");
            assert!(!output.finger, "{source:?} 必须按行");
            assert_eq!(output.delta, (-1.0, 0.0));
        }
    }

    #[test]
    fn an_empty_frame_yields_no_event() {
        let mut scroll = PointerScroll::default();
        assert_eq!(scroll.apply(ScrollEvent::Frame), None);
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            }),
            None,
            "没有开始过的手势：零散 axis_stop 不得凭空产 End"
        );
        assert_eq!(scroll.apply(ScrollEvent::Frame), None);
    }

    #[test]
    fn a_same_source_on_every_frame_does_not_split_the_gesture() {
        let mut scroll = PointerScroll::default();
        let mut outputs = Vec::new();
        for frame in 0..3 {
            for event in [
                ScrollEvent::Source(ScrollSource::Finger),
                ScrollEvent::Continuous {
                    axis: ScrollAxis::Vertical,
                    value: -8.0,
                },
                ScrollEvent::Frame,
            ] {
                if let Some(output) = scroll.apply(event) {
                    outputs.push((frame, output));
                }
            }
        }
        let phases: Vec<_> = outputs.iter().map(|(_, output)| output.phase).collect();
        assert_eq!(
            phases,
            vec![ScrollPhase::Begin, ScrollPhase::Update, ScrollPhase::Update],
            "同值 source 每帧重复不得触发 End / 重置手势阶段"
        );
        assert!(
            outputs
                .iter()
                .all(|(_, output)| output.phase != ScrollPhase::End),
            "三帧同源手势不得出现 End（换源兜底只对**真换源**生效）"
        );
    }

    #[test]
    fn a_gesture_produces_exactly_one_end_even_with_stray_stops() {
        let mut scroll = PointerScroll::default();
        for event in finger_frame(-24.0) {
            scroll.apply(event);
        }
        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("最后一根轴停止 ⇒ 唯一一条 End");
        assert_eq!(end.phase, ScrollPhase::End);
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "同帧再来的从未活跃轴 stop 不得补发第二条 End"
        );
        scroll.apply(ScrollEvent::Frame);
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "跨帧的零散 stop 同样不得补发 End（本手势已收尾）"
        );
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            }),
            None,
            "同一手势不会因为重复 stop 再收尾一次"
        );
    }

    #[test]
    fn a_source_arriving_after_this_frames_data_only_updates_the_source() {
        let mut scroll = PointerScroll::default();
        scroll.apply(ScrollEvent::Source(ScrollSource::Finger));
        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -8.0,
        });
        scroll.apply(ScrollEvent::Frame);

        scroll.apply(ScrollEvent::Continuous {
            axis: ScrollAxis::Vertical,
            value: -6.0,
        });
        assert_eq!(
            scroll.apply(ScrollEvent::Source(ScrollSource::Wheel)),
            None,
            "本帧已有数据：换源只更新源，不得产出 End"
        );
        assert_eq!(
            scroll.apply(ScrollEvent::Stop {
                axis: ScrollAxis::Horizontal,
            }),
            None,
            "本帧刚标记的垂直轴仍然活跃：不得被换源清掉"
        );
        let output = scroll
            .apply(ScrollEvent::Frame)
            .expect("本帧必须有滚动输出");
        assert!(
            !output.finger,
            "本帧的权威源已更新成 Wheel ⇒ 连续值按行（协议：source 适用于本帧事件）"
        );
        assert_eq!(output.delta, (0.0, 0.6), "-6 / 约定 10 = 0.6 行");

        let end = scroll
            .apply(ScrollEvent::Stop {
                axis: ScrollAxis::Vertical,
            })
            .expect("垂直轴停止 ⇒ 本手势唯一一条 End");
        assert_eq!(end.phase, ScrollPhase::End);
    }
}
