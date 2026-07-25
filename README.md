# Vapor Diagnostics Server

Opt-in diagnostics/log upload service for Vapor.

Initial implementation uses Axum/Tokio.

## Responsibility

- accept explicit diagnostics uploads;
- store uploaded diagnostics bundles;
- index diagnostics metadata;
- support authorized root developer listing/download;
- support diagnostics export/import;
- enforce upload size limits and retention policy.

## Diagnostics policy direction

- Upload is explicit opt-in.
- Git is not a diagnostics transport.
- Normal players do not need GitHub for diagnostics upload.
- Do not capture hostname.
- Do not capture persistent machine id.
- Rough non-identifying system specs are acceptable.
- Redacted logs and small text bundles are preferred.

## Route

Expected public API route behind the root reverse proxy:

```text
/api/diagnostics/
```

## State

Owns diagnostics bundles, diagnostics indexes, retention metadata, and
diagnostics export/import data.

## Routes

```text
GET  /healthz
GET  /v1/status
POST /v1/runs
GET  /v1/runs
GET  /v1/runs/<run-id>
GET  /v1/export
POST /v2/reports
GET  /v2/reports
GET  /v2/reports/<run-id>
```

`GET /v1/status` is public and reports current storage/readiness, upload limit,
configured aggregate storage limit, diagnostics count, collection policy,
supported schema versions, and temporary auth model. It must not expose tokens.

Upload is intentionally unauthenticated but explicit opt-in so normal players do
not need GitHub. Listing, download, and export are protected and expect:

```text
Authorization: Bearer <VAPOR_DIAGNOSTICS_ADMIN_TOKEN>
```

## Diagnostics v2 upload

`POST /v2/reports` accepts JSON:

```json
{
  "schema_version": 2,
  "consent": true,
  "client_version": "local-build-id",
  "platform": {
    "os_family": "linux",
    "arch": "x86_64",
    "memory_mb_bucket": "8192-16383",
    "steam_deck": false
  },
  "artifacts": [
    {
      "name": "vapor.log",
      "content": "log text"
    }
  ]
}
```

Allowed artifact names are:

- `vapor.log`
- `launcher.log`
- `steps.txt`
- `errors.txt`

The service rejects unknown JSON fields and non-allowlisted artifact names. The
schema does not include hostname, persistent machine identifier, remote IP,
account identifier, serial number, or MAC address fields.

The legacy `POST /v1/runs` plain-text upload route remains available and uses
the same redaction/storage core.

## Storage and redaction

Each accepted upload receives a collision-resistant run ID:

```text
diag-<unix-milliseconds>-<uuid-v4>
```

Runs are staged before being atomically promoted under `runs/<run-id>/`.
Stored files are:

```text
metadata.json
metadata.toml
vapor.log
```

Raw request bodies are not retained. Logs are normalized and redacted before
storage. Redaction covers common sensitive key/value forms, authorization and
cookie headers, sensitive URL query parameters, ticket/auth values, and common
GitHub token prefixes.

`VAPOR_DIAGNOSTICS_MAX_STORED_BYTES` optionally sets a non-secret aggregate
storage quota. The default is `268435456` bytes. Exceeding the quota rejects the
new upload without deleting existing reports.

## Non-goals

- Steam/GitHub identity authority;
- docs artifact storage;
- homepage/legal content;
- deployment orchestration.
