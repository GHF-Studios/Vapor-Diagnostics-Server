# Vapor Diagnostics Server

Opt-in diagnostics/log upload service for Vapor.

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

## Non-goals

- Steam/GitHub identity authority;
- docs artifact storage;
- homepage/legal content;
- deployment orchestration.
