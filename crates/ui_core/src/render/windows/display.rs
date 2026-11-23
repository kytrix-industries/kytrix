use std::sync::atomic::{AtomicU64, Ordering};

static EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn note_display_change() {
    EPOCH.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn epoch() -> u64 {
    EPOCH.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{epoch, note_display_change};

    #[test]
    fn every_registered_display_change_advances_the_epoch() {
        let first = epoch();
        note_display_change();
        let second = epoch();
        assert!(second > first, "恢复代际必须单调前进：{first} → {second}");
        note_display_change();
        assert!(epoch() > second, "连续两次恢复必须各推进一步");
    }
}
