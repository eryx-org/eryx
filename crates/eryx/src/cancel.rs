//! Cancellation tokens for executions and session state operations.

/// A token for cancelling an execution or session state operation.
///
/// Pass one to an execution's `with_cancellation` and call
/// [`cancel`](Self::cancel) from anywhere to stop the run. Clones share
/// state: cancelling one cancels them all, along with every token made by
/// [`child_token`](Self::child_token).
///
/// This wraps `tokio_util::sync::CancellationToken` so that eryx's public API
/// doesn't depend on tokio-util's version.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(tokio_util::sync::CancellationToken);

impl CancellationToken {
    /// Create a token that isn't cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel this token, its clones, and its children. Calling it again does
    /// nothing.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Whether this token has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }

    /// Wait until this token is cancelled.
    pub async fn cancelled(&self) {
        self.0.cancelled().await;
    }

    /// Create a token that is cancelled when this one is, but whose own
    /// cancellation doesn't affect this one.
    #[must_use]
    pub fn child_token(&self) -> Self {
        Self(self.0.child_token())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_a_parent_cancels_its_children_but_not_vice_versa() {
        let parent = CancellationToken::new();
        let child = parent.child_token();

        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());

        let other = parent.child_token();
        parent.clone().cancel();
        assert!(parent.is_cancelled());
        assert!(other.is_cancelled());
        other.cancelled().await;
    }
}
