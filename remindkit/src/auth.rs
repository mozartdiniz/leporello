//! Apple ID sign-in for iCloud web services.
//!
//! Flow (same as icloud.com, see pyicloud for the reference implementation):
//! 1. `idmsa.apple.com/appleauth/auth/signin/init` + `signin/complete` (SRP-6a)
//! 2. if Apple answers 409, a 2FA code is pushed to trusted devices:
//!    `verify/trusteddevice/securitycode`, then `2sv/trust`
//! 3. `setup.icloud.com/setup/ws/1/accountLogin` → cookies + service URLs
//!
//! The whole session (headers state + cookie jar) serialises to JSON so the
//! caller can keep it in the platform keyring and skip 2FA on later launches.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Response, StatusCode};
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::srp::{derive_password_key, SrpClient};
use crate::{Error, Result};

const AUTH_ENDPOINT: &str = "https://idmsa.apple.com/appleauth/auth";
const SETUP_ENDPOINT: &str = "https://setup.icloud.com/setup/ws/1";
const WIDGET_KEY: &str = "d39ba9916b7251055b22c7f910e2ea796ee65e98b2ddecea8f5dde8d9d1a815d";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15";
pub(crate) const CLIENT_BUILD: &str = "2420Hotfix12";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInOutcome {
    SignedIn,
    /// A code was pushed to the user's trusted Apple devices; call `submit_code`.
    /// `phones` can receive the code by SMS instead (`request_sms`).
    NeedsCode { phones: Vec<TrustedPhone> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPhone {
    pub id: u64,
    /// Masked by Apple, e.g. "+55 •• •••••-••12".
    pub number: String,
}

/// Where the pending 2FA code was sent.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
enum CodeTarget {
    #[default]
    TrustedDevice,
    Sms(u64),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionState {
    client_id: String,
    apple_id: Option<String>,
    scnt: Option<String>,
    session_id: Option<String>,
    session_token: Option<String>,
    trust_token: Option<String>,
    account_country: Option<String>,
    dsid: Option<String>,
    ckdatabasews_url: Option<String>,
    #[serde(default)]
    code_target: CodeTarget,
    /// This installation's CRDT replica UUID: the "device" amadeuz edits come from.
    #[serde(default)]
    replica_uuid: Option<uuid::Uuid>,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    state: SessionState,
    cookies: String,
}

#[derive(Clone)]
pub struct ICloudClient {
    pub(crate) http: reqwest::Client,
    cookies: Arc<CookieStoreMutex>,
    state: SessionState,
}

impl ICloudClient {
    pub fn new() -> Self {
        Self::with_parts(SessionState::default(), CookieStore::default())
    }

    /// Restore a session produced by [`ICloudClient::save`].
    pub fn restore(saved: &str) -> Result<Self> {
        let saved: SavedSession = serde_json::from_str(saved)
            .map_err(|e| Error::Protocol(format!("corrupt saved session: {e}")))?;
        let jar = cookie_store::serde::json::load_all(saved.cookies.as_bytes())
            .map_err(|e| Error::Protocol(format!("corrupt saved cookies: {e}")))?;
        Ok(Self::with_parts(saved.state, jar))
    }

    fn with_parts(mut state: SessionState, jar: CookieStore) -> Self {
        if state.client_id.is_empty() {
            state.client_id = format!("auth-{}", uuid::Uuid::new_v4());
        }
        state.replica_uuid.get_or_insert_with(uuid::Uuid::new_v4);
        let cookies = Arc::new(CookieStoreMutex::new(jar));
        let mut headers = HeaderMap::new();
        headers.insert("Origin", HeaderValue::from_static("https://www.icloud.com"));
        headers.insert("Referer", HeaderValue::from_static("https://www.icloud.com/"));
        let http = reqwest::Client::builder()
            .cookie_provider(cookies.clone())
            .default_headers(headers)
            .user_agent(USER_AGENT)
            .build()
            .expect("reqwest client");
        Self { http, cookies, state }
    }

    /// Serialise the session (tokens + cookies) for secure storage.
    /// Contains credentials-equivalent material: store it in the keyring only.
    pub fn save(&self) -> String {
        let mut buf = Vec::new();
        {
            let jar = self.cookies.lock().expect("cookie jar lock");
            cookie_store::serde::json::save_incl_expired_and_nonpersistent(&jar, &mut buf)
                .expect("serialise cookies");
        }
        serde_json::to_string(&SavedSession {
            state: self.state.clone(),
            cookies: String::from_utf8(buf).expect("cookie json is utf-8"),
        })
        .expect("serialise session")
    }

    pub fn apple_id(&self) -> Option<&str> {
        self.state.apple_id.as_deref()
    }

    /// Stable per installation (persisted with the session).
    pub fn replica_uuid(&self) -> uuid::Uuid {
        self.state.replica_uuid.expect("set in with_parts")
    }

    pub fn is_signed_in(&self) -> bool {
        self.state.ckdatabasews_url.is_some()
    }

    pub(crate) fn ckdatabasews_url(&self) -> Result<&str> {
        self.state.ckdatabasews_url.as_deref().ok_or(Error::NotSignedIn)
    }

    pub(crate) fn client_params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("clientBuildNumber", CLIENT_BUILD.to_string()),
            ("clientMasteringNumber", CLIENT_BUILD.to_string()),
            ("clientId", self.state.client_id.clone()),
        ];
        if let Some(dsid) = &self.state.dsid {
            params.push(("dsid", dsid.clone()));
        }
        params
    }

    // ── Sign-in ───────────────────────────────────────────────────────────────

    pub async fn sign_in(&mut self, apple_id: &str, password: &str) -> Result<SignInOutcome> {
        self.state.apple_id = Some(apple_id.to_string());
        let srp = SrpClient::new();

        let resp = self
            .http
            .post(format!("{AUTH_ENDPOINT}/signin/init"))
            .headers(self.auth_headers())
            .json(&json!({
                "a": B64.encode(srp.public_a()),
                "accountName": apple_id,
                "protocols": ["s2k", "s2k_fo"],
            }))
            .send()
            .await?;
        self.capture_headers(&resp);
        if resp.status() == StatusCode::UNAUTHORIZED || resp.status() == StatusCode::FORBIDDEN {
            return Err(Error::BadCredentials);
        }
        let init: Value = expect_json(resp).await?;

        let salt = b64_field(&init, "salt")?;
        let b = b64_field(&init, "b")?;
        let c = str_field(&init, "c")?;
        let protocol = str_field(&init, "protocol")?;
        let iterations = init["iteration"]
            .as_u64()
            .ok_or_else(|| Error::Protocol("signin/init: missing iteration".into()))?;

        let key = derive_password_key(password, &salt, iterations as u32, &protocol);
        let proof = srp
            .process_challenge(apple_id, &key, &salt, &b)
            .ok_or_else(|| Error::Protocol("signin/init: invalid SRP challenge".into()))?;

        let trust_tokens: Vec<String> = self.state.trust_token.iter().cloned().collect();
        let resp = self
            .http
            .post(format!("{AUTH_ENDPOINT}/signin/complete"))
            .query(&[("isRememberMeEnabled", "true")])
            .headers(self.auth_headers())
            .json(&json!({
                "accountName": apple_id,
                "c": c,
                "m1": B64.encode(&proof.m1),
                "m2": B64.encode(&proof.m2),
                "rememberMe": true,
                "trustTokens": trust_tokens,
            }))
            .send()
            .await?;
        self.capture_headers(&resp);

        match resp.status() {
            StatusCode::OK => {
                if self.account_login().await? {
                    self.start_two_factor().await
                } else {
                    Ok(SignInOutcome::SignedIn)
                }
            }
            // 409: two-factor challenge.
            StatusCode::CONFLICT => self.start_two_factor().await,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::BadCredentials),
            StatusCode::PRECONDITION_FAILED => Err(Error::AccountActionRequired(
                "sign in once at icloud.com to accept Apple's terms".into(),
            )),
            s => Err(Error::Protocol(format!("signin/complete returned {s}"))),
        }
    }

    /// Load the account's 2FA options, as icloud.com does after a 409.
    /// This request is what makes Apple push the code to trusted devices.
    async fn start_two_factor(&mut self) -> Result<SignInOutcome> {
        self.state.code_target = CodeTarget::TrustedDevice;
        let resp = self
            .http
            .get(AUTH_ENDPOINT)
            .headers(self.auth_headers())
            .send()
            .await?;
        self.capture_headers(&resp);
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(Error::Protocol(format!("2FA options returned {status}")));
        }
        // Lenient: the code may still have been pushed even if this isn't JSON.
        let data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let phones = data["trustedPhoneNumbers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some(TrustedPhone {
                    id: p["id"].as_u64()?,
                    number: p["numberWithDialCode"]
                        .as_str()
                        .or(p["obfuscatedNumber"].as_str())
                        .unwrap_or("?")
                        .to_string(),
                })
            })
            .collect();
        Ok(SignInOutcome::NeedsCode { phones })
    }

    /// Send the code by SMS to one of the trusted phone numbers instead.
    pub async fn request_sms(&mut self, phone_id: u64) -> Result<()> {
        let resp = self
            .http
            .put(format!("{AUTH_ENDPOINT}/verify/phone"))
            .headers(self.auth_headers())
            .json(&json!({ "phoneNumber": { "id": phone_id }, "mode": "sms" }))
            .send()
            .await?;
        self.capture_headers(&resp);
        if !resp.status().is_success() {
            return Err(Error::Protocol(format!("SMS request returned {}", resp.status())));
        }
        self.state.code_target = CodeTarget::Sms(phone_id);
        Ok(())
    }

    /// Submit the 6-digit code from a trusted device, or from the SMS if
    /// `request_sms` was called.
    pub async fn submit_code(&mut self, code: &str) -> Result<()> {
        let code = code.trim();
        let request = match self.state.code_target {
            CodeTarget::TrustedDevice => self
                .http
                .post(format!("{AUTH_ENDPOINT}/verify/trusteddevice/securitycode"))
                .json(&json!({ "securityCode": { "code": code } })),
            CodeTarget::Sms(id) => self
                .http
                .post(format!("{AUTH_ENDPOINT}/verify/phone/securitycode"))
                .json(&json!({
                    "phoneNumber": { "id": id },
                    "securityCode": { "code": code },
                    "mode": "sms",
                })),
        };
        let resp = request.headers(self.auth_headers()).send().await?;
        self.capture_headers(&resp);
        match resp.status() {
            s if s.is_success() => {}
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED => return Err(Error::BadCode),
            s => return Err(Error::Protocol(format!("securitycode returned {s}"))),
        }

        // Mark this client as trusted so later sign-ins skip 2FA.
        let resp = self
            .http
            .get(format!("{AUTH_ENDPOINT}/2sv/trust"))
            .headers(self.auth_headers())
            .send()
            .await?;
        self.capture_headers(&resp);
        if !resp.status().is_success() {
            return Err(Error::Protocol(format!("2sv/trust returned {}", resp.status())));
        }

        if self.account_login().await? {
            return Err(Error::Protocol("still asking for 2FA after a valid code".into()));
        }
        Ok(())
    }

    /// Ask Apple to push a fresh code to trusted devices.
    pub async fn resend_code(&mut self) -> Result<()> {
        self.state.code_target = CodeTarget::TrustedDevice;
        let resp = self
            .http
            .put(format!("{AUTH_ENDPOINT}/verify/trusteddevice/securitycode"))
            .headers(self.auth_headers())
            .send()
            .await?;
        self.capture_headers(&resp);
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(Error::Protocol(format!("resend code returned {}", resp.status())))
        }
    }

    /// Check that saved cookies are still accepted and refresh service URLs.
    pub async fn validate(&mut self) -> Result<()> {
        let resp = self
            .http
            .post(format!("{SETUP_ENDPOINT}/validate"))
            .query(&self.client_params())
            .body("null")
            .send()
            .await?;
        if matches!(resp.status().as_u16(), 401 | 403 | 421) {
            self.state.ckdatabasews_url = None;
            return Err(Error::SessionExpired);
        }
        let data: Value = expect_json(resp).await?;
        self.apply_account_data(&data)
    }

    pub fn sign_out(&mut self) {
        let replica = self.state.replica_uuid;
        *self = Self::new();
        // Keep the replica identity: it's this installation, not the account.
        self.state.replica_uuid = replica;
    }

    /// Returns true if Apple still requires a 2FA code.
    async fn account_login(&mut self) -> Result<bool> {
        let token = self
            .state
            .session_token
            .clone()
            .ok_or_else(|| Error::Protocol("no session token after sign-in".into()))?;
        let resp = self
            .http
            .post(format!("{SETUP_ENDPOINT}/accountLogin"))
            .query(&self.client_params())
            .json(&json!({
                "accountCountryCode": self.state.account_country.clone().unwrap_or_default(),
                "dsWebAuthToken": token,
                "extended_login": true,
                "trustToken": self.state.trust_token.clone().unwrap_or_default(),
            }))
            .send()
            .await?;
        if resp.status().as_u16() == 421 {
            return Err(Error::SessionExpired);
        }
        let data: Value = expect_json(resp).await?;
        self.apply_account_data(&data)?;
        let challenge = data["hsaChallengeRequired"].as_bool().unwrap_or(false);
        let trusted = data["hsaTrustedBrowser"].as_bool().unwrap_or(true);
        Ok(challenge && !trusted)
    }

    fn apply_account_data(&mut self, data: &Value) -> Result<()> {
        let dsid = match &data["dsInfo"]["dsid"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Err(Error::Protocol("account data: missing dsid".into())),
        };
        self.state.dsid = Some(dsid);
        match data["webservices"]["ckdatabasews"]["url"].as_str() {
            Some(url) => self.state.ckdatabasews_url = Some(url.trim_end_matches('/').to_string()),
            None => {
                // Typical for Advanced Data Protection accounts without web access.
                return Err(Error::AccountActionRequired(
                    "iCloud web access is disabled for this account. If Advanced Data \
                     Protection is on, enable \"Access iCloud Data on the Web\" in \
                     iCloud settings on your iPhone or Mac"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut h = HeaderMap::new();
        let mut set = |k: &'static str, v: &str| {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k, v);
            }
        };
        set("Accept", "application/json, text/javascript");
        set("Content-Type", "application/json");
        set("X-Apple-OAuth-Client-Id", WIDGET_KEY);
        set("X-Apple-OAuth-Client-Type", "firstPartyAuth");
        set("X-Apple-OAuth-Redirect-URI", "https://www.icloud.com");
        set("X-Apple-OAuth-Require-Grant-Code", "true");
        set("X-Apple-OAuth-Response-Mode", "web_message");
        set("X-Apple-OAuth-Response-Type", "code");
        set("X-Apple-OAuth-State", &self.state.client_id);
        set("X-Apple-Widget-Key", WIDGET_KEY);
        if let Some(v) = &self.state.scnt {
            set("scnt", v);
        }
        if let Some(v) = &self.state.session_id {
            set("X-Apple-ID-Session-Id", v);
        }
        h
    }

    fn capture_headers(&mut self, resp: &Response) {
        let get = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        if let Some(v) = get("X-Apple-ID-Account-Country") {
            self.state.account_country = Some(v);
        }
        if let Some(v) = get("X-Apple-ID-Session-Id") {
            self.state.session_id = Some(v);
        }
        if let Some(v) = get("X-Apple-Session-Token") {
            self.state.session_token = Some(v);
        }
        if let Some(v) = get("X-Apple-TwoSV-Trust-Token") {
            self.state.trust_token = Some(v);
        }
        if let Some(v) = get("scnt") {
            self.state.scnt = Some(v);
        }
    }
}

impl Default for ICloudClient {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) async fn expect_json(resp: Response) -> Result<Value> {
    let status = resp.status();
    let url = resp.url().path().to_string();
    let text = resp.text().await?;
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        return Err(Error::Protocol(format!("{url} returned {status}: {snippet}")));
    }
    serde_json::from_str(&text).map_err(|e| Error::Protocol(format!("{url}: invalid JSON: {e}")))
}

fn str_field(v: &Value, key: &str) -> Result<String> {
    v[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Protocol(format!("missing field {key}")))
}

fn b64_field(v: &Value, key: &str) -> Result<Vec<u8>> {
    B64.decode(str_field(v, key)?)
        .map_err(|e| Error::Protocol(format!("field {key} is not base64: {e}")))
}
