//! Fetching packuments from the real npm registry.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::timeutil::parse_instant;

pub struct FetchedVersion {
    pub version: String,
    pub publish_ms: i64,
    pub manifest_json: String,
}

pub struct Packument {
    pub versions: Vec<FetchedVersion>,
    pub created_ms: Option<i64>,
}

pub enum Fetched {
    Missing,
    Found(Packument),
}

pub struct Upstream {
    /// Built on first use so that constructing an `Upstream` (e.g. in tests)
    /// does not require a usable system CA store.
    client: OnceLock<reqwest::Client>,
    base: String,
    timeout: Duration,
    user_agent: &'static str,
}

impl Upstream {
    pub fn new(base: &str, timeout: Duration) -> Self {
        Self {
            client: OnceLock::new(),
            base: base.trim_end_matches('/').to_string(),
            timeout,
            user_agent: concat!("stable-npm/", env!("CARGO_PKG_VERSION")),
        }
    }

    fn client(&self) -> Result<&reqwest::Client> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = reqwest::Client::builder()
            .user_agent(self.user_agent)
            .timeout(self.timeout)
            .build()
            .context("build http client")?;
        let _ = self.client.set(client);
        Ok(self.client.get().expect("client was just set"))
    }

    pub async fn fetch(&self, name: &str) -> Result<Fetched> {
        let url = format!("{}/{}", self.base, encode_package_name(name));
        let response = self
            .client()?
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| format!("request {url}"))?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Fetched::Missing);
        }
        if !status.is_success() {
            anyhow::bail!("upstream {url} returned {status}");
        }

        let body = response.text().await.context("read upstream body")?;
        let root: Value =
            serde_json::from_str(&body).with_context(|| format!("parse packument {url}"))?;

        let time = root.get("time").and_then(Value::as_object);
        let created_ms = time
            .and_then(|t| t.get("created"))
            .and_then(Value::as_str)
            .and_then(parse_instant);

        let versions_obj = root
            .get("versions")
            .and_then(Value::as_object)
            .context("packument has no versions object")?;

        let mut versions = Vec::with_capacity(versions_obj.len());
        for (version, manifest) in versions_obj {
            let Some(publish_ms) = time
                .and_then(|t| t.get(version))
                .and_then(Value::as_str)
                .and_then(parse_instant)
            else {
                tracing::warn!(package = %name, version = %version, "no publish time, skipping");
                continue;
            };
            let manifest_json =
                serde_json::to_string(manifest).context("serialize version manifest")?;
            versions.push(FetchedVersion {
                version: version.clone(),
                publish_ms,
                manifest_json,
            });
        }

        Ok(Fetched::Found(Packument {
            versions,
            created_ms,
        }))
    }

    pub fn base(&self) -> &str {
        &self.base
    }
}

/// npm encodes the `/` of a scoped name as `%2F`: `@scope%2Fname`.
fn encode_package_name(name: &str) -> String {
    name.replace('/', "%2F")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_scoped_names() {
        assert_eq!(encode_package_name("express"), "express");
        assert_eq!(encode_package_name("@types/node"), "@types%2Fnode");
    }
}
