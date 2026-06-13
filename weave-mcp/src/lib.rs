#![recursion_limit = "256"]
#[cfg(feature = "surfaces")]
pub mod dashboard;
pub mod http;
pub mod mcp;

#[cfg(feature = "surfaces")]
pub use http::serve_dashboard;
pub use http::serve_http;
pub use mcp::{serve, PullConsent};
