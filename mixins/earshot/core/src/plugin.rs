//! The `voicechat:*` plugin-channel messages carried over the Minecraft connection
//! (PROTOCOL.md, section 1). Each type knows its channel name and encodes or decodes the
//! channel's data, not including the channel name itself.

use crate::wire::{DecodeError, Reader, Uuid, Writer};

/// SVC compatibility version these messages match (SVC 2.6.27, from the server log).
pub const COMPATIBILITY_VERSION: i32 = 20;

pub const REQUEST_SECRET: &str = "voicechat:request_secret";
pub const SECRET: &str = "voicechat:secret";
pub const UPDATE_STATE: &str = "voicechat:update_state";
pub const STATE: &str = "voicechat:state";
pub const STATES: &str = "voicechat:states";

/// Vanilla's play-phase channel announcement: channel names separated by NUL bytes.
pub const MINECRAFT_REGISTER: &str = "minecraft:register";

/// The channels a client says it can receive (capture 02, client -> server `minecraft:register`
/// at 0.53 s, in that order). The server answers `request_secret` only to a client that
/// registered these. Earshot decodes `secret`, `state` and `states`; the rest are for later.
pub const CLIENT_RECEIVES: &[&str] = &[
    "voicechat:add_group",
    "voicechat:state",
    "voicechat:secret",
    "voicechat:joined_group",
    "voicechat:remove_category",
    "voicechat:remove_group",
    "voicechat:add_category",
    "voicechat:remove_state",
    "voicechat:states",
];

/// The channels the server says it receives (capture 02, server -> client `minecraft:register`
/// at 0.49 s). Seeing `request_secret` here is how a client knows the server runs SVC.
pub const SERVER_RECEIVES: &[&str] = &[
    "voicechat:set_group",
    "voicechat:leave_group",
    "voicechat:create_group",
    "voicechat:update_state",
    "voicechat:request_secret",
];

/// Encodes a `minecraft:register` payload.
pub fn encode_register(channels: &[&str]) -> Vec<u8> {
    channels.join("\0").into_bytes()
}

/// Decodes a `minecraft:register` payload into channel names.
pub fn decode_register(data: &[u8]) -> Vec<String> {
    data.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect()
}

/// Whether a server's `minecraft:register` means it runs SVC.
pub fn server_has_voice(registered: &[String]) -> bool {
    registered.iter().any(|c| c == REQUEST_SECRET)
}

/// Client -> server: "give me my voice secret".
#[derive(Debug, Clone, PartialEq)]
pub struct RequestSecret {
    pub compatibility_version: i32,
}

impl RequestSecret {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(self.compatibility_version);
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let msg = Self { compatibility_version: r.i32("compatibility version")? };
        r.finish()?;
        Ok(msg)
    }
}

/// Opus application mode the server asks for (`codec` in its config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Voip,
    Audio,
    RestrictedLowDelay,
}

impl Codec {
    /// Only 0 = VOIP has been observed; 1 and 2 follow the config file's order (a hypothesis).
    fn from_byte(b: u8) -> Result<Self, DecodeError> {
        match b {
            0 => Ok(Codec::Voip),
            1 => Ok(Codec::Audio),
            2 => Ok(Codec::RestrictedLowDelay),
            _ => Err(DecodeError::Invalid("codec")),
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            Codec::Voip => 0,
            Codec::Audio => 1,
            Codec::RestrictedLowDelay => 2,
        }
    }
}

/// Server -> client: the voice secret and the server's voice settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Secret {
    /// AES-128 key for this player's UDP packets.
    pub secret: [u8; 16],
    pub server_port: i32,
    pub player: Uuid,
    pub codec: Codec,
    pub mtu: i32,
    pub voice_distance: f64,
    pub keep_alive_ms: i32,
    pub groups_enabled: bool,
    /// Where to send UDP: `host`, `host:port`, or empty (then the game server's host and `server_port`).
    pub voice_host: String,
    pub recording_allowed: bool,
}

