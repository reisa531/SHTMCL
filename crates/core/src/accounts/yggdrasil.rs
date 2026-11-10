//! MUA's authlib-injector/Yggdrasil HTTP adapter.
//!
//! Passwords and bearer tokens remain in this module's call stack. Provider
//! response bodies and transport errors are reduced to stable opaque strings
//! before they can reach account events or command errors.

use std::io::Read;

use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use reqwest::Url;
use serde_json::{json, Value};

use crate::accounts::ports::{
    AccountsPorts, LoginChallenge, PollOutcome, YggdrasilAuthError, YggdrasilGrant,
};

pub const MUA_API_ROOT: &str = "https://skin.mualliance.ltd/api/yggdrasil";
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_AUTH_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YggdrasilMetadata {
    pub api_root: String,
    pub server_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YggdrasilError {
    ClientUnavailable,
    InvalidApiRoot,
    MetadataUnavailable,
    MetadataInvalid,
    AuthenticationRejected,
    AuthenticationUnavailable,
    NoSelectedProfile,
}

impl From<YggdrasilError> for YggdrasilAuthError {
    fn from(error: YggdrasilError) -> Self {
        match error {
            YggdrasilError::AuthenticationRejected => Self::Rejected,
            YggdrasilError::NoSelectedProfile => Self::ProfileSelectionRequired,
            YggdrasilError::ClientUnavailable
            | YggdrasilError::InvalidApiRoot
            | YggdrasilError::MetadataUnavailable
            | YggdrasilError::MetadataInvalid
            | YggdrasilError::AuthenticationUnavailable => Self::Unavailable,
        }
    }
}

/// A small blocking adapter intended to run on the account worker, never on a
/// UI thread. Redirects are disabled so credentials cannot be forwarded to a
/// different host. The API root is fixed to the MUA endpoint confirmed for this
/// product.
pub struct MuaYggdrasilClient {
    client: Client,
}

impl MuaYggdrasilClient {
    pub fn new() -> Result<Self, YggdrasilError> {
        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(20))
            .redirect(Policy::none())
            .build()
            .map_err(|_| YggdrasilError::ClientUnavailable)?;
        Ok(Self { client })
    }

    /// Fetch the public API metadata without sending account credentials.
    pub fn metadata(&self) -> Result<YggdrasilMetadata, YggdrasilError> {
        let configured = parse_api_root(MUA_API_ROOT)?;
        let response = self
            .client
            .get(configured.clone())
            .send()
            .map_err(|_| YggdrasilError::MetadataUnavailable)?;
        let status = response.status();
        let api_root = resolve_api_location(&configured, &response)?;
        if !status.is_success() {
            return Err(YggdrasilError::MetadataUnavailable);
        }
        let body = read_bounded(response, MAX_METADATA_BYTES)
            .map_err(|_| YggdrasilError::MetadataInvalid)?;
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| YggdrasilError::MetadataInvalid)?;
        let server_name = value
            .pointer("/meta/serverName")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty() && name.len() <= 128)
            .ok_or(YggdrasilError::MetadataInvalid)?
            .to_owned();

        Ok(YggdrasilMetadata {
            api_root: api_root.to_string(),
            server_name,
        })
    }

    /// Authenticate a single account. This first cut requires the server to
    /// return a selected profile; a multi-profile picker and token refresh are
    /// separate follow-up work so the access token never has to cross the UI.
    pub fn authenticate(
        &self,
        account: &str,
        password: &str,
    ) -> Result<YggdrasilGrant, YggdrasilError> {
        if account.trim().is_empty() || password.is_empty() {
            return Err(YggdrasilError::AuthenticationRejected);
        }
        let metadata = self.metadata()?;
        let api_root = parse_api_root(&metadata.api_root)?;
        let endpoint = api_root
            .join("authserver/authenticate")
            .map_err(|_| YggdrasilError::InvalidApiRoot)?;
        ensure_mua_origin(&endpoint)?;

        let client_token = shtmcl_platform::entropy::random_hex(16)
            .map_err(|_| YggdrasilError::AuthenticationUnavailable)?;
        let body = authentication_request_body(account, password, &client_token)?;
        let response = self
            .client
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .map_err(|_| YggdrasilError::AuthenticationUnavailable)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(YggdrasilError::AuthenticationRejected);
        }
        if !response.status().is_success() {
            return Err(YggdrasilError::AuthenticationUnavailable);
        }
        let body = read_bounded(response, MAX_AUTH_RESPONSE_BYTES)
            .map_err(|_| YggdrasilError::AuthenticationUnavailable)?;
        parse_authentication_response(&body)
    }
}

