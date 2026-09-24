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
