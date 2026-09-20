//! Loopback servers the bridge calls back into.
//!
//! These invert the connection direction. The SDK runs a small Connect server
//! on `127.0.0.1`, tells the bridge its URL plus a bearer token *this* process
//! chose, and the bridge authenticates to us with that token on every
//! callback — which we validate exactly as the bridge validates ours.
//!
//! Two services live here:
//!
//! * [`tools`] — `SdkCustomToolCallbackService`, so an agent can call a Rust
//!   function.
//! * [`store`] — `SdkStoreCallbackService`, so durable local agent state can
//!   live wherever the host wants.

pub mod store;
pub mod tools;

pub(crate) mod server;

pub use server::CallbackServer;

/// A boxed future, so trait objects can have async methods without a
/// procedural macro.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The error type user-supplied handlers return.
///
/// Boxed so `?` works on anything that implements [`std::error::Error`].
pub type HandlerError = Box<dyn std::error::Error + Send + Sync>;

/// Result alias for user-supplied callback handlers.
pub type HandlerResult<T> = std::result::Result<T, HandlerError>;
