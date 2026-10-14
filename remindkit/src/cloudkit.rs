//! CloudKit Web Services transport for the Reminders container.
//!
//! Container `com.apple.reminders` (Reminders.app entitlements), private
//! database. Record types and fields come from `remindd`; see
//! `docs/ICLOUD_REMINDERS_PROTOCOL.md`. This module only moves CloudKit JSON;
//! `model` turns it into reminders.

use serde_json::{json, Value};

use crate::auth::ICloudClient;
use crate::model::Store;
use crate::{Error, Result};

pub const CONTAINER: &str = "com.apple.reminders";
const ZONE_NAME: &str = "Reminders";
/// Records fetched per `changes/zone` page.
const PAGE_SIZE: u32 = 200;

/// One `changes/zone` pass: changed records, and the token for the next pass.
#[derive(Debug, Default)]
pub struct ZoneChanges {
    pub records: Vec<Value>,
    pub sync_token: Option<String>,
}

impl ICloudClient {
    fn ck_url(&self, path: &str) -> Result<String> {
        Ok(format!(
            "{}/database/1/{CONTAINER}/production/private/{path}",
            self.ckdatabasews_url()?
        ))
    }

    /// Every zone in the private database (`zoneID` objects).
    pub async fn list_zones(&self) -> Result<Vec<Value>> {
        let resp = self.http.get(self.ck_url("zones/list")?).query(&self.client_params()).send().await?;
        let status = resp.status().as_u16();
        if status == 401 || status == 421 {
            return Err(Error::SessionExpired);
        }
        let data = crate::auth::expect_json(resp).await?;
        Ok(data["zones"].as_array().cloned().unwrap_or_default())
    }

    /// Records changed in `zone_id` since `sync_token` (all records when
    /// `None`), following `moreComing` to the end. Deleted records come back
    /// with `"deleted": true` and no fields.
    pub async fn zone_changes(&self, zone_id: &Value, sync_token: Option<&str>) -> Result<ZoneChanges> {
        let mut out = ZoneChanges { records: Vec::new(), sync_token: sync_token.map(str::to_string) };
        loop {
            let mut zone = json!({ "zoneID": zone_id, "resultsLimit": PAGE_SIZE });
            if let Some(t) = &out.sync_token {
                zone["syncToken"] = json!(t);
            }
            let body = json!({ "zones": [zone] });
            let data = retrying(|| self.ck_post("changes/zone", &body)).await?;
            let zone = &data["zones"][0];
            if zone["serverErrorCode"].is_string() {
                return Err(server_error("changes/zone", zone));
            }
            out.records.extend(zone["records"].as_array().into_iter().flatten().cloned());
            let next = zone["syncToken"].as_str().map(str::to_string);
            let more = zone["moreComing"].as_bool().unwrap_or(false);
            if next.is_some() {
                out.sync_token = next;
            }
            if !more || out.sync_token.is_none() {
                return Ok(out);
            }
        }
    }

    /// `records/modify` with the given operations (`create`, `update`,
    /// `forceDelete`, …), atomically within `zone_id`. Returns the saved
    /// records. A per-record error fails the whole call.
    pub async fn modify_records(&self, zone_id: &Value, operations: Vec<Value>) -> Result<Vec<Value>> {
        let body = json!({ "operations": operations, "zoneID": zone_id, "atomic": true });
        let data = retrying(|| self.ck_post("records/modify", &body))
            .await
            .inspect_err(|e| log::warn!("records/modify rejected ({e}); request: {body}"))?;
        let records = data["records"].as_array().cloned().unwrap_or_default();
        let failed: Vec<&Value> = records.iter().filter(|r| r["serverErrorCode"].is_string()).collect();
        if !failed.is_empty() {
            for r in &failed {
                log::warn!("records/modify: {r}");
            }
            log::warn!("records/modify request: {body}");
            // In an atomic batch the other records fail with ATOMIC_FAILURE;
            // report the one that caused it.
            let cause = failed.iter().find(|r| r["serverErrorCode"] != "ATOMIC_FAILURE").unwrap_or(&failed[0]);
            let name = cause["recordName"].as_str().unwrap_or("?");
            return Err(server_error(&format!("records/modify {name}"), cause));
        }
        Ok(records)
    }

