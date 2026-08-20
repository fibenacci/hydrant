//! What the handlers need.

use std::sync::Arc;

/// Shared state behind the public routes.
///
/// `shared_max_age` is a single value for now. Per-collection cache directives are declared in the
/// collection schema, and start being honoured once schema loading exists.
#[derive(Debug)]
pub struct ApiState<S> {
    /// The store to read from.
    pub store: Arc<S>,
    /// `s-maxage` in seconds, sent on every cacheable response.
    pub shared_max_age: u32,
}

impl<S> ApiState<S> {
    /// Wraps a store.
    #[must_use]
    pub fn new(store: S, shared_max_age: u32) -> Self {
        Self {
            store: Arc::new(store),
            shared_max_age,
        }
    }
}

// Derived `Clone` would demand `S: Clone`, which the store does not need to be: axum only ever
// clones the state, and an `Arc` is enough for that.
impl<S> Clone for ApiState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            shared_max_age: self.shared_max_age,
        }
    }
}
