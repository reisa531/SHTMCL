//! Public Modrinth metadata. No accounts, game files or installer writes here.
use serde_json::Value;
use shtmcl_domain::{ErrorCategory, ErrorCode, ErrorEnvelope};
use std::{
    collections::VecDeque,
    io::Read,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub(crate) const API: &str = "https://api.modrinth.com/v2/";
const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
const CACHE_LIMIT: usize = 16 * 1024 * 1024;
const CACHE_ENTRIES: usize = 24;

pub(crate) fn error(code: &'static str) -> ErrorEnvelope {
    ErrorEnvelope::new(ErrorCode::from_static(code), ErrorCategory::Network, true)
}
pub(crate) fn invalid() -> ErrorEnvelope {
    ErrorEnvelope::new(
        ErrorCode::from_static("content.modrinth.invalid_metadata"),
        ErrorCategory::InvalidRequest,
        false,
    )
}

pub(crate) trait MetadataHttp: Send + Sync {
    fn get(&self, url: &reqwest::Url) -> Result<Vec<u8>, ErrorEnvelope>;
}

struct PublicHttp;
impl MetadataHttp for PublicHttp {
    fn get(&self, url: &reqwest::Url) -> Result<Vec<u8>, ErrorEnvelope> {
        static CLIENT: OnceLock<Result<reqwest::blocking::Client, reqwest::Error>> =
            OnceLock::new();
        let client = CLIENT
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(concat!(
                        "SHTMCL/",
                        env!("CARGO_PKG_VERSION"),
                        " (desktop content catalog)"
                    ))
                    .connect_timeout(Duration::from_secs(8))
                    .timeout(Duration::from_secs(20))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
            })
            .as_ref()
            .map_err(|_| error("content.modrinth.unavailable"))?;
        let response = client
            .get(url.clone())
            .send()
            .map_err(|_| error("content.modrinth.unavailable"))?;
        match response.status().as_u16() {
            200 => (),
            404 => return Err(error("content.modrinth.not_found")),
            429 => return Err(error("content.modrinth.rate_limited")),
            _ => return Err(error("content.modrinth.unavailable")),
        }
        if response
            .content_length()
            .is_some_and(|n| n > RESPONSE_LIMIT as u64)
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        response
            .take(RESPONSE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| error("content.modrinth.unavailable"))?;
        if bytes.len() > RESPONSE_LIMIT {
            return Err(invalid());
        }
        Ok(bytes)
    }
}

struct CachedResponse {
    url: String,
    at: Instant,
    bytes: Vec<u8>,
}
struct State {
    cache: VecDeque<CachedResponse>,
    active: usize,
}

pub(crate) struct Modrinth {
    http: Arc<dyn MetadataHttp>,
    state: Mutex<State>,
}
impl Modrinth {
    pub(crate) fn new() -> Self {
        Self::with_http(Arc::new(PublicHttp))
    }
    pub(crate) fn with_http(http: Arc<dyn MetadataHttp>) -> Self {
        Self {
            http,
            state: Mutex::new(State {
                cache: VecDeque::new(),
                active: 0,
            }),
        }
    }
    pub(crate) fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ErrorEnvelope> {
        let mut url = reqwest::Url::parse(API)
            .unwrap()
            .join(path)
            .map_err(|_| invalid())?;
        // All callers construct paths from validated identifiers. Also constrain
        // the transport itself so a future caller cannot forward arbitrary URLs.
        if url.origin() != reqwest::Url::parse(API).unwrap().origin()
            || !url.path().starts_with("/v2/")
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return Err(invalid());
        }
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
        }
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| ErrorEnvelope::internal_unavailable())?;
            state
                .cache
                .retain(|entry| entry.at.elapsed() < Duration::from_secs(120));
            if let Some(entry) = state.cache.iter().find(|entry| entry.url == url.as_str()) {
                return serde_json::from_slice(&entry.bytes).map_err(|_| invalid());
            }
            if state.active >= 4 {
                return Err(error("content.modrinth.busy"));
            }
            state.active += 1;
        }
        // Do not hold the cache lock across network IO. Failed responses are not cached.
        let result = self.http.get(&url).and_then(|bytes| {
            if bytes.len() > RESPONSE_LIMIT {
                return Err(invalid());
            }
            let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| invalid())?;
            Ok((bytes, value))
        });
        let mut state = self
            .state
            .lock()
            .map_err(|_| ErrorEnvelope::internal_unavailable())?;
        state.active -= 1;
        let (bytes, value) = result?;
        state.cache.retain(|entry| entry.url != url.as_str());
        while state.cache.len() >= CACHE_ENTRIES
            || state.cache.iter().map(|e| e.bytes.len()).sum::<usize>() + bytes.len() > CACHE_LIMIT
        {
            if state.cache.pop_front().is_none() {
                break;
            }
        }
        state.cache.push_back(CachedResponse {
            url: url.to_string(),
            at: Instant::now(),
            bytes,
        });
        Ok(value)
    }
}

pub(crate) fn identifier(value: &str) -> Result<&str, ErrorEnvelope> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(invalid());
    }
    Ok(value)
}

pub(crate) fn slug(value: &str) -> Result<&str, ErrorEnvelope> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    {
        return Err(invalid());
    }
    Ok(value)
}

#[cfg(test)]
#[path = "modrinth_tests.rs"]
mod tests;