impl Secret {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(&self.secret)
            .i32(self.server_port)
            .uuid(&self.player)
            .u8(self.codec.to_byte())
            .i32(self.mtu)
            .f64(self.voice_distance)
            .i32(self.keep_alive_ms)
            .bool(self.groups_enabled)
            .string(&self.voice_host)
            .bool(self.recording_allowed);
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let msg = Self {
            secret: r.take(16, "secret")?.try_into().unwrap(),
            server_port: r.i32("server port")?,
            player: r.uuid("player")?,
            codec: Codec::from_byte(r.u8("codec")?)?,
            mtu: r.i32("mtu")?,
            voice_distance: r.f64("voice distance")?,
            keep_alive_ms: r.i32("keep-alive")?,
            groups_enabled: r.bool("groups enabled")?,
            voice_host: r.string("voice host")?,
            recording_allowed: r.bool("recording allowed")?,
        };
        r.finish()?;
        Ok(msg)
    }

    /// The UDP address to use, given the host the player joined.
    /// `voice_host` may name a host, a port, or both; whatever it leaves out comes from the game connection.
    /// IPv6 addresses come back without brackets (`::1`), as a socket address lookup wants them.
    pub fn udp_target(&self, game_host: &str) -> (String, u16) {
        let default_port = self.server_port as u16;
        let game_host = unbracket(game_host).to_string();
        let vh = self.voice_host.trim();
        if vh.is_empty() {
            return (game_host, default_port);
        }
        if let Ok(port) = vh.parse::<u16>() {
            return (game_host, port);
        }
        // `[IPv6]:port` or `[IPv6]`.
        if let Some(rest) = vh.strip_prefix('[') {
            if let Some((host, after)) = rest.split_once(']') {
                let port = after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()).unwrap_or(default_port);
                return (host.to_string(), port);
            }
        }
        // `host:port`, but not a bare IPv6 address (which has several colons).
        if let Some((host, port)) = vh.rsplit_once(':') {
            if !host.contains(':') {
                if let Ok(port) = port.parse::<u16>() {
                    return (host.to_string(), port);
                }
            }
        }
        (vh.to_string(), default_port)
    }
}

/// `host` without the brackets an IPv6 address wears beside a port (`[::1]` -> `::1`).
fn unbracket(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// Client -> server: voice chat switched off (`true`) or on (`false`). Muting sends nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateState {
    pub disabled: bool,
}

impl UpdateState {
    pub fn encode(&self) -> Vec<u8> {
        vec![self.disabled as u8]
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let msg = Self { disabled: r.bool("disabled")? };
        r.finish()?;
        Ok(msg)
    }
}

/// One player's voice state, as `state` and `states` carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    pub disabled: bool,
    /// Not connected to the voice server (yet). Hypothesis: flips to false once UDP is validated.
    pub disconnected: bool,
    pub player: Uuid,
    pub name: String,
    /// The group the player is in (capture 06: a bool, then the group's id when true).
    pub group: Option<Uuid>,
}

impl PlayerState {
    fn write(&self, w: &mut Writer) {
        w.bool(self.disabled).bool(self.disconnected).uuid(&self.player).string(&self.name).bool(self.group.is_some());
        if let Some(g) = &self.group {
            w.uuid(g);
        }
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            disabled: r.bool("disabled")?,
            disconnected: r.bool("disconnected")?,
            player: r.uuid("player")?,
            name: r.string("name")?,
            group: if r.bool("has group")? { Some(r.uuid("group")?) } else { None },
        })
    }

    /// `voicechat:state` data: one entry.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let s = Self::read(&mut r)?;
        r.finish()?;
        Ok(s)
    }

    /// `voicechat:states` data: i32 count, then the entries.
    pub fn encode_list(states: &[PlayerState]) -> Vec<u8> {
        let mut w = Writer::new();
        w.i32(states.len() as i32);
        for s in states {
            s.write(&mut w);
        }
        w.into_bytes()
    }

    pub fn decode_list(data: &[u8]) -> Result<Vec<Self>, DecodeError> {
        let mut r = Reader::new(data);
        let n = r.i32("count")?;
        if n < 0 {
            return Err(DecodeError::Invalid("count"));
        }
        // Each entry is at least 20 bytes, so a count beyond that is a lie; don't pre-allocate on it.
        let mut out = Vec::with_capacity((n as usize).min(data.len() / 20));
        for _ in 0..n {
            out.push(Self::read(&mut r)?);
        }
        r.finish()?;
        Ok(out)
    }
}

/// A voice group, as dd_group announces it (capture 06).
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
    pub has_password: bool,
    /// Four more bytes, zero in every capture (group type and flags, probably). Kept as they came.
    pub extra: [u8; 4],
}

impl Group {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.uuid(&self.id).string(&self.name).bool(self.has_password).raw(&self.extra);
        w.into_bytes()
    }

    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        Ok(Self {
            id: r.uuid("group id")?,
            name: r.string("group name")?,
            has_password: r.bool("has password")?,
            extra: r.take(4, "group flags")?.try_into().unwrap(),
        })
    }
}

