use std::time::Instant;

use futures::task;

use reactor::{Message, Handle};

/// A token that identifies an active timeout.
pub struct TimeoutToken {
    token: usize,
}

impl TimeoutToken {
    /// Adds a new timeout to get fired at the specified instant, notifying the
    /// specified task.
    pub fn new(at: Instant, handle: &Handle) -> Option<TimeoutToken> {
        handle.inner().map(|inner| {
            let token = inner.add_timeout();
            handle.send(Message::ResetTimeout(token, at));
            TimeoutToken { token: token }
        })
    }

    /// Updates a previously added timeout to notify a new task instead.
    ///
    /// # Panics
    ///
    /// This method will panic if the timeout specified was not created by this
    /// loop handle's `add_timeout` method.
    pub fn update_timeout(&self, handle: &Handle) {
        handle.send(Message::UpdateTimeout(self.token, task::current()))
    }

    /// Resets previously added (or fired) timeout to an new timeout
    ///
    /// # Panics
    ///
    /// This method will panic if the timeout specified was not created by this
    /// loop handle's `add_timeout` method.
    pub fn reset_timeout(&self, at: Instant, handle: &Handle) {
        handle.send(Message::ResetTimeout(self.token, at));
    }

    /// Cancel a previously added timeout.
    ///
    /// # Panics
    ///
    /// This method will panic if the timeout specified was not created by this
    /// loop handle's `add_timeout` method.
    pub fn cancel_timeout(&self, handle: &Handle) {
        debug!("cancel timeout {}", self.token);
        handle.send(Message::CancelTimeout(self.token))
    }
}
