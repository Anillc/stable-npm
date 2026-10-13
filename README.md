# stable-npm

Read-only, time-traveling proxy for the npm registry: given a package and a point
in time, it serves the packument as it was then.

## Usage

The instant is part of the registry path; npm appends the package name itself.

```console
$ ./stable-npm
$ npm install --registry http://127.0.0.1:3000/2020-01-01T00:00:00Z/ lodash
```

- The base must end with `/`.
- `GET /<time>/<name>` returns the packument, `GET /<time>/<name>/<version-or-tag>`
  a single version.

## Semantics

- `<time>` is RFC3339 UTC, e.g. `2020-01-01T00:00:00Z`. Bare unix timestamps are
  rejected.
- Versions published at or before `<time>` are served.
- A `<time>` in the future is a 400.
- A `<time>` before the package existed, or an unknown package, is npm's
  `404 {"error":"Not found"}`.
- The same `(package, time)` always returns the same bytes.

## Storage

SQLite in WAL mode. `versions` is append-only and immutable; `packages` holds one
watermark per package. Responses are not stored; they are rebuilt from
`versions`, so the database grows with the number of versions, not with requests.
Finished responses are cached in memory, bounded by `--cache-mb`.

## Config

| Flag | Default |
| --- | --- |
| `--addr` | `127.0.0.1:3000` |
| `--db` | `stable-npm.db` |
| `--upstream` | `https://registry.npmjs.org` |
| `--timeout` | `120` (seconds) |
| `--cache-mb` | `128` |
| `--log` | `info` |

## Limits

- Only the `latest` dist-tag is served; npm exposes no tag history.
- Single instance.
- Tarballs are never proxied or stored. Packuments keep pointing at
  registry.npmjs.org, and npm's rewritten tarball requests are 302'd back there.