/// Client -> server oicechat:create_group (capture 06): the name, an optional password, then
/// two bytes that were  0 00 both times (the group's type, probably: SVC's group screen offers
/// several). The creator joins the new group at once.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateGroup {
    pub name: String,
    pub password: Option<String>,
}

impl CreateGroup {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(&self.name).bool(self.password.is_some());
        if let Some(p) = &self.password {
            w.string(p);
        }
        w.raw(&[0, 0]);
        w.into_bytes()
    }
}

/// Client -> server oicechat:set_group: join an existing group.
///
/// **Hypothesis, not yet observed**: the group's id, then an optional password as in
/// create_group. The server's joined_group reply confirms or refutes it (PROTOCOL.md).
#[derive(Debug, Clone, PartialEq)]
pub struct SetGroup {
    pub group: Uuid,
    pub password: Option<String>,
}

impl SetGroup {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.uuid(&self.group).bool(self.password.is_some());
        if let Some(p) = &self.password {
            w.string(p);
        }
        w.into_bytes()
    }
}

/// Server -> client oicechat:joined_group (capture 06): the group we're now in, if any, and
/// a flag that was 0 both times (hypothesis: "wrong password").
#[derive(Debug, Clone, PartialEq)]
pub struct JoinedGroup {
    pub group: Option<Uuid>,
    pub wrong_password: bool,
}

impl JoinedGroup {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(data);
        let group = if r.bool("in a group")? { Some(r.uuid("group")?) } else { None };
        Ok(Self { group, wrong_password: r.bool("wrong password")? })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.bool(self.group.is_some());
        if let Some(g) = &self.group {
            w.uuid(g);
        }
        w.bool(self.wrong_password);
        w.into_bytes()
    }
}

pub const CREATE_GROUP: &str = "voicechat:create_group";
pub const SET_GROUP: &str = "voicechat:set_group";
pub const LEAVE_GROUP: &str = "voicechat:leave_group";
pub const ADD_GROUP: &str = "voicechat:add_group";
pub const REMOVE_GROUP: &str = "voicechat:remove_group";
pub const JOINED_GROUP: &str = "voicechat:joined_group";
/// Server -> client: a player left the server (capture 06, 611.18 s): their UUID.
pub const REMOVE_STATE: &str = "voicechat:remove_state";

/// A decoded server -> client message.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Secret(Secret),
    State(PlayerState),
    States(Vec<PlayerState>),
    AddGroup(Group),
    RemoveGroup(Uuid),
    JoinedGroup(JoinedGroup),
    RemoveState(Uuid),
    /// A `voicechat:` channel Earshot doesn't decode yet.
    Other(String),
}

