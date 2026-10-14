//! iCloud Reminders access over CloudKit Web Services.
//!
//! No GTK dependency, so it builds and tests on any platform. Sign-in (`auth`,
//! `srp`) is shared with amadeuz's iCloud Notes client. See
//! `docs/ICLOUD_REMINDERS_PROTOCOL.md` for the protocol research.

pub mod auth;
pub mod cloudkit;
pub mod crdt;
pub mod document;
pub mod model;
mod srp;
pub mod wire;

pub use auth::{ICloudClient, SignInOutcome, TrustedPhone};
pub use model::{DeltaUnit, Draft, Due, EarlyAlert, List, Priority, Reminder, Store, Writer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("incorrect Apple ID or password")]
    BadCredentials,
    #[error("incorrect verification code")]
    BadCode,
    #[error("not signed in to iCloud")]
    NotSignedIn,
    #[error("iCloud session expired")]
    SessionExpired,
    #[error("Apple requires attention on this account: {0}")]
    AccountActionRequired(String),
    #[error("the reminder changed on another device; reload it and try again")]
    Conflict,
    /// Transient server condition; retry (after this many seconds, if given).
    #[error("iCloud is busy; try again in a moment")]
    Retry(Option<f64>),
    #[error("unexpected iCloud response: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, Error>;
