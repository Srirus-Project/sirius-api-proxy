# Changelog

## 1.1.0

- Add explicit JP/TW/EN/KR identities and reserve CN without enabling unverified networking.
- Separate region, platform and environment; reject mismatched known service endpoints.
- Document capability boundaries and paired-service upgrade requirements in `docs/REGIONS.md`.
- Generate independent native JP and Global protobuf codecs, retaining compatible dynamic reload.
- Add authenticated region capability and Global server-discovery routes; expose resource version observations.
- Emit region-bearing schema-2 snapshots and select the requested platform hash.
- Refuse Global operations outside the verified discovery/version protocol rather than using JP messages.


## 1.0.0

- Add a default-on `session_lock` configuration switch for upstream RPC serialization.
- Initial public-release candidate for BanG Dream! Our Notes.
- Replace the pre-release Viola codename with Sirius configuration filenames,
  SIRIUS_* environment variables and container user names. Old names are not aliases.
- Retain the appropriate Haruki derived-from attribution and MIT notices.
- Include runtime files, configuration examples, documentation and licenses in release archives.
- Keep real credentials, downloaded content and private research out of public artifacts.

See README.md for supported features, validated scope and current limitations.
