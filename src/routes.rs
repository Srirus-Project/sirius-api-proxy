pub const VERSION: &str = "/app.masterdata.MasterdataService/Version";
pub const ANNOUNCEMENTS: &str = "/app.announcement.AnnouncementService/GetList";
pub const ANNOUNCEMENT: &str = "/app.announcement.AnnouncementService/Get";
pub const EVENT_RANKING: &str = "/app.event.EventService/GetRankingList";
pub const EVENT_DECK: &str = "/app.event.EventService/GetDeck";
pub const MUSIC_RANKING: &str = "/app.livemusic.LiveMusicService/GetRanking";
pub const CHALLENGE_RANKING: &str = "/app.event.EventService/GetChallengeMusicRanking";
pub const WHOAMI: &str = "/app.player.PlayerService/Whoami";
pub const PLAYER_DATA: &str = "/app.player.PlayerService/GetPlayerData";
pub const PROFILE: &str = "/app.friend.FriendService/FindByProfileID";
pub const ROUTES: &[&str] = &[
    VERSION,
    ANNOUNCEMENTS,
    ANNOUNCEMENT,
    PROFILE,
    EVENT_RANKING,
    EVENT_DECK,
    MUSIC_RANKING,
    CHALLENGE_RANKING,
    WHOAMI,
    PLAYER_DATA,
];

pub const SERVER_LIST: &str = "/app.playerlogin.PlayerLoginService/GetServerList";
/// Global login: exchanges an SDK identity for a game credential. Internal only, never public.
pub const PLAYER_LOGIN: &str = "/app.playerlogin.PlayerLoginService/PlayerLogin";
/// Public Global routes. Whoami is disabled on Global production and is never sent; the account
/// identity comes from the PlayerLogin response instead.
pub const GLOBAL_ROUTES: &[&str] = &[
    VERSION,
    SERVER_LIST,
    ANNOUNCEMENTS,
    ANNOUNCEMENT,
    PROFILE,
    EVENT_RANKING,
    EVENT_DECK,
    MUSIC_RANKING,
    CHALLENGE_RANKING,
    PLAYER_DATA,
];
const GLOBAL_CONTRACT: &[&str] = &[
    VERSION,
    SERVER_LIST,
    ANNOUNCEMENTS,
    ANNOUNCEMENT,
    PROFILE,
    EVENT_RANKING,
    EVENT_DECK,
    MUSIC_RANKING,
    CHALLENGE_RANKING,
    PLAYER_DATA,
    PLAYER_LOGIN,
];
/// Routes the proxy exposes for a protocol family.
pub fn for_family(family: &str) -> &'static [&'static str] {
    if family == "global" {
        GLOBAL_ROUTES
    } else {
        ROUTES
    }
}
/// Every RPC a family's bundle must provide: public routes plus internal login.
pub fn contract_for_family(family: &str) -> &'static [&'static str] {
    if family == "global" {
        GLOBAL_CONTRACT
    } else {
        ROUTES
    }
}