impl ServerMessage {
    /// Decodes a custom payload, or `None` if the channel isn't SVC's.
    pub fn decode(channel: &str, data: &[u8]) -> Option<Result<Self, DecodeError>> {
        if !channel.starts_with("voicechat:") {
            return None;
        }
        Some(match channel {
            SECRET => Secret::decode(data).map(ServerMessage::Secret),
            STATE => PlayerState::decode(data).map(ServerMessage::State),
            STATES => PlayerState::decode_list(data).map(ServerMessage::States),
            ADD_GROUP => Group::decode(data).map(ServerMessage::AddGroup),
            REMOVE_GROUP => Reader::new(data).uuid("group id").map(ServerMessage::RemoveGroup),
            JOINED_GROUP => JoinedGroup::decode(data).map(ServerMessage::JoinedGroup),
            REMOVE_STATE => Reader::new(data).uuid("player").map(ServerMessage::RemoveState),
            other => Ok(ServerMessage::Other(other.to_string())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::from_hex;
    use std::net::ToSocketAddrs;

    const NULLPORT: &str = "c5c602ae651d38579a04b3e776be1219";
    const AIO: &str = "a1eb0477f0f03c47a9e46bc28a7d9d04";

    #[test]
    fn request_secret_matches_capture() {
        // Capture 03, client -> server: just the compatibility version.
        let data = from_hex("00000014");
        assert_eq!(RequestSecret::decode(&data).unwrap().compatibility_version, COMPATIBILITY_VERSION);
        assert_eq!(RequestSecret { compatibility_version: 20 }.encode(), data);
    }

    #[test]
    fn secret_matches_capture() {
        // Capture 03's voicechat:secret, with the voice host replaced by a placeholder
        // (the real one was the server's own address; the length byte is fixed to match).
        let host = "example.test:24455";
        let data = from_hex(&format!(
            "91f06606a6b37a6a2fe55466a7dec84f 00005f86 {NULLPORT} 00 000004fb 4048000000000000 000003e8 01 {:02x}{} 01",
            host.len(),
            host.bytes().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        let s = Secret::decode(&data).unwrap();
        assert_eq!(s.secret[0], 0x91);
        assert_eq!(s.server_port, 24454);
        assert_eq!(s.player.to_string(), "c5c602ae-651d-3857-9a04-b3e776be1219");
        assert_eq!(s.codec, Codec::Voip);
        assert_eq!(s.mtu, 1275);
        assert_eq!(s.voice_distance, 48.0);
        assert_eq!(s.keep_alive_ms, 1000);
        assert!(s.groups_enabled);
        assert_eq!(s.voice_host, host);
        assert!(s.recording_allowed);
        assert_eq!(s.encode(), data, "re-encoding gives the same bytes");
    }

    #[test]
    fn secret_rejects_trailing_bytes() {
        let mut data = sample_secret("").encode();
        data.push(0);
        assert_eq!(Secret::decode(&data), Err(DecodeError::TrailingBytes(1)));
    }

    fn sample_secret(voice_host: &str) -> Secret {
        Secret {
            secret: [7; 16],
            server_port: 24454,
            player: Uuid([1; 16]),
            codec: Codec::Voip,
            mtu: 1275,
            voice_distance: 48.0,
            keep_alive_ms: 1000,
            groups_enabled: true,
            voice_host: voice_host.into(),
            recording_allowed: true,
        }
    }

    #[test]
    fn udp_target_fills_in_from_the_game_connection() {
        let joined = "mc.example.test";
        assert_eq!(sample_secret("").udp_target(joined), (joined.into(), 24454));
        assert_eq!(sample_secret("24455").udp_target(joined), (joined.into(), 24455));
        assert_eq!(sample_secret("10.0.0.5").udp_target(joined), ("10.0.0.5".into(), 24454));
        assert_eq!(sample_secret("10.0.0.5:24455").udp_target(joined), ("10.0.0.5".into(), 24455));
        assert_eq!(sample_secret("::1").udp_target(joined), ("::1".into(), 24454));
    }

    #[test]
    fn udp_target_takes_bracketed_ipv6() {
        assert_eq!(sample_secret("[::1]:24455").udp_target("x"), ("::1".into(), 24455));
        assert_eq!(sample_secret("[2001:db8::7]").udp_target("x"), ("2001:db8::7".into(), 24454));
        // The joined host may be bracketed too.
        assert_eq!(sample_secret("").udp_target("[::1]"), ("::1".into(), 24454));
        // What comes out resolves as an address.
        let (host, port) = sample_secret("[::1]:24455").udp_target("x");
        assert!((host.as_str(), port).to_socket_addrs().is_ok_and(|mut a| a.next().is_some_and(|a| a.is_ipv6())));
    }

    #[test]
    fn update_state_matches_capture() {
        // Capture 04: 01 after "mark disable", 00 after "mark enable".
        assert!(UpdateState::decode(&[1]).unwrap().disabled);
        assert!(!UpdateState::decode(&[0]).unwrap().disabled);
        assert_eq!(UpdateState { disabled: true }.encode(), [1]);
    }

    #[test]
    fn state_matches_capture() {
        // Capture 04 at 476.29 s: NullPort404 disabled, connected, no group.
        let data = from_hex(&format!("0100 {NULLPORT} 0b 4e756c6c506f7274343034 00"));
        let s = PlayerState::decode(&data).unwrap();
        assert!(s.disabled && !s.disconnected && s.group.is_none());
        assert_eq!(s.name, "NullPort404");
        assert_eq!(s.encode(), data);
    }

    #[test]
    fn states_matches_capture() {
        // Capture 05 at 4.70 s: the list AIOMarcus got on join (both players, AIOMarcus still disconnected).
        let data = from_hex(&format!(
            "00000002 0001 {AIO} 09 41494f4d6172637573 00 0000 {NULLPORT} 0b 4e756c6c506f7274343034 00"
        ));
        let list = PlayerState::decode_list(&data).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].name.as_str(), list[0].disconnected), ("AIOMarcus", true));
        assert_eq!((list[1].name.as_str(), list[1].disconnected), ("NullPort404", false));
        assert_eq!(PlayerState::encode_list(&list), data);
    }

    #[test]
    fn states_rejects_a_lying_count() {
        assert!(PlayerState::decode_list(&from_hex("7fffffff")).is_err());
        assert!(PlayerState::decode_list(&from_hex("ffffffff")).is_err());
    }

    #[test]
    fn register_lists_match_capture() {
        // Capture 02, server -> client minecraft:register at 0.49 s.
        let from_server = b"voicechat:set_group\0voicechat:leave_group\0voicechat:create_group\0voicechat:update_state\0voicechat:request_secret";
        let channels = decode_register(from_server);
        assert_eq!(channels, SERVER_RECEIVES);
        assert!(server_has_voice(&channels));
        assert!(!server_has_voice(&decode_register(b"minecraft:brand\0fabric:registry/sync")));
        assert_eq!(encode_register(SERVER_RECEIVES), from_server);
        assert_eq!(decode_register(&encode_register(CLIENT_RECEIVES)), CLIENT_RECEIVES);
    }

    // Capture 06 (NullPort404 on Java SVC, AIOMarcus on Earshot), exact bytes.
    const GROUP_1: &str = "b055a39643b44bffb4c733305d73d293";

    #[test]
    fn create_group_matches_capture() {
        // 191.69 s: "Test1", no password.
        assert_eq!(CreateGroup { name: "Test1".into(), password: None }.encode(), from_hex("05 5465737431 00 0000"));
        // 213.54 s: "TestPassword" with password "1221".
        assert_eq!(
            CreateGroup { name: "TestPassword".into(), password: Some("1221".into()) }.encode(),
            from_hex("0c 5465737450617373776f7264 01 04 31323231 0000")
        );
    }

    #[test]
    fn add_group_matches_capture() {
        let data = from_hex(&format!("{GROUP_1} 05 5465737431 00 00000000"));
        let g = Group::decode(&data).unwrap();
        assert_eq!((g.id.to_string().as_str(), g.name.as_str(), g.has_password), ("b055a396-43b4-4bff-b4c7-33305d73d293", "Test1", false));
        assert_eq!(g.encode(), data);
        let pw = Group::decode(&from_hex("b001f0e7f366409a819249b20512c3bf 0c 5465737450617373776f7264 01 00000000")).unwrap();
        assert!(pw.has_password);
        assert_eq!(ServerMessage::decode(ADD_GROUP, &data), Some(Ok(ServerMessage::AddGroup(g))));
    }

    #[test]
    fn joined_and_left_match_capture() {
        let joined = from_hex(&format!("01 {GROUP_1} 00"));
        assert_eq!(
            JoinedGroup::decode(&joined).unwrap(),
            JoinedGroup { group: Some(Uuid::parse(GROUP_1).unwrap()), wrong_password: false }
        );
        assert_eq!(JoinedGroup::decode(&from_hex("0000")).unwrap(), JoinedGroup { group: None, wrong_password: false });
        assert_eq!(JoinedGroup { group: Some(Uuid::parse(GROUP_1).unwrap()), wrong_password: false }.encode(), joined);
        assert_eq!(
            ServerMessage::decode(REMOVE_GROUP, &from_hex(GROUP_1)),
            Some(Ok(ServerMessage::RemoveGroup(Uuid::parse(GROUP_1).unwrap())))
        );
    }

    #[test]
    fn a_state_can_carry_a_group() {
        // 191.70 s: NullPort404 now in Test1.
        let data = from_hex(&format!("0000 {NULLPORT} 0b 4e756c6c506f7274343034 01 {GROUP_1}"));
        let s = PlayerState::decode(&data).unwrap();
        assert_eq!(s.group, Some(Uuid::parse(GROUP_1).unwrap()));
        assert_eq!(s.encode(), data);
    }

    #[test]
    fn set_group_hypothesis_encoding() {
        // Not observed: id, then the password as create_group carries it.
        let id = Uuid::parse(GROUP_1).unwrap();
        assert_eq!(SetGroup { group: id, password: None }.encode(), from_hex(&format!("{GROUP_1} 00")));
        assert_eq!(SetGroup { group: id, password: Some("pw".into()) }.encode(), from_hex(&format!("{GROUP_1} 01 02 7077")));
    }

    #[test]
    fn dispatch_by_channel() {
        assert!(ServerMessage::decode("minecraft:brand", &[]).is_none());
        assert_eq!(
            ServerMessage::decode("voicechat:remove_category", &[1, 2]).unwrap().unwrap(),
            ServerMessage::Other("voicechat:remove_category".into())
        );
        let state = PlayerState { disabled: false, disconnected: false, player: Uuid([2; 16]), name: "A".into(), group: None };
        assert_eq!(ServerMessage::decode(STATE, &state.encode()).unwrap().unwrap(), ServerMessage::State(state));
    }
}