impl AccountsPorts for MuaYggdrasilClient {
    fn begin_login(&self, provider_id: &str, _now: u64) -> Result<LoginChallenge, String> {
        Err(format!("provider:{provider_id}:unsupported"))
    }

    fn poll_login(
        &self,
        provider_id: &str,
        _identity_id: &str,
        _user_code: &str,
        _now: u64,
    ) -> Result<PollOutcome, String> {
        Err(format!("provider:{provider_id}:unsupported"))
    }

    fn authenticate_yggdrasil(
        &self,
        account: &str,
        password: &str,
    ) -> Result<YggdrasilGrant, YggdrasilAuthError> {
        self.authenticate(account, password).map_err(Into::into)
    }

    fn offer_session_token(&self, handle_id: &str, _token: &str) -> Result<(), String> {
        Err(format!("session:{handle_id}:no_consumer"))
    }

    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}

fn authentication_request_body(
    account: &str,
    password: &str,
    client_token: &str,
) -> Result<Vec<u8>, YggdrasilError> {
    serde_json::to_vec(&json!({
        "agent": { "name": "Minecraft", "version": 1 },
        "username": account,
        "password": password,
        "requestUser": true,
        "clientToken": client_token,
    }))
    .map_err(|_| YggdrasilError::AuthenticationUnavailable)
}

fn parse_authentication_response(bytes: &[u8]) -> Result<YggdrasilGrant, YggdrasilError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| YggdrasilError::AuthenticationUnavailable)?;
    let access_token = value
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty() && token.len() <= 4096)
        .ok_or(YggdrasilError::AuthenticationRejected)?
        .to_owned();
    let profile = value
        .get("selectedProfile")
        .filter(|profile| !profile.is_null())
        .ok_or(YggdrasilError::NoSelectedProfile)?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_profile_id(id))
        .ok_or(YggdrasilError::AuthenticationUnavailable)?
        .to_ascii_lowercase();
    let display_name = profile
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| is_profile_name(name))
        .ok_or(YggdrasilError::AuthenticationUnavailable)?
        .to_owned();

    Ok(YggdrasilGrant {
        access_token,
        display_name,
        account_uuid: profile_id.clone(),
        profile_id,
    })
}

fn parse_api_root(value: &str) -> Result<Url, YggdrasilError> {
    let mut url = Url::parse(value).map_err(|_| YggdrasilError::InvalidApiRoot)?;
    ensure_mua_origin(&url)?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(YggdrasilError::InvalidApiRoot);
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

fn ensure_mua_origin(url: &Url) -> Result<(), YggdrasilError> {
    if url.scheme() != "https"
        || url.host_str() != Some("skin.mualliance.ltd")
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(YggdrasilError::InvalidApiRoot);
    }
    Ok(())
}

fn resolve_api_location(configured: &Url, response: &Response) -> Result<Url, YggdrasilError> {
    let Some(location) = response
        .headers()
        .get("x-authlib-injector-api-location")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(configured.clone());
    };
    let resolved = configured
        .join(location)
        .map_err(|_| YggdrasilError::InvalidApiRoot)?;
    ensure_mua_origin(&resolved)?;
    parse_api_root(resolved.as_str())
}

fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>, std::io::Error> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(std::io::Error::other("response too large"));
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    response
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(std::io::Error::other("response too large"));
    }
    Ok(body)
}

fn is_profile_id(value: &str) -> bool {
    let normalized = value.replace('-', "");
    normalized.len() == 32 && normalized.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_profile_name(value: &str) -> bool {
    (1..=16).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_root_is_normalized_without_changing_the_user_confirmed_path() {
        let url = parse_api_root(MUA_API_ROOT).expect("MUA API root");
        assert_eq!(url.as_str(), "https://skin.mualliance.ltd/api/yggdrasil/");
    }

    #[test]
    fn rejects_http_downgrade_and_foreign_api_location() {
        assert_eq!(
            parse_api_root("http://skin.mualliance.ltd/api/yggdrasil"),
            Err(YggdrasilError::InvalidApiRoot)
        );
        let configured = parse_api_root(MUA_API_ROOT).expect("root");
        let foreign = Url::parse("https://attacker.invalid/api/").expect("url");
        assert_eq!(
            ensure_mua_origin(&foreign),
            Err(YggdrasilError::InvalidApiRoot)
        );
        assert!(configured.as_str().starts_with("https://"));
    }

    #[test]
    fn profile_id_and_name_are_bounded_to_minecraft_shapes() {
        assert!(is_profile_id("00112233445566778899aabbccddeeff"));
        assert!(is_profile_id("00112233-4455-6677-8899-aabbccddeeff"));
        assert!(!is_profile_id("not-a-profile"));
        assert!(is_profile_name("Player_1"));
        assert!(!is_profile_name("invalid name"));
    }

    #[test]
    fn access_tokens_are_redacted_from_grant_debug_output() {
        let grant = YggdrasilGrant {
            access_token: "secret-mua-access-token".into(),
            display_name: "Player".into(),
            account_uuid: "00112233445566778899aabbccddeeff".into(),
            profile_id: "00112233445566778899aabbccddeeff".into(),
        };
        let debug = format!("{grant:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-mua-access-token"));
    }

    #[test]
    fn authentication_request_uses_yggdrasil_shape_without_reusing_password_fields() {
        let body = authentication_request_body("user@example.test", "passphrase", "client-hex")
            .expect("request body");
        let value: Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(value["agent"]["name"], "Minecraft");
        assert_eq!(value["agent"]["version"], 1);
        assert_eq!(value["username"], "user@example.test");
        assert_eq!(value["password"], "passphrase");
        assert_eq!(value["requestUser"], true);
        assert_eq!(value["clientToken"], "client-hex");
    }

    #[test]
    fn authentication_response_requires_a_valid_selected_profile() {
        let grant = parse_authentication_response(
            br#"{"accessToken":"secret-token","selectedProfile":{"id":"00112233445566778899aabbccddeeff","name":"MuaPlayer"}}"#,
        )
        .expect("grant");
        assert_eq!(grant.display_name, "MuaPlayer");
        assert_eq!(grant.profile_id, "00112233445566778899aabbccddeeff");
        assert_eq!(
            parse_authentication_response(
                br#"{"accessToken":"secret-token","selectedProfile":null,"availableProfiles":[{"id":"00112233445566778899aabbccddeeff","name":"MuaPlayer"}]}"#
            ),
            Err(YggdrasilError::NoSelectedProfile)
        );
    }

    #[test]
    #[ignore = "real MUA HTTPS metadata check; no account credentials are used"]
    fn live_mua_metadata_resolves_the_configured_api_root() {
        let client = MuaYggdrasilClient::new().expect("HTTP client");
        let metadata = client.metadata().expect("MUA metadata");
        assert_eq!(metadata.server_name, "MUA 用户中心");
        assert_eq!(
            metadata.api_root,
            "https://skin.mualliance.ltd/api/yggdrasil/"
        );
        println!(
            "mua_metadata server_name={} api_root={}",
            metadata.server_name, metadata.api_root
        );
    }
}
