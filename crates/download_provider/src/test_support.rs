use tokio::runtime::Handle;

pub(crate) fn test_handle() -> Handle {
    static SHARED: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

    SHARED
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("a test runtime must build")
        })
        .handle()
        .clone()
}
