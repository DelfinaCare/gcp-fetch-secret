//! Where secrets come from. The [`SecretSource`] trait lets the sync logic be
//! tested against an in-memory fake; production uses Secret Manager.

use std::future::Future;

use google_cloud_secretmanager_v1::client::SecretManagerService;

pub type SourceError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedSecret {
    /// The concrete version number the request resolved to (e.g. `7` when
    /// `latest` was requested).
    pub version: String,
    pub data: Vec<u8>,
}

pub trait SecretSource: Send + Sync {
    /// Fetch the secret payload.
    fn access(
        &self,
        secret_id: &str,
    ) -> impl Future<Output = Result<FetchedSecret, SourceError>> + Send;

    /// Resolve the version `secret_id` currently points at, without fetching
    /// the payload.
    fn current_version(
        &self,
        secret_id: &str,
    ) -> impl Future<Output = Result<String, SourceError>> + Send;
}

/// `projects/1/secrets/s/versions/7` -> `7`
pub fn version_from_name(name: &str) -> Result<String, SourceError> {
    match name.rsplit_once("/versions/") {
        Some((_, v)) if !v.is_empty() && !v.contains('/') => Ok(v.to_owned()),
        _ => Err(format!("unexpected secret version name {name:?} in response").into()),
    }
}

impl SecretSource for SecretManagerService {
    async fn access(&self, secret_id: &str) -> Result<FetchedSecret, SourceError> {
        let response = self
            .access_secret_version()
            .set_name(secret_id)
            .send()
            .await?;
        let payload = response
            .payload
            .ok_or_else(|| format!("response for {secret_id} has no payload"))?;
        Ok(FetchedSecret {
            version: version_from_name(&response.name)?,
            data: payload.data.to_vec(),
        })
    }

    async fn current_version(&self, secret_id: &str) -> Result<String, SourceError> {
        let version = self.get_secret_version().set_name(secret_id).send().await?;
        version_from_name(&version.name)
    }
}

/// Build a Secret Manager client using Application Default Credentials.
///
/// `endpoint` overrides the API endpoint (for regional endpoints, Private
/// Service Connect, or a local emulator). Plain `http://` endpoints are only
/// meant for local testing, so they get anonymous credentials: an access
/// token is never sent over an unencrypted connection.
pub async fn secret_manager(endpoint: Option<&str>) -> Result<SecretManagerService, SourceError> {
    let mut builder = SecretManagerService::builder();
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
        if endpoint.starts_with("http://") {
            builder = builder.with_credentials(
                google_cloud_auth::credentials::anonymous::Builder::new().build(),
            );
        }
    }
    Ok(builder.build().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_name() {
        assert_eq!(
            version_from_name("projects/123/secrets/s/versions/7").unwrap(),
            "7"
        );
        assert_eq!(
            version_from_name("projects/1/locations/l/secrets/s/versions/12").unwrap(),
            "12"
        );
        assert!(version_from_name("").is_err());
        assert!(version_from_name("projects/1/secrets/s").is_err());
        assert!(version_from_name("projects/1/secrets/s/versions/").is_err());
    }
}
