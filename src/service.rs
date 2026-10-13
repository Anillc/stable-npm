//! The core logic: turn `(name, t)` into a packument that is reproducible
//! byte-for-byte for the same request.

use anyhow::{Result, anyhow};
use bytes::Bytes;
use moka::future::Cache;
use serde_json::{Map, Value};

use crate::db::{Db, Snapshot};
use crate::timeutil::format_ms;
use crate::upstream::{Fetched, Upstream};

#[derive(Clone)]
pub struct Answer {
    pub status: u16,
    pub body: Bytes,
    pub etag: String,
}

impl Answer {
    pub fn ok(body: impl Into<Bytes>) -> Self {
        let body = body.into();
        let etag = etag_for(&body);
        Self {
            status: 200,
            body,
            etag,
        }
    }

    /// The same 404 body registry.npmjs.org sends.
    pub fn not_found() -> Self {
        let body = Bytes::from_static(br#"{"error":"Not found"}"#);
        let etag = etag_for(&body);
        Self {
            status: 404,
            body,
            etag,
        }
    }
}

fn etag_for(body: &[u8]) -> String {
    format!("\"{}\"", blake3::hash(body).to_hex())
}

pub struct Service {
    db: Db,
    upstream: Upstream,
    /// Bounded cache of finished responses, keyed by `(name, t_ms)`. It caps
    /// memory, coalesces concurrent requests for the same key into one build,
    /// and avoids re-serializing large packuments. Answers are immutable, so a
    /// hit is always safe.
    answers: Cache<(String, i64), Answer>,
}

impl Service {
    pub fn new(db: Db, upstream: Upstream, cache_capacity_bytes: u64) -> Self {
        let answers = Cache::builder()
            .max_capacity(cache_capacity_bytes.max(1))
            .weigher(|_key: &(String, i64), answer: &Answer| {
                u32::try_from(answer.body.len()).unwrap_or(u32::MAX)
            })
            .build();
        Self {
            db,
            upstream,
            answers,
        }
    }

    pub async fn packument(&self, name: &str, t_ms: i64) -> Result<Answer> {
        let key = (name.to_string(), t_ms);
        self.answers
            .try_get_with(key, self.build(name, t_ms))
            .await
            .map_err(|error| anyhow!("{error:#}"))
    }

    async fn build(&self, name: &str, t_ms: i64) -> Result<Answer> {
        let needs_refresh = self
            .db
            .get_watermark(name)
            .await?
            .is_none_or(|watermark| t_ms > watermark);
        if needs_refresh {
            self.refresh(name).await?;
        }

        let snapshot = self.db.snapshot_and_mark(name, t_ms).await?;
        render(name, t_ms, &snapshot)
    }

    /// Accepts a version or a dist-tag, like the registry's single-version
    /// endpoint. Derived from the same packument so the two endpoints can never
    /// disagree.
    pub async fn version_manifest(&self, name: &str, version: &str, t_ms: i64) -> Result<Answer> {
        let packument = self.packument(name, t_ms).await?;
        if packument.status != 200 {
            return Ok(packument);
        }
        let root: Value = serde_json::from_slice(&packument.body)?;
        let versions = root.get("versions");
        let resolved = versions.and_then(|v| v.get(version)).or_else(|| {
            root.get("dist-tags")
                .and_then(|tags| tags.get(version))
                .and_then(Value::as_str)
                .and_then(|target| versions.and_then(|v| v.get(target)))
        });
        match resolved {
            Some(manifest) => Ok(Answer::ok(serde_json::to_string(manifest)?)),
            None => Ok(Answer::not_found()),
        }
    }

    /// We never proxy or store tarballs, so bounce npm's rewritten URL back.
    pub fn tarball_location(&self, path: &str) -> String {
        format!("{}/{}", self.upstream.base(), path)
    }

    async fn refresh(&self, name: &str) -> Result<()> {
        let fetched = self.upstream.fetch(name).await?;
        self.ingest(name, fetched).await
    }

