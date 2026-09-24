# JP iOS 1.0.3 proxy protocol subset

This bundle contains the ten RPCs in `src/routes.rs`, six services and their transitive
request/response message and enum dependencies. There are 46 proto files: 45 game schema
files and the standard `google/protobuf/descriptor.proto` dependency for method annotations.
Only the `skip_authentication` custom option is retained.

All fields, numbers, enum values and presence semantics of reachable messages remain intact.
The internal GetPlayerData response requires its complete reachable message graph.
Registration, account binding, payment, administration and debug RPCs are not included.

`bundle.json` and `proto/` are build and runtime inputs. Matching fingerprints select native
codecs; compatible hot updates use dynamic codecs. See [protocol updates](../../../docs/PROTO_RELOAD.md).
The reduced `tests/fixtures/proxy-descriptors.pb` independently checks source and codec
compatibility and is never loaded at runtime. Changing the allowlist requires reviewing
both the dependency closure and this test baseline; never replace it with a full game dump.
