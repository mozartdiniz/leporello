//! iCloud web session in the system keyring (Secret Service on Linux,
//! Keychain on macOS). Keyring calls block, so they run on tokio's
//! blocking pool.

use keyring::Entry;

const SERVICE: &str = crate::config::APP_ID;
const USER: &str = "icloud-session";

pub async fn load_session() -> Option<String> {
    blocking(|e| match e.get_password() {
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

pub async fn save_session(session: String) {
    if let Some(Err(err)) = blocking(move |e| e.set_password(&session)).await {
        log::warn!("keyring write failed: {err}");
    }
}

pub async fn delete_session() {
    let _ = blocking(|e| e.delete_credential()).await;
}

async fn blocking<T: Send + 'static>(f: impl FnOnce(&Entry) -> T + Send + 'static) -> Option<T> {
    crate::bg(async move {
        tokio::task::spawn_blocking(move || Entry::new(SERVICE, USER).ok().map(|e| f(&e))).await.ok().flatten()
    })
    .await
}
