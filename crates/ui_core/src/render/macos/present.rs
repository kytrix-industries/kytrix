//! 呈现（drawable 上屏）的唯一入口 + 接口/线程纪律的回归防线。
//!
//! # 为什么窗口路径**不能**用 `presentDrawable:atTime:`
//!
//! 窗口路径的 drawable 由 `CAMetalDisplayLink` 给出，而 display link **独占呈现时刻**：它给的
//! `CAMetalDisplayLinkUpdate.targetPresentationTimestamp` 就是这一帧被排定的上屏时刻。因此
//! `CAMetalDrawable` 拒绝任何显式时刻，运行时直接抛 ObjC 异常：
//!
//! ```text
//! CAMetalDrawableInvalidOperation: -presentAtTime should not be called when using CAMetalDisplayLink.
//! ```
//!
//! 抛出点在 **Metal 自己的队列**上（`com.Metal.CompletionQueueDispatch` →
//! `-[_MTLCommandBuffer presentDrawable:options:]_block_invoke` → `-[CAMetalDrawable
//! presentWithOptions:]`），时间点是命令缓冲被驱动调度（`didScheduleWithStartTime:`）时 ——
//! 也就是说 `presentDrawable:atTime:` 返回之后才炸，**没有任何 Rust 帧能拦住它**。展开穿透
//! FFI 的边界后进程直接 SIGSEGV（真机 `EXC_BAD_ACCESS` + `SIGSEGV`，见 P0 报告）。
//!
//! Apple 的 `CAMetalDisplayLinkDelegate.metalDisplayLink(_:needsUpdate:)` 文档也只承诺一个
//! 「调用 drawable 的 **`present()`** 的截止时刻」（"a deadline to call its drawable's `present()`
//! method"），从不提 `present(at:)`；`MTLCommandBuffer.present(_:atTime:)` 的文档说明它就是
//! 「命令队列调度命令缓冲之后调用 drawable 的 `present(at:)`」—— 正是被 display link 禁掉的那条路。
//!
//! 所以本后端只用 `MTLCommandBuffer::presentDrawable:`（等价 `present(_:)`，**没有时刻参数**）：
//! 呈现节奏完全由 display link 决定 —— 它给 drawable，我们画完就尽快提交。
//!
//! # 上屏时刻从哪里来
//!
//! display link 的 `CAMetalDisplayLinkUpdate.targetPresentationTimestamp` 是「这张 drawable 被
//! 排定的上屏时刻」。它不能传给 `presentDrawable:atTime:`（见上），但可以在提交时记下来做
//! **tick 推断**：下一个 tick 到达 ⇒ 该 vsync 的呈现截止时刻已过 ⇒ 上一帧已上屏；推断出的上屏
//! 时刻误差 ≤1 个 vsync。实现见 `render/macos/renderer.rs` 的 `presentation_instant` 与
//! `FeedbackQueue::observe_tick`。`addPresentedHandler:` / `MTLDrawable.presentedTime` 在
//! display-link 驱动的 drawable 上不可靠（真机实测只回调 3 次、`presentedTime` 恒为 0），本后端
//! **不使用**它。
//!
//! # 线程纪律
//!
//! drawable 在**主线程**取得（display link delegate），编码 / 提交 / present 全在 `kytrix-metal`
//! 线程。`presentDrawable:` 只是把「这一帧要上屏」登记进命令缓冲，真正的
//! `presentWithOptions:` 由驱动在自己的队列上执行，所以这次的跨线程本身是合法的；
//! 但**同一张 drawable 的取得与 present 必须落在同一个 GPU 线程的同一帧里** —— 把别的帧/
//! 别的线程的 drawable 交给本帧的命令缓冲，一样会在驱动里炸掉，因此这里有一道断言守卫。

use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer, MTLDrawable};

use crate::render::macos::device::on_gpu_thread;

/// present 之前的线程归属断言。
///
/// 这里刻意用 release 也生效的 `assert!`（而同模块的其它线程归属断言都是 `debug_assert!`）：
/// present 是**唯一不可恢复**的调用点 —— 走错线程的后果是驱动里抛 ObjC 异常、穿 FFI 后整个进程
/// SIGSEGV，没有任何机会发布错误或降级。代价只是每帧一次 thread-local 读。
fn assert_present_thread() {
    assert!(
        on_gpu_thread(),
        "Metal window frames may only be presented on the kytrix-metal thread"
    );
}

/// **唯一**的 present 入口：`MTLCommandBuffer::presentDrawable:`（无时刻参数，见模块文档）。
pub(crate) fn present(
    command: &ProtocolObject<dyn MTLCommandBuffer>,
    drawable: &ProtocolObject<dyn MTLDrawable>,
) {
    assert_present_thread();
    command.presentDrawable(drawable);
}

#[cfg(test)]
mod tests {
    use super::{assert_present_thread, on_gpu_thread};
    use crate::render::macos::device::enter_gpu_thread;

    /// 「drawable 取得 → 编码 → 提交 → present 全在同一条 GPU 线程」：任何别的线程（UI 线程、
    /// Metal 的完成/呈现回调线程）调用 present 入口都必须立刻炸在断言上。
    #[test]
    #[should_panic(expected = "kytrix-metal thread")]
    fn presenting_off_the_gpu_thread_is_a_contract_violation() {
        assert!(!on_gpu_thread(), "测试线程不是 GPU 线程");
        assert_present_thread();
    }

    /// GPU 线程上同一个入口不触发断言（`enter_gpu_thread` 是 GPU 循环入口打的标记）。
    #[test]
    fn presenting_on_the_gpu_thread_is_allowed() {
        std::thread::spawn(|| {
            enter_gpu_thread();
            assert_present_thread();
            assert!(on_gpu_thread());
        })
        .join()
        .expect("gpu thread");
    }
}
