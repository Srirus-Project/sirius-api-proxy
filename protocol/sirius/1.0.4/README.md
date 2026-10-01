# JP 1.0.4 proxy protocol subset

This bundle contains the ten RPCs in `src/routes.rs`, six services and their transitive
request/response message and enum dependencies. There are 47 proto files: 46 game schema
files and the standard `google/protobuf/descriptor.proto` dependency for method annotations.
Only the `skip_authentication` custom option is retained.

All fields, numbers, enum values and presence semantics of reachable messages remain intact.
The internal GetPlayerData response requires its complete reachable message graph.
Registration, account binding, payment, administration and debug RPCs are not included.

Compared with 1.0.3 the subset only gains definitions: `Announcement.platform` and the
`AnnouncementPlatform` enum, the costume fields of `PlayerData` and `Notification` with the new
`entity/character_costume.proto`, and `ResourceType.RESOURCE_TYPE_CHARACTER_UNLOCKED_COSTUME`.
The 1.0.3 bundle stays available for a restart-based rollback; a running 1.0.4 service cannot
hot-reload back to it because that would remove fields.

`bundle.json` and `proto/` are build and runtime inputs. Matching fingerprints select native
codecs; compatible hot updates use dynamic codecs. See [protocol updates](../../../docs/PROTO_RELOAD.md).
The reduced `tests/fixtures/proxy-descriptors.pb` independently checks source and codec
compatibility and is never loaded at runtime. Changing the allowlist requires reviewing
both the dependency closure and this test baseline; never replace it with a full game dump.