    /// The `Reminders` zone ID, for a new [`Store`].
    pub async fn reminders_zone(&self) -> Result<Value> {
        self.list_zones()
            .await?
            .into_iter()
            .find(|z| z["zoneID"]["zoneName"] == ZONE_NAME)
            .map(|z| z["zoneID"].clone())
            .ok_or_else(|| Error::Protocol("no Reminders zone in com.apple.reminders".into()))
    }

    /// Bring `store` up to date (everything on first call, changes after).
    /// An expired sync token starts over from scratch.
    pub async fn sync(&self, store: &mut Store) -> Result<()> {
        match self.zone_changes(&store.zone_id, store.sync_token.as_deref()).await {
            Ok(c) => store.apply(c.records, c.sync_token),
            Err(Error::Protocol(msg)) if store.sync_token.is_some() && msg.contains("SYNC") => {
                log::info!("sync token rejected ({msg}); fetching everything");
                let zone = store.zone_id.clone();
                *store = Store::new(zone);
                let c = self.zone_changes(&store.zone_id, None).await?;
                store.apply(c.records, c.sync_token);
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Send operations built by [`crate::Writer`] and fold the saved records
    /// into `store`.
    pub async fn write(&self, store: &mut Store, operations: Vec<Value>) -> Result<()> {
        let saved = self.modify_records(&store.zone_id, operations).await?;
        store.apply(saved, None);
        Ok(())
    }

    /// Whether the "Urgent" toggle is on, from a reminder's
    /// `UrgentPresentationAlarmsAsData` asset.
    pub async fn fetch_urgent(&self, asset_url: &str) -> Result<bool> {
        let bytes = self.download_asset(asset_url).await?;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| Error::Protocol(format!("urgent asset: {e}")))?;
        Ok(v["account"].as_array().into_iter().flatten().any(|a| a["isEnabled"] == true))
    }

    /// Download a CloudKit asset (large title/notes documents, images).
    pub async fn download_asset(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.http.get(url).send().await?.error_for_status()?.bytes().await?.to_vec())
    }

    /// POST to a CloudKit endpoint. Errors iCloud reports in the body (with
    /// HTTP 4xx/5xx or 200) become typed errors; transient ones become
    /// [`Error::Retry`].
    async fn ck_post(&self, path: &str, body: &Value) -> Result<Value> {
        let resp = self.http.post(self.ck_url(path)?).query(&self.client_params()).json(body).send().await?;
        let status = resp.status().as_u16();
        if status == 401 || status == 421 {
            return Err(Error::SessionExpired);
        }
        let text = resp.text().await?;
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if data["serverErrorCode"].is_string() {
            return Err(server_error(path, &data));
        }
        match status {
            200..=299 if !data.is_null() => Ok(data),
            429 | 503 => Err(Error::Retry(None)),
            _ => {
                let snippet: String = text.chars().take(300).collect();
                Err(Error::Protocol(format!("{path} returned {status}: {snippet}")))
            }
        }
    }
}

/// Map an iCloud error object (`serverErrorCode`, `reason`, `retryAfter`).
fn server_error(what: &str, e: &Value) -> Error {
    let code = e["serverErrorCode"].as_str().unwrap_or("");
    match code {
        // Stale recordChangeTag: someone else saved first.
        "CONFLICT" | "OPLOCK" => Error::Conflict,
        "TRY_AGAIN_LATER" | "ZONE_BUSY" | "THROTTLED" | "SERVICE_UNAVAILABLE" | "RETRY_LATER" => {
            Error::Retry(e["retryAfter"].as_f64())
        }
        "AUTHENTICATION_FAILED" | "AUTHENTICATION_REQUIRED" => Error::SessionExpired,
        _ => Error::Protocol(format!("{what}: {code}: {}", e["reason"].as_str().unwrap_or(""))),
    }
}

/// Run `op`, retrying [`Error::Retry`] with backoff (0.5 s, 1 s, 2 s, 4 s, 8 s)
/// or the server's `retryAfter`.
async fn retrying<T, F, Fut>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    const ATTEMPTS: u32 = 6;
    for attempt in 1.. {
        match op().await {
            Err(Error::Retry(after)) if attempt < ATTEMPTS => {
                let backoff = 0.5 * f64::from(1u32 << (attempt - 1));
                let wait = after.unwrap_or(backoff).clamp(0.2, 30.0);
                log::info!("iCloud busy, retrying in {wait:.1}s (attempt {attempt})");
                tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
            }
            Err(Error::Retry(_)) => return Err(Error::Protocol("iCloud is busy; try again in a moment".into())),
            other => return other,
        }
    }
    unreachable!("loop returns")
}
