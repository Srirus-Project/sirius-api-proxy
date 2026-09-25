# Architecture

HTTP routes → allowlisted RPCs → active native/dynamic codec → HTTP/2 unary gRPC.
Each configured region owns its environment, protocol bundle, bounded account pool and state.
Single-region deployments preserve the original routes; multi-region deployments add region prefixes.
Authenticated queries first establish the Master version; anonymous RPCs omit account credentials.

- `src/api.rs`: public queries and internal state/account routes.
- `src/peer.rs`: separately authorized, local-only node query contract.
- `src/node_routing.rs`, `src/peer_transport.rs`: public query priorities, passive health and bounded peer calls.
- `src/deployment.rs`, `src/accounts.rs`: region assembly, account selection and credential reload.
- `src/response_cache.rs`: bounded memory/Redis cache with opt-in stale refresh.
- `src/asset_dispatch.rs`, `src/asset_outbox.rs`: durable updater dispatch and recovery.
- `build.rs`, `src/native.rs`: generated Protobuf/JSON codecs and static RPC dispatch.
- `src/proto_source.rs`: shared source snapshot compiler and semantic fingerprint.
- `src/protocol.rs`: contract validation, codec selection and atomic reload.
- `src/client.rs`: metadata, TLS, trailers, configurable deadlines/concurrency/response limits and bounded anonymous retries.
- `src/resources.rs`: compatible resource version selection and credential references.
- `src/master.rs`, `src/rijndael.rs`: validated Master decoding and atomic local snapshots.
- `src/master_update.rs`: manifest downloads, version rechecks, writer locking and periodic updates.
- `src/config.rs`, `src/error.rs`: configuration validation and sanitized errors.
- `src/tests.rs`: local protocol, boundary and integration fixtures.

Sirius Asset Updater consumes resource snapshots without owning the account lifecycle.
No Sekai account models or arbitrary RPC passthrough are included. Other game versions
require independent validation. Original project attribution is preserved without unrelated
source code or development history.
