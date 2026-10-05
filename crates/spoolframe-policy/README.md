# Spoolframe policy

License: GPL-3.0-or-later. The copyright holder has relicensed the pure watch and
track-selection rules extracted from the AGPL server for sharing with the GPL
Linux client. Storage, authentication, media probing and transcoding stay in the
server; UI and media-engine integration stay in the client.

This crate depends only on permissive protocol/types and serde. It is deliberately
outside the public SDK dependency graph. Watch thresholds are defaults; the
server may configure other values and remains authoritative.
