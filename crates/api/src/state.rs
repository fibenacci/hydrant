//! What the handlers need.

use std::sync::Arc;

use hydrant_core::SchemaSet;
use hydrant_store::ByteBudget;

/// Shared state behind the routes.
///
/// The schema set is what makes a collection exist: a name that is not declared is a 404, not an
/// empty page. Cache directives come from the collection's own definition, so an operator can say
/// per collection how long a shared cache may keep it.
#[derive(Debug)]
pub struct ApiState<S> {
    /// The store to read from.
    pub store: Arc<S>,
    /// Every collection the service serves.
    pub schemas: Arc<SchemaSet>,
    /// How many payload bytes one page may return.
    ///
    /// The record count alone does not bound a response: a full page of large records is large. A
    /// page that runs out of budget ends early and offers a cursor, so nothing becomes unreachable.
    pub response_bytes: ByteBudget,
}

impl<S> ApiState<S> {
    /// Wraps a store and the collections it serves.
    #[must_use]
    pub fn new(store: S, schemas: SchemaSet) -> Self {
        Self {
            store: Arc::new(store),
            schemas: Arc::new(schemas),
            response_bytes: ByteBudget::default(),
        }
    }

    /// Sets how many payload bytes a page may return.
    #[must_use]
    pub const fn with_response_bytes(mut self, bytes: ByteBudget) -> Self {
        self.response_bytes = bytes;
        self
    }
}

// Derived `Clone` would demand `S: Clone`, which the store does not need to be: axum only ever
// clones the state, and an `Arc` is enough for that.
impl<S> Clone for ApiState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            schemas: Arc::clone(&self.schemas),
            response_bytes: self.response_bytes,
        }
    }
}
