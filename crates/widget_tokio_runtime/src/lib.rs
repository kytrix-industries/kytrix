//! The application Tokio runtime shared by every asynchronous service.
//!
//! Two modules, one rule: the runtime is read from [`global`], and work reaches
//! it through [`runtime::Runtime`], which carries the context-sensitive Tokio
//! APIs so no other crate has to spell them.

pub mod global;
pub mod runtime;
