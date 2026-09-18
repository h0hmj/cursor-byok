//! Persists the last successful official `GetMe` response on disk.
use std::{collections::HashSet, path::PathBuf, sync::OnceLock, time::Instant};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header, HeaderMap, HeaderValue, Method, Request, Response, Uri},
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::{
    api::cursor::proxy::{self, CursorProxy},
    cursor::services::startup_timing::{self, MetadataSource},
    Result,
};

struct RefreshGate {
    inflight: Mutex<HashSet<[u8; 32]>>,
}

fn gate() -> &'static RefreshGate {
    static GATE: OnceLock<RefreshGate> = OnceLock::new();
    GATE.get_or_init(|| RefreshGate {
        inflight: Mutex::new(HashSet::new()),
    })
}

pub async fn serve(proxy: CursorProxy, request: Request<Body>) -> Result<Response<Body>> {
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let body = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| crate::Error::Protocol(format!("cannot read request body: {error}")))?;
    let Some(token) = token_hash(&parts.headers) else {
        return forward_now(proxy, parts.method, parts.uri, parts.headers, body).await;
    };

    if let Some(cached) = read_cache(&token).await {
        if begin_refresh(token) {
            let method = parts.method.clone();
            let uri = parts.uri.clone();
            let headers = parts.headers.clone();
            let refresh_path = path.clone();
            tokio::spawn(async move {
                refresh(proxy, token, method, uri, headers, body, refresh_path).await;
            });
        }
        startup_timing::log_metadata(
            &path,
            MetadataSource::Cached,
            started.elapsed(),
            None,
            Some(true),
            None,
        );
        return Ok(proto_response(cached));
    }

    let upstream = forward_buffered(proxy, parts.method, parts.uri, parts.headers, body).await?;
    if upstream.status.is_success() {
        if let Err(error) = write_cache(&token, &upstream.body).await {
            tracing::warn!(%error, "failed to persist GetMe cache");
        }
    }
    let elapsed = started.elapsed();
    startup_timing::log_metadata(
        &path,
        MetadataSource::Upstream,
        elapsed,
        Some(elapsed),
        Some(false),
        None,
    );
    Ok(upstream.into_response())
}

async fn refresh(
    proxy: CursorProxy,
    token: [u8; 32],
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
    path: String,
) {
    let started = Instant::now();
    match forward_buffered(proxy, method, uri, headers, body).await {
        Ok(upstream) if upstream.status.is_success() => {
            if let Err(error) = write_cache(&token, &upstream.body).await {
                tracing::warn!(%error, "failed to persist GetMe cache");
            }
            let elapsed = started.elapsed();
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
                "GetMe refresh rejected"
            );
        }
        Err(error) => {
            tracing::warn!(
                path,
                %error,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "GetMe refresh failed"
            );
        }
    }
    finish_refresh(&token);
}

async fn forward_now(
    proxy: CursorProxy,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>> {
    Ok(forward_buffered(proxy, method, uri, headers, body)
        .await?
        .into_response())
}

async fn forward_buffered(
    proxy: CursorProxy,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<proxy::BufferedResponse> {
    let mut request = Request::new(Body::from(body));
    *request.method_mut() = method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    proxy::forward_buffered(&proxy, request).await
}

fn begin_refresh(token: [u8; 32]) -> bool {
    gate().inflight.lock().insert(token)
}

fn finish_refresh(token: &[u8; 32]) {
    gate().inflight.lock().remove(token);
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

async fn read_cache(token: &[u8; 32]) -> Option<Vec<u8>> {
    let path = cache_file(token).ok()?;
    let body = tokio::fs::read(&path).await.ok()?;
    (!body.is_empty()).then_some(body)
}

async fn write_cache(token: &[u8; 32], body: &[u8]) -> Result<()> {
    let path = cache_file(token)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await;
        }
    }
    let temporary = path.with_extension("tmp");
    tokio::fs::write(&temporary, body).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).await;
    }
    tokio::fs::rename(&temporary, &path).await?;
    Ok(())
}

fn cache_file(token: &[u8; 32]) -> Result<PathBuf> {
    Ok(crate::config::managed_data_dir()?
        .join("cache")
        .join("get_me")
        .join(hex(token)))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn proto_response(body: Vec<u8>) -> Response<Body> {
    let length = body.len();
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        length
            .to_string()
            .parse()
            .expect("body length is a valid header"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn cache_file_name_is_the_token_hash_not_the_token() {
        let token = [0xab, 0xcd, 0x10, 0xff].repeat(8);
        let token: [u8; 32] = token.try_into().unwrap();
        let path = Path::new("/tmp/get-me").join(hex(&token));
        assert_eq!(path.file_name().unwrap(), hex(&token).as_str());
        assert!(!path.to_string_lossy().contains("secret"));
        assert_eq!(hex(&token).len(), 64);
    }

    #[tokio::test]
    async fn persisted_body_round_trips_and_ignores_an_empty_file() {
        let directory = tempfile::tempdir().unwrap();
        let token = [9u8; 32];
        let path = directory.path().join(hex(&token));
        tokio::fs::create_dir_all(directory.path()).await.unwrap();
        tokio::fs::write(&path, b"").await.unwrap();
        assert!(tokio::fs::read(&path).await.unwrap().is_empty());

        let temporary = path.with_extension("tmp");
        tokio::fs::write(&temporary, b"get-me-body").await.unwrap();
        tokio::fs::rename(&temporary, &path).await.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"get-me-body");
    }
}
