//! Returns startup metadata from memory, refreshing upstream in the background.
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
    time::Instant,
};

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Extension, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri},
};
use parking_lot::Mutex;
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{
    api::cursor::proxy::{self, CursorProxy},
    cursor::{
        services::startup_timing::{self, MetadataSource},
        transport::TransportRegistry,
    },
    local_app, Result,
};

#[derive(Clone, Copy, PartialEq, Message)]
struct EmptyResponse {}

#[derive(Clone, Eq, PartialEq, Hash)]
struct CacheKey {
    path: String,
    token: [u8; 32],
}

struct CachedMetadata {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    stored_at: Instant,
}

struct MetadataCache {
    entries: Mutex<HashMap<CacheKey, CachedMetadata>>,
    refreshing: Mutex<HashSet<CacheKey>>,
}

impl MetadataCache {
    fn get(&self, key: &CacheKey) -> Option<CachedMetadata> {
        let entries = self.entries.lock();
        let cached = entries.get(key)?;
        Some(CachedMetadata {
            status: cached.status,
            headers: cached.headers.clone(),
            body: cached.body.clone(),
            stored_at: cached.stored_at,
        })
    }

    fn store(&self, key: CacheKey, status: StatusCode, headers: HeaderMap, body: Bytes) {
        self.entries.lock().insert(
            key,
            CachedMetadata {
                status,
                headers,
                body,
                stored_at: Instant::now(),
            },
        );
    }

    fn begin_refresh(&self, key: CacheKey) -> bool {
        self.refreshing.lock().insert(key)
    }

    fn finish_refresh(&self, key: &CacheKey) {
        self.refreshing.lock().remove(key);
    }
}

fn cache() -> &'static MetadataCache {
    static CACHE: OnceLock<MetadataCache> = OnceLock::new();
    CACHE.get_or_init(|| MetadataCache {
        entries: Mutex::new(HashMap::new()),
        refreshing: Mutex::new(HashSet::new()),
    })
}

pub async fn serve(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let path = request.uri().path().to_owned();
    let enabled = registry.store().cli_startup_local_metadata().await?;
    match serve_mode(request.headers(), enabled) {
        ServeMode::Forward => proxy::forward(Extension(proxy), request).await,
        ServeMode::Empty => {
            consume_body(request).await?;
            startup_timing::log_metadata(
                &path,
                MetadataSource::Local,
                started.elapsed(),
                None,
                Some(false),
                None,
            );
            Ok(empty_proto())
        }
        ServeMode::Cache => serve_cached(proxy, request, &path, started).await,
    }
}

async fn serve_cached(
    proxy: CursorProxy,
    request: Request<Body>,
    path: &str,
    started: Instant,
) -> Result<Response<Body>> {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    let Some(token) = token_hash(&parts.headers) else {
        startup_timing::log_metadata(
            path,
            MetadataSource::Local,
            started.elapsed(),
            None,
            Some(false),
            None,
        );
        return Ok(empty_proto());
    };
    let key = CacheKey {
        path: path.to_owned(),
        token,
    };

    let cached = cache().get(&key);
    let cache_age = cached.as_ref().map(|entry| entry.stored_at.elapsed());
    if cache().begin_refresh(key.clone()) {
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let headers = parts.headers.clone();
        let refresh_path = path.to_owned();
        tokio::spawn(async move {
            refresh(proxy, key, method, uri, headers, body, refresh_path).await;
        });
    }

    let response = match cached {
        Some(cached) => {
            startup_timing::log_metadata(
                path,
                MetadataSource::Cached,
                started.elapsed(),
                None,
                Some(true),
                cache_age,
            );
            cached_response(cached)
        }
        None => {
            startup_timing::log_metadata(
                path,
                MetadataSource::Local,
                started.elapsed(),
                None,
                Some(false),
                None,
            );
            empty_proto()
        }
    };
    Ok(response)
}

async fn refresh(
    proxy: CursorProxy,
    key: CacheKey,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    path: String,
) {
    let started = Instant::now();
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    match proxy::forward_buffered(&proxy, request).await {
        Ok(upstream) if upstream.status.is_success() => {
            let elapsed = started.elapsed();
            cache().store(key.clone(), upstream.status, upstream.headers, upstream.body);
            startup_timing::log_metadata(
                &path,
                MetadataSource::Upstream,
                elapsed,
                Some(elapsed),
                None,
                None,
            );
        }
        Ok(upstream) => {
            tracing::warn!(
                path,
                status = %upstream.status,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "startup metadata refresh rejected"
            );
        }
        Err(error) => {
            tracing::warn!(
                path,
                %error,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "startup metadata refresh failed"
            );
        }
    }
    cache().finish_refresh(&key);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServeMode {
    Forward,
    Empty,
    Cache,
}

fn serve_mode(headers: &HeaderMap, enabled: bool) -> ServeMode {
    if local_app::request_uses_local_cursor_token(headers) {
        ServeMode::Empty
    } else if enabled {
        ServeMode::Cache
    } else {
        ServeMode::Forward
    }
}

fn token_hash(headers: &HeaderMap) -> Option<[u8; 32]> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(Sha256::digest(token.as_bytes()).into())
}

fn cached_response(cached: CachedMetadata) -> Response<Body> {
    let mut headers = cached.headers;
    headers.insert(
        header::CONTENT_LENGTH,
        cached
            .body
            .len()
            .to_string()
            .parse()
            .expect("body length is a valid header"),
    );
    let mut response = Response::new(Body::from(cached.body));
    *response.status_mut() = cached.status;
    *response.headers_mut() = headers;
    response
}

fn empty_proto() -> Response<Body> {
    let body = EmptyResponse {}.encode_to_vec();
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

async fn consume_body(request: Request<Body>) -> Result<()> {
    to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_tokens_use_the_persistent_cache_only_when_enabled() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer official-cursor-token"),
        );
        assert_eq!(serve_mode(&headers, true), ServeMode::Cache);
        assert_eq!(serve_mode(&headers, false), ServeMode::Forward);

        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&local_app::local_cursor_authorization()).unwrap(),
        );
        assert_eq!(serve_mode(&headers, true), ServeMode::Empty);
        assert_eq!(serve_mode(&headers, false), ServeMode::Empty);
    }

    #[test]
    fn cache_is_scoped_to_the_endpoint_and_refreshes_once_at_a_time() {
        let cache = MetadataCache {
            entries: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
        };
        let plugins = CacheKey {
            path: "/GetEffectiveUserPlugins".into(),
            token: [7u8; 32],
        };
        let privacy = CacheKey {
            path: "/GetUserPrivacyMode".into(),
            token: [7u8; 32],
        };
        cache.store(
            plugins.clone(),
            StatusCode::OK,
            HeaderMap::new(),
            Bytes::from_static(b"plugins"),
        );
        assert_eq!(cache.get(&plugins).unwrap().body.as_ref(), b"plugins");
        assert!(cache.get(&privacy).is_none());
        assert!(cache.begin_refresh(plugins.clone()));
        assert!(!cache.begin_refresh(plugins.clone()));
        assert!(cache.begin_refresh(privacy.clone()));
        cache.finish_refresh(&plugins);
        assert!(cache.begin_refresh(plugins));
    }
}
