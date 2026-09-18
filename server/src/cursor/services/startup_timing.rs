//! Structured timing for Cursor metadata endpoints on the CLI startup path.
use std::time::Duration;

/// How a metadata request was satisfied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataSource {
    Local,
    Cached,
    Upstream,
    Merged,
}

impl MetadataSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cached => "cached",
            Self::Upstream => "upstream",
            Self::Merged => "merged",
        }
    }
}

/// Logs a single metadata handling outcome without auth or body contents.
pub fn log_metadata(
    path: &str,
    source: MetadataSource,
    total: Duration,
    upstream: Option<Duration>,
    cache_hit: Option<bool>,
    cache_age: Option<Duration>,
) {
    tracing::info!(
        path,
        source = source.as_str(),
        total_ms = total.as_millis() as u64,
        upstream_ms = upstream.map(|value| value.as_millis() as u64),
        cache_hit,
        cache_age_ms = cache_age.map(|value| value.as_millis() as u64),
        "cursor metadata"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_labels_match_plan_contract() {
        assert_eq!(MetadataSource::Local.as_str(), "local");
        assert_eq!(MetadataSource::Cached.as_str(), "cached");
        assert_eq!(MetadataSource::Upstream.as_str(), "upstream");
        assert_eq!(MetadataSource::Merged.as_str(), "merged");
    }
}
