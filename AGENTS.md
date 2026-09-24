# Development rules

This is a standalone Sirius project for BanG Dream! Our Notes. Do not reintroduce Sekai game protocols,
CP/Nuverse providers, legacy account databases, Ent schemas or Master DB ingestion.
Reuse Haruki concepts only where they fit the actual Sirius protocol.
Keep Haruki MIT attribution and Sirius attribution in LICENSE; sources are in docs/SOURCES.md.

- Rust implementation lives in `src/`; there are no legacy workspace members.
- Never commit tokens, credentials, local config, game binaries or downloaded resources.
- Test locally; no live game availability is required for tests.
- Run `cargo fmt --all -- --check`, `cargo check --locked --all-targets`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, and `cargo test --locked`.
- Repository Actions remain disabled until deployment is configured, except for explicitly scheduled release preflight runs. Restore the previous setting after preflight; do not create tags, public releases or registry pushes as part of preparation. CI/Release/Docker target this project's root binary.
- Commit subjects use `[Feat]`, `[Fix]`, `[Chore]` or `[Docs]` and an imperative description.
- Include `Co-authored-by: Codex <noreply@openai.com>` in Codex commit bodies.

Build generates native prost/pbjson codecs from `protocol/sirius/1.0.3`. Runtime loads the configured
`.proto` bundle and prefers native only on exact fingerprint match; otherwise use dynamic hot reload.
Keep build/runtime compilation and fingerprinting shared in src/proto_source.rs.
Keep only the RPCs in src/routes.rs and their transitive message/enum dependencies.
The reduced tests/fixtures/proxy-descriptors.pb is an independent regression baseline, not runtime input.
Do not add full game descriptors, protocol inventories, private research links or reverse-engineering artifacts.
Do not invent fields or add generic RPC passthrough. Preserve standard TLS verification,
gRPC trailers, request deadlines, response limits and Protobuf JSON int64 strings.
Game credentials and API/internal bearer tokens have separate scopes. Shared-account fields
must not appear in public rankings. Keep tests inline in src/tests.rs.

Write README.md, AGENTS.md, release notes and user-facing documentation in English.
Use Sirius names and SIRIUS_* environment variables. The game is BanG Dream! Our Notes.
Keep explicit derived-from links to the appropriate Haruki repository in README.md.
Release archives must include runtime files, example configuration, documentation and licenses.
Keep repository visibility, release publication and workflow activation explicit operations.
