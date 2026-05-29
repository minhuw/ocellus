use std::time::Duration;

pub(crate) const BYTES_PER_CACHE_LINE: f64 = 64.0;
pub(crate) const DEFAULT_MAX_SLICE: Duration = Duration::from_millis(100);

pub(crate) fn topology_label(value: Option<u32>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
