# Agent instructions

Keep this service focused on opt-in diagnostics upload/storage/export.

- Do not add Git-backed diagnostics transport.
- Do not collect hostname or persistent machine identity.
- Do not commit secrets.
- Keep authorization integration delegated to the identity service.
- Keep diagnostics state export/import in scope.
- Route integration belongs to `Vapor-Server-Root`.
