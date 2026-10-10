# Global Android 1.0.3 proxy protocol subset

Shared by TW/HK/MO (`hk`), EN Region (`en`) and Korea (`kr`). The same eleven RPCs, reduction
rules and retained `skip_authentication` method option as `protocol/global/1.0.1`; every message is
copied unchanged from the descriptors embedded in the Global 1.0.3 client (`com.bilibili.sirius`
versionCode 56). There are 48 files: 47 game schema files and the standard
`google/protobuf/descriptor.proto` dependency.

Compared with 1.0.1 the subset only gains definitions, the same ones JP gained from 1.0.3 to 1.0.4:
`Announcement.platform` and the `AnnouncementPlatform` enum, the costume fields of `PlayerData`
and `Notification` with the new `entity/character_costume.proto`, and
`ResourceType.RESOURCE_TYPE_CHARACTER_UNLOCKED_COSTUME`. `PlayerLogin`, `GetServerList` and every
other message are unchanged. The 1.0.1 bundle stays available for a restart-based rollback; a
running 1.0.3 service cannot hot-reload back to it because that would remove fields.

The client still names `ServerInfo` field 8 `areaID`; see `protocol/global/1.0.1/README.md`.

`bundle.json` and `proto/` are build and runtime inputs with their own native codecs and a
family-aware fingerprint. See `docs/REGIONS.md` for the capability boundary.
