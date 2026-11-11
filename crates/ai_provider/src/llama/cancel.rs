//! The cancellation check the worker polls while it works.

/// A cancellation check the worker calls before work that can be abandoned.
///
/// The worker calls it after receiving a request, before each prompt prefill
/// chunk and before each sampled token. Returning `true` stops the request with
/// [`ProviderError::Cancelled`](crate::error::ProviderError::Cancelled) and leaves
/// the worker free to serve the next command.
pub type CancelCheck = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;
