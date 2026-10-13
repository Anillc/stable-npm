//! SQLite persistence.
//!
//! `versions` is append-only and immutable, so unpublishing (or any other
//! upstream edit) cannot change what we have already observed.
//!
//! `watermark_ms` is the largest `<time>` answered for a package and only moves
//! up: `t <= watermark_ms` is served locally, `t > watermark_ms` refetches. A
//! version that shows up later with a publish time at or before it gets
//! `hidden_until_ms` set, keeping it out of answers we already gave while still
//! storing it for later queries.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use tokio_rusqlite::Connection;

use crate::upstream::FetchedVersion;

const SCHEMA: &str = r"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS packages (
    name         TEXT PRIMARY KEY,
    watermark_ms INTEGER NOT NULL,
    created_ms   INTEGER
);

CREATE TABLE IF NOT EXISTS versions (
    name            TEXT NOT NULL,
    version         TEXT NOT NULL,
    publish_ms      INTEGER NOT NULL,
    manifest        TEXT NOT NULL,
    -- hides a late-discovered version from answers already given
    hidden_until_ms INTEGER,
    PRIMARY KEY (name, version)
);

CREATE INDEX IF NOT EXISTS idx_versions_name_publish
    ON versions (name, publish_ms);
";

pub struct VersionRow {
    pub version: String,
    pub publish_ms: i64,
    pub manifest_json: String,
}

pub struct Snapshot {
    pub created_ms: Option<i64>,
    pub versions: Vec<VersionRow>,
}

#[derive(Clone)]
pub struct Db {
    conn: Connection,
}

impl Db {
    pub async fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).await?;
        Self::from_connection(conn).await
    }

    #[cfg(test)]
    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().await?;
        Self::from_connection(conn).await
    }

    async fn from_connection(conn: Connection) -> Result<Self> {
        conn.call(|c| {
            c.execute_batch(SCHEMA)?;
            Ok::<(), rusqlite::Error>(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn get_watermark(&self, name: &str) -> Result<Option<i64>> {
        let name = name.to_string();
        let watermark = self
            .conn
            .call(move |c| {
                c.query_row(
                    "SELECT watermark_ms FROM packages WHERE name = ?1",
                    params![name],
                    |r| r.get(0),
                )
                .optional()
            })
            .await?;
        Ok(watermark)
    }

    /// The watermark is not touched here: it only moves when a request is
    /// actually answered.
    pub async fn upsert_found(&self, name: &str, created_ms: Option<i64>) -> Result<()> {
        let name = name.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO packages (name, watermark_ms, created_ms)
                     VALUES (?1, 0, ?2)
                     ON CONFLICT(name) DO UPDATE SET
                        created_ms = COALESCE(packages.created_ms, excluded.created_ms)",
                    params![name, created_ms],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Ensure a row exists for a package upstream reported as missing. A 404
    /// needs no column of its own: no versions plus `t <= watermark_ms` already
    /// means 404.
    pub async fn ensure_package(&self, name: &str) -> Result<()> {
        let name = name.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO packages (name, watermark_ms, created_ms)
                     VALUES (?1, 0, NULL)
                     ON CONFLICT(name) DO NOTHING",
                    params![name],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Never updates or deletes an existing row; a late arrival gets
    /// `hidden_until_ms` so earlier answers stand.
    pub async fn merge_versions(&self, name: &str, versions: Vec<FetchedVersion>) -> Result<()> {
        let name = name.to_string();
        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                let watermark: i64 = tx
                    .query_row(
                        "SELECT watermark_ms FROM packages WHERE name = ?1",
                        params![name],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);
                {
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO versions
                            (name, version, publish_ms, manifest, hidden_until_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?;
                    for v in &versions {
                        let hidden_until = (v.publish_ms <= watermark).then_some(watermark);
                        stmt.execute(params![
                            name,
                            v.version,
                            v.publish_ms,
                            v.manifest_json,
                            hidden_until
                        ])?;
                    }
                }
                tx.commit()?;
                Ok::<(), rusqlite::Error>(())
            })
            .await?;
        Ok(())
    }

    /// Read the versions visible at `t_ms` and advance the watermark in one
    /// transaction. The atomicity is what keeps determinism sound: no version
    /// that shows up later can slip into an answer already given.
    pub async fn snapshot_and_mark(&self, name: &str, t_ms: i64) -> Result<Snapshot> {
        let name = name.to_string();
        let snapshot = self
            .conn
            .call(move |c| {
                let tx = c.transaction()?;
                let created_ms: Option<i64> = tx
                    .query_row(
                        "SELECT created_ms FROM packages WHERE name = ?1",
                        params![name],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let versions = {
                    let mut stmt = tx.prepare(
                        "SELECT version, publish_ms, manifest
                         FROM versions
                         WHERE name = ?1
                           AND publish_ms <= ?2
                           AND (hidden_until_ms IS NULL OR hidden_until_ms < ?2)
                         ORDER BY publish_ms ASC, version ASC",
                    )?;
                    stmt.query_map(params![name, t_ms], |r| {
                        Ok(VersionRow {
                            version: r.get(0)?,
                            publish_ms: r.get(1)?,
                            manifest_json: r.get(2)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                };
                tx.execute(
                    "UPDATE packages SET watermark_ms = MAX(watermark_ms, ?2)
                     WHERE name = ?1",
                    params![name, t_ms],
                )?;
                tx.commit()?;
                Ok::<_, rusqlite::Error>(Snapshot {
                    created_ms,
                    versions,
                })
            })
            .await?;
        Ok(snapshot)
    }
}
