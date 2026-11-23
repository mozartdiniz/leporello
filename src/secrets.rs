//! iCloud web session in the system keyring (Secret Service on Linux,
//! Keychain on macOS). The entry is shared with amadeuz (iCloud Notes), so
//! signing in to either app signs in both. Keyring calls block, so they run
//! on tokio's blocking pool.
//!
//! The Apple ID password is kept next to it (also shared), so an expired
//! session can be renewed in the background without a 2FA code while Apple
//! trusts this computer. Signing out deletes both.

use keyring::Entry;

const SERVICE: &str = "io.github.mozartdiniz.iCloud";
const USER: &str = "web-session";
const PASSWORD_USER: &str = "apple-id-password";
/// Where this app kept its own session before the entry was shared.
const LEGACY_SERVICE: &str = crate::config::APP_ID;
const LEGACY_USER: &str = "icloud-session";

pub async fn load_session() -> Option<String> {
    if let Some(s) = read(SERVICE, USER).await {
        return Some(s);
    }
    let s = read(LEGACY_SERVICE, LEGACY_USER).await?;
    if write(s.clone()).await {
        let _ = blocking(LEGACY_SERVICE, LEGACY_USER, |e| e.delete_credential()).await;
    }
    Some(s)
}

pub async fn save_session(session: String) {
    write(session).await;
}

pub async fn load_password() -> Option<String> {
    read(SERVICE, PASSWORD_USER).await
}

pub async fn save_password(password: String) {
    if let Some(Err(err)) = blocking(SERVICE, PASSWORD_USER, move |e| e.set_password(&password)).await {
        log::warn!("keyring write failed: {err}");
    }
}

/// Apple refused it: forget it, so background renewals don't keep trying it.
pub async fn delete_password() {
    let _ = blocking(SERVICE, PASSWORD_USER, |e| e.delete_credential()).await;
}

/// Signs out every app that shares the entry.
pub async fn delete_session() {
    let _ = blocking(SERVICE, USER, |e| e.delete_credential()).await;
    delete_password().await;
    let _ = blocking(LEGACY_SERVICE, LEGACY_USER, |e| e.delete_credential()).await;
}

async fn read(service: &'static str, user: &'static str) -> Option<String> {
    blocking(service, user, |e| match e.get_password() {
        Ok(t) => Some(t),
        Err(keyring::Error::NoEntry) => None,
        Err(err) => {
            log::warn!("keyring read failed: {err}");
            None
        }
    })
    .await
    .flatten()
}

async fn write(session: String) -> bool {
    match blocking(SERVICE, USER, move |e| e.set_password(&session)).await {
        Some(Ok(())) => true,
        Some(Err(err)) => {
            log::warn!("keyring write failed: {err}");
            false
        }
        None => false,
    }
}

async fn blocking<T: Send + 'static>(
    service: &'static str,
    user: &'static str,
    f: impl FnOnce(&Entry) -> T + Send + 'static,
) -> Option<T> {
    crate::bg(async move {
        tokio::task::spawn_blocking(move || Entry::new(service, user).ok().map(|e| f(&e))).await.ok().flatten()
    })
    .await
}
