pub type CancelCheck = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;
