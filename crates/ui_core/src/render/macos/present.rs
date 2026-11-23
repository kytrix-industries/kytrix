use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer, MTLDrawable};

use crate::render::macos::device::on_gpu_thread;

fn assert_present_thread() {
    assert!(
        on_gpu_thread(),
        "Metal window frames may only be presented on the kytrix-metal thread"
    );
}

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

    #[test]
    #[should_panic(expected = "kytrix-metal thread")]
    fn presenting_off_the_gpu_thread_is_a_contract_violation() {
        assert!(!on_gpu_thread(), "测试线程不是 GPU 线程");
        assert_present_thread();
    }

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
