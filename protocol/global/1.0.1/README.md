# Global Android 1.0.1 proxy protocol subset

Shared by TW/HK/MO (`hk`), EN Region (`en`) and Korea (`kr`). Every message is copied unchanged
from the descriptors embedded in the Global 1.0.1 client (field names, numbers, types, labels and
presence). Like `protocol/sirius/1.0.3`, only the RPCs in `src/routes.rs` for the Global family
and their transitive message/enum dependencies are kept, and only the `skip_authentication`
method option is retained. File-level C# options are omitted. There are 47 files: 46 game schema
files and the standard `google/protobuf/descriptor.proto` dependency.

RPCs:

- Anonymous: `MasterdataService/Version`, `PlayerLoginService/GetServerList`,
  `AnnouncementService/GetList` and `Get`.
- Internal login (never a public route): `PlayerLoginService/PlayerLogin`, which exchanges an SDK
  identity for a game `entity.PlayerCredential`.
- Authenticated: `FriendService/FindByProfileID`, `EventService/GetRankingList`, `GetDeck`,
  `GetChallengeMusicRanking`, `LiveMusicService/GetRanking` and `PlayerService/GetPlayerData`.

`PlayerService/Whoami` is deliberately absent: Global production rejects it, and the account
identity comes from the `PlayerLogin` response. Registration, pre-login probing, payment,
administration and debug RPCs are not included.

The client names `ServerInfo` field 8 `areaID` (JSON `areaID`). The proxy checks that field by
number and keeps publishing it as `areaId` in `/api/v1/servers`.

`bundle.json` and `proto/` are build and runtime inputs with their own native codecs and a
family-aware fingerprint. See `docs/REGIONS.md` for the capability boundary.
