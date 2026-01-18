use iroh::EndpointId;
use tokio::sync::oneshot;

use crate::ClaimPin;

/// Error type for claim operations.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    /// The machine is already claimed by another manager.
    #[error("Machine is already claimed")]
    AlreadyClaimed,

    /// Failed to send response.
    #[error("Failed to send response")]
    SendFailed,
}

/// A claim request received by a machine.
///
/// This type encapsulates the details of an incoming claim request
/// and provides methods to accept or reject it.
#[derive(Debug)]
pub struct ClaimRequest {
    /// The PIN that should be verified by the user before accepting.
    pin: ClaimPin,

    /// Internal sender for accepting/rejecting the claim.
    /// This MUST always be `Some` unless the claim request is immediately dropped.
    /// The reason this is an `Option` is to allow for calling `Option::take` to
    /// immediately respond to the `Sender` and drop the claim request.
    responder: Option<oneshot::Sender<bool>>,
}

impl ClaimRequest {
    /// Creates a new claim request.
    pub(crate) fn new(pin: ClaimPin, responder: oneshot::Sender<bool>) -> Self {
        Self {
            pin,
            responder: Some(responder),
        }
    }

    /// Gets the PIN for this claim request.
    ///
    /// This PIN should be displayed to the user and verified against
    /// the PIN shown on the manager device before accepting the claim.
    pub fn pin(&self) -> ClaimPin {
        self.pin
    }

    /// Accepts the claim request.
    ///
    /// Returns an error if the claim was already responded to.
    pub fn accept(mut self) -> Result<(), ClaimError> {
        self.responder
            .take()
            .expect("Responder must be `Some`")
            .send(true)
            .map_err(|_| ClaimError::SendFailed)
    }

    /// Rejects the claim request.
    ///
    /// Returns an error if the claim was already responded to.
    pub fn reject(mut self) -> Result<(), ClaimError> {
        self.responder
            .take()
            .expect("Responder must be `Some`")
            .send(false)
            .map_err(|_| ClaimError::SendFailed)
    }
}

impl Drop for ClaimRequest {
    fn drop(&mut self) {
        // If the request is dropped without responding, reject it.
        if let Some(responder) = self.responder.take() {
            let _ = responder.send(false);
        }
    }
}

/// A claim attempt initiated by a manager.
///
/// This type encapsulates the details of an outgoing claim attempt
/// and provides methods to accept, reject, or await the result.
#[derive(Debug)]
pub struct ClaimAttempt {
    /// The machine ID being claimed.
    machine_id: EndpointId,

    /// The PIN that should be verified by the user before accepting.
    pin: ClaimPin,

    /// Internal sender for accepting/rejecting the claim.
    /// This MUST always be `Some` unless the claim request is immediately dropped.
    /// The reason this is an `Option` is to allow for calling `Option::take` to
    /// immediately respond to the `Sender` and drop the claim request.
    responder: Option<oneshot::Sender<bool>>,
}

impl ClaimAttempt {
    /// Creates a new claim attempt.
    pub(crate) fn new(
        machine_id: EndpointId,
        pin: ClaimPin,
        responder: oneshot::Sender<bool>,
    ) -> Self {
        Self {
            machine_id,
            pin,
            responder: Some(responder),
        }
    }

    /// Gets the machine ID for this claim attempt.
    pub fn machine_id(&self) -> &EndpointId {
        &self.machine_id
    }

    /// Gets the PIN for this claim attempt.
    ///
    /// This PIN should be displayed to the user and verified against
    /// the PIN shown on the machine device before accepting the claim.
    pub fn pin(&self) -> ClaimPin {
        self.pin
    }

    /// Accepts the claim attempt.
    ///
    /// Returns an error if the claim was already responded to.
    pub fn accept(mut self) -> Result<(), ClaimError> {
        self.responder
            .take()
            .expect("Responder must be `Some`")
            .send(true)
            .map_err(|_| ClaimError::SendFailed)
    }

    /// Rejects the claim attempt.
    ///
    /// Returns an error if the claim was already responded to.
    pub fn reject(mut self) -> Result<(), ClaimError> {
        self.responder
            .take()
            .expect("Responder must be `Some`")
            .send(false)
            .map_err(|_| ClaimError::SendFailed)
    }
}

impl Drop for ClaimAttempt {
    fn drop(&mut self) {
        // If the attempt is dropped without responding, reject it.
        if let Some(responder) = self.responder.take() {
            let _ = responder.send(false);
        }
    }
}
