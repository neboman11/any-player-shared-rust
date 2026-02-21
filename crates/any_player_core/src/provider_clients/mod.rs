pub mod jellyfin;
pub mod plex;
pub mod spotify;

use crate::providers::{ProviderAuthRequest, ProviderError};

pub(super) fn required_session_param<'a>(
    session: &'a ProviderAuthRequest,
    provider_name: &str,
    key: &'static str,
) -> Result<&'a str, ProviderError> {
    session
        .get(key)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ProviderError(format!("Missing {} {}", provider_name, key)))
}
