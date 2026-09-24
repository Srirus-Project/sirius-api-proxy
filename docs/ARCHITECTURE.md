# Architecture

HTTP routes → allowlisted RPCs → active native/dynamic codec → HTTP/2 unary gRPC.
Each deployment has one upstream environment and one optional existing account.
Authenticated queries first establish the Master version; anonymous RPCs omit account credentials.

- `src/api.rs`: public queries and internal state/account routes.
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
No database, account pool or arbitrary RPC passthrough is included. Other game versions
require independent validation. Original project attribution is preserved without unrelated
source code or development history.