    /// Split out from `refresh` so the merge rules can be tested without a
    /// network.
    async fn ingest(&self, name: &str, fetched: Fetched) -> Result<()> {
        match fetched {
            Fetched::Missing => {
                tracing::info!(package = %name, "upstream: not found");
                self.db.ensure_package(name).await?;
            }
            Fetched::Found(packument) => {
                tracing::info!(
                    package = %name,
                    versions = packument.versions.len(),
                    "upstream: fetched"
                );
                if !packument.versions.is_empty() {
                    self.db.merge_versions(name, packument.versions).await?;
                }
                self.db.upsert_found(name, packument.created_ms).await?;
            }
        }
        Ok(())
    }
}

/// Pure: the same snapshot always produces the same bytes.
fn render(name: &str, t_ms: i64, snapshot: &Snapshot) -> Result<Answer> {
    if snapshot.versions.is_empty() {
        return Ok(Answer::not_found());
    }
    if snapshot.created_ms.is_some_and(|created| t_ms < created) {
        return Ok(Answer::not_found());
    }

    let mut versions_map = Map::new();
    let mut time_map = Map::new();
    let mut parsed: Vec<(semver::Version, String)> = Vec::new();
    let mut max_publish = i64::MIN;

    for row in &snapshot.versions {
        let manifest: Value = serde_json::from_str(&row.manifest_json)?;
        versions_map.insert(row.version.clone(), manifest);
        time_map.insert(
            row.version.clone(),
            Value::String(format_ms(row.publish_ms)),
        );
        max_publish = max_publish.max(row.publish_ms);
        if let Ok(semver) = semver::Version::parse(&row.version) {
            parsed.push((semver, row.version.clone()));
        }
    }

    // npm gives no tag history, so we only expose `latest`, recomputed as the
    // highest stable version at t (falling back to the highest prerelease).
    let mut tags = Map::new();
    let latest = parsed
        .iter()
        .filter(|(version, _)| version.pre.is_empty())
        .max_by(|a, b| a.0.cmp(&b.0))
        .or_else(|| parsed.iter().max_by(|a, b| a.0.cmp(&b.0)))
        .map(|(_, version)| version.clone());
    if let Some(latest) = latest {
        tags.insert("latest".to_string(), Value::String(latest));
    }

    let modified = format_ms(max_publish);
    let mut time_obj = Map::new();
    if let Some(created) = snapshot.created_ms {
        time_obj.insert("created".to_string(), Value::String(format_ms(created)));
    }
    for (version, when) in time_map {
        time_obj.insert(version, when);
    }
    time_obj.insert("modified".to_string(), Value::String(modified.clone()));

    let mut root = Map::new();
    root.insert("name".to_string(), Value::String(name.to_string()));
    root.insert("dist-tags".to_string(), Value::Object(tags));
    root.insert("versions".to_string(), Value::Object(versions_map));
    root.insert("time".to_string(), Value::Object(time_obj));
    root.insert("modified".to_string(), Value::String(modified));

    Ok(Answer::ok(serde_json::to_string(&Value::Object(root))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::VersionRow;
    use crate::timeutil::now_ms;
    use crate::upstream::{FetchedVersion, Packument};
    use std::time::Duration;

    fn fetched(name: &str, versions: &[(&str, i64)]) -> Fetched {
        Fetched::Found(Packument {
            versions: versions
                .iter()
                .map(|(version, publish_ms)| FetchedVersion {
                    version: version.to_string(),
                    publish_ms: *publish_ms,
                    manifest_json: serde_json::json!({
                        "name": name,
                        "version": version,
                        "dist": {
                            "tarball": format!(
                                "https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"
                            ),
                        },
                    })
                    .to_string(),
                })
                .collect(),
            created_ms: versions.first().map(|(_, publish_ms)| *publish_ms),
        })
    }

    async fn service() -> Service {
        let db = Db::open_in_memory().await.unwrap();
        let upstream = Upstream::new("https://example.invalid", Duration::from_secs(1));
        Service::new(db, upstream, 8 * 1024 * 1024)
    }

    fn visible(snapshot: &Snapshot) -> Vec<String> {
        snapshot
            .versions
            .iter()
            .map(|row| row.version.clone())
            .collect()
    }

    /// A version published before a time we already answered must stay invisible
    /// there, even though we only learn about it later (upstream CDN lag).
    #[tokio::test]
    async fn late_version_is_hidden_from_earlier_answers() {
        let service = service().await;
        let now = now_ms();
        let at = now - 700;

        service
            .ingest(
                "demo",
                fetched("demo", &[("1.0.0", now - 1000), ("2.0.0", now - 500)]),
            )
            .await
            .unwrap();
        // Pretend requests up to `now` have already been answered.
        service.db.snapshot_and_mark("demo", now).await.unwrap();

        assert_eq!(
            visible(&service.db.snapshot_and_mark("demo", at).await.unwrap()),
            ["1.0.0"]
        );

        service
            .ingest("demo", fetched("demo", &[("1.0.1", now - 800)]))
            .await
            .unwrap();

        assert_eq!(
            visible(&service.db.snapshot_and_mark("demo", at).await.unwrap()),
            ["1.0.0"],
            "the late version must not appear at an earlier time"
        );
        assert!(
            visible(
                &service
                    .db
                    .snapshot_and_mark("demo", now + 10_000)
                    .await
                    .unwrap()
            )
            .contains(&"1.0.1".to_string()),
            "but it must not be dropped either"
        );
    }

    /// Losing a package upstream must not lose the versions we already have.
    #[tokio::test]
    async fn unpublishing_keeps_previous_versions() {
        let service = service().await;
        let now = now_ms();

        service
            .ingest("gone", fetched("gone", &[("1.0.0", now - 1000)]))
            .await
            .unwrap();
        service.db.snapshot_and_mark("gone", now).await.unwrap();
        service.ingest("gone", Fetched::Missing).await.unwrap();

        let answer = service.packument("gone", now - 500).await.unwrap();
        assert_eq!(answer.status, 200);
        let value: Value = serde_json::from_slice(&answer.body).unwrap();
        assert!(value["versions"].get("1.0.0").is_some());
    }

    #[tokio::test]
    async fn version_endpoint_accepts_a_dist_tag() {
        let service = service().await;
        let now = now_ms();
        service
            .ingest(
                "demo",
                fetched("demo", &[("1.0.0", now - 1000), ("2.0.0", now - 500)]),
            )
            .await
            .unwrap();
        service.db.snapshot_and_mark("demo", now).await.unwrap();

        let by_version = service
            .version_manifest("demo", "2.0.0", now)
            .await
            .unwrap();
        let by_tag = service
            .version_manifest("demo", "latest", now)
            .await
            .unwrap();
        assert_eq!(by_tag.status, 200);
        assert_eq!(by_tag.body, by_version.body);

        // Ranges are not a thing on this endpoint, matching the real registry.
        assert_eq!(
            service
                .version_manifest("demo", "1", now)
                .await
                .unwrap()
                .status,
            404
        );
    }

    #[test]
    fn render_picks_latest_stable_and_reports_created() {
        let row = |version: &str, publish_ms: i64| VersionRow {
            version: version.to_string(),
            publish_ms,
            manifest_json: serde_json::json!({ "version": version }).to_string(),
        };
        let snapshot = Snapshot {
            created_ms: Some(1_000),
            versions: vec![row("1.0.0", 1_000), row("2.0.0-beta.1", 1_500)],
        };

        let answer = render("demo", 2_000, &snapshot).unwrap();
        let value: Value = serde_json::from_slice(&answer.body).unwrap();
        assert_eq!(value["dist-tags"]["latest"], "1.0.0");
        assert_eq!(value["time"]["created"], "1970-01-01T00:00:01.000Z");
        assert_eq!(value["time"]["modified"], "1970-01-01T00:00:01.500Z");
    }
}
