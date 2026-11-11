//! The voice connection's state machine, without I/O (PROTOCOL.md, "Order at join").
//!
//! 1. The server's `minecraft:register` lists `voicechat:request_secret`: we register the
//!    channels we receive and send `request_secret`.
//! 2. `voicechat:secret` arrives: we have a key and a UDP address. Send UDP `Authenticate`
//!    (0x05), once a second until the server's `AuthenticateAck` (0x06).
//! 3. Send `ConnectionCheck` (0x09), once a second until `ConnectionCheckAck` (0x0a).
//! 4. Connected: tell the server our on/off state (`update_state`), answer keep-alives, and give
//!    up if the server goes quiet.
//!
//! The caller passes the time in milliseconds, which keeps this testable without a clock.

use crate::crypto::VoiceKey;
use crate::plugin::{self, RequestSecret, Secret, UpdateState};
use crate::udp::Message;
use crate::wire::Uuid;

/// How often unanswered handshake packets are repeated.
const RETRY_MS: u64 = 1000;
/// Give up on a handshake step after this long.
const HANDSHAKE_TIMEOUT_MS: u64 = 15_000;
/// Connected, but nothing from the server for this long: the connection is gone.
const SILENCE_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send a plugin message over the game connection.
    Plugin { channel: &'static str, data: Vec<u8> },
    /// Send a UDP message to the voice server.
    Udp(Message),
    /// The voice connection is up.
    Connected,
    /// The voice connection failed or was lost; the reason is for logs and the HUD.
    Lost(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The server hasn't said it runs SVC (or we haven't joined one).
    Idle,
    /// `request_secret` sent at `since`.
    Requested { since: u64 },
    Authenticating { since: u64, last_sent: u64 },
    Checking { since: u64, last_sent: u64 },
    Connected { last_heard: u64 },
    /// Gave up. A new join starts over.
    Failed(String),
}

pub struct Session {
    pub phase: Phase,
    secret: Option<Secret>,
    key: Option<VoiceKey>,
    /// Voice chat switched off by the player (sent to the server as `update_state`).
    pub disabled: bool,
}

impl Session {
    pub fn new() -> Self {
        Self { phase: Phase::Idle, secret: None, key: None, disabled: false }
    }

    pub fn secret(&self) -> Option<&Secret> {
        self.secret.as_ref()
    }

    pub fn key(&self) -> Option<&VoiceKey> {
        self.key.as_ref()
    }

    pub fn player(&self) -> Option<Uuid> {
        self.secret.as_ref().map(|s| s.player)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.phase, Phase::Connected { .. })
    }

    /// The server's `minecraft:register`.
    pub fn on_server_register(&mut self, channels: &[String], now: u64) -> Vec<Action> {
        if !plugin::server_has_voice(channels) || !matches!(self.phase, Phase::Idle | Phase::Failed(_)) {
            return vec![];
        }
        self.phase = Phase::Requested { since: now };
        vec![
            Action::Plugin { channel: plugin::MINECRAFT_REGISTER, data: plugin::encode_register(plugin::CLIENT_RECEIVES) },
            request_secret(),
        ]
    }

    /// The server's `voicechat:secret`.
    pub fn on_secret(&mut self, secret: Secret, now: u64) -> Vec<Action> {
        let auth = Message::Authenticate { player: secret.player, secret: secret.secret };
        self.key = Some(VoiceKey::new(&secret.secret));
        self.secret = Some(secret);
        self.phase = Phase::Authenticating { since: now, last_sent: now };
        vec![Action::Udp(auth)]
    }

    /// A UDP message that decrypted with our key.
    pub fn on_udp(&mut self, msg: &Message, now: u64) -> Vec<Action> {
        match (&self.phase, msg) {
            (Phase::Authenticating { .. }, Message::AuthenticateAck) => {
                self.phase = Phase::Checking { since: now, last_sent: now };
                vec![Action::Udp(Message::ConnectionCheck)]
            }
            (Phase::Checking { .. }, Message::ConnectionCheckAck) => {
                self.phase = Phase::Connected { last_heard: now };
                // The real client tells the server its on/off state right after connecting (capture 04, 1.63 s).
                vec![Action::Connected, self.update_state()]
            }
            (Phase::Connected { .. }, msg) => {
                self.phase = Phase::Connected { last_heard: now };
                // The server sends a keep-alive each `keep_alive_ms` and the client answers within
                // about 20 ms (capture 03: s2c at 2.26 s, c2s at 2.28 s).
                if *msg == Message::KeepAlive { vec![Action::Udp(Message::KeepAlive)] } else { vec![] }
            }
            _ => vec![],
        }
    }

    /// Call regularly (every few milliseconds is fine): retries and timeouts.
    ///
    /// `now` may be a little older than the times the other calls were given (another thread
    /// read the clock first), so every difference here saturates instead of wrapping: a wrapped
    /// difference would look like a huge delay and give up at once.
    pub fn poll(&mut self, now: u64) -> Vec<Action> {
        match self.phase.clone() {
            Phase::Requested { since } if now.saturating_sub(since) >= HANDSHAKE_TIMEOUT_MS => {
                self.fail("the server never sent a voice key: its Simple Voice Chat may be a version Earshot doesn't speak (Earshot speaks SVC 2.6, compatibility 20)")
            }
            Phase::Authenticating { since, last_sent } => {
                if now.saturating_sub(since) >= HANDSHAKE_TIMEOUT_MS {
                    return self.fail("the voice server didn't answer (is its UDP port reachable?)");
                }
                if now.saturating_sub(last_sent) < RETRY_MS {
                    return vec![];
                }
                self.phase = Phase::Authenticating { since, last_sent: now };
                let s = self.secret.as_ref().unwrap();
                vec![Action::Udp(Message::Authenticate { player: s.player, secret: s.secret })]
            }
            Phase::Checking { since, last_sent } => {
                if now.saturating_sub(since) >= HANDSHAKE_TIMEOUT_MS {
                    return self.fail("the voice connection check never came back");
                }
                if now.saturating_sub(last_sent) < RETRY_MS {
                    return vec![];
                }
                self.phase = Phase::Checking { since, last_sent: now };
                vec![Action::Udp(Message::ConnectionCheck)]
            }
            Phase::Connected { last_heard } if now.saturating_sub(last_heard) >= SILENCE_TIMEOUT_MS => {
                self.fail("lost the voice server")
            }
            _ => vec![],
        }
    }

    /// The player switched voice chat on or off.
    pub fn set_disabled(&mut self, disabled: bool) -> Vec<Action> {
        if self.disabled == disabled {
            return vec![];
        }
        self.disabled = disabled;
        if self.is_connected() { vec![self.update_state()] } else { vec![] }
    }

    /// Left the server.
    pub fn reset(&mut self) {
        let disabled = self.disabled;
        *self = Self::new();
        self.disabled = disabled;
    }

    fn update_state(&self) -> Action {
        Action::Plugin { channel: plugin::UPDATE_STATE, data: UpdateState { disabled: self.disabled }.encode() }
    }

    /// Gives up on this connection (also used by the engine when the UDP socket can't open).
    pub fn fail(&mut self, why: &str) -> Vec<Action> {
        self.phase = Phase::Failed(why.into());
        self.key = None;
        vec![Action::Lost(why.into())]
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

fn request_secret() -> Action {
    Action::Plugin {
        channel: plugin::REQUEST_SECRET,
        data: RequestSecret { compatibility_version: plugin::COMPATIBILITY_VERSION }.encode(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Codec;

    fn svc_server() -> Vec<String> {
        plugin::SERVER_RECEIVES.iter().map(|s| s.to_string()).collect()
    }

    fn secret() -> Secret {
        Secret {
            secret: [5; 16],
            server_port: 24454,
            player: Uuid([1; 16]),
            codec: Codec::Voip,
            mtu: 1275,
            voice_distance: 48.0,
            keep_alive_ms: 1000,
            groups_enabled: true,
            voice_host: String::new(),
            recording_allowed: true,
        }
    }

    fn connected() -> Session {
        let mut s = Session::new();
        s.on_server_register(&svc_server(), 0);
        s.on_secret(secret(), 100);
        s.on_udp(&Message::AuthenticateAck, 150);
        s.on_udp(&Message::ConnectionCheckAck, 200);
        assert!(s.is_connected());
        s
    }

    #[test]
    fn a_server_without_svc_is_left_alone() {
        let mut s = Session::new();
        assert!(s.on_server_register(&["minecraft:brand".into()], 0).is_empty());
        assert_eq!(s.phase, Phase::Idle);
    }

    #[test]
    fn the_handshake_follows_the_captured_order() {
        let mut s = Session::new();
        let first = s.on_server_register(&svc_server(), 0);
        assert_eq!(first.len(), 2);
        assert!(matches!(&first[0], Action::Plugin { channel: plugin::MINECRAFT_REGISTER, .. }));
        assert_eq!(first[1], Action::Plugin { channel: plugin::REQUEST_SECRET, data: vec![0, 0, 0, 20] });

        assert_eq!(s.on_secret(secret(), 100), [Action::Udp(Message::Authenticate { player: Uuid([1; 16]), secret: [5; 16] })]);
        assert_eq!(s.on_udp(&Message::AuthenticateAck, 150), [Action::Udp(Message::ConnectionCheck)]);
        assert_eq!(
            s.on_udp(&Message::ConnectionCheckAck, 200),
            [Action::Connected, Action::Plugin { channel: plugin::UPDATE_STATE, data: vec![0] }]
        );
    }

    #[test]
    fn registering_twice_does_not_request_twice() {
        let mut s = Session::new();
        s.on_server_register(&svc_server(), 0);
        assert!(s.on_server_register(&svc_server(), 10).is_empty());
    }

    #[test]
    fn authenticate_is_retried_each_second_then_given_up() {
        let mut s = Session::new();
        s.on_server_register(&svc_server(), 0);
        s.on_secret(secret(), 0);
        assert!(s.poll(500).is_empty());
        assert!(matches!(s.poll(1000)[..], [Action::Udp(Message::Authenticate { .. })]));
        assert!(s.poll(1500).is_empty());
        assert!(matches!(s.poll(2000)[..], [Action::Udp(Message::Authenticate { .. })]));
        assert!(matches!(s.poll(15_000)[..], [Action::Lost(_)]));
        assert!(s.key().is_none());
    }

    #[test]
    fn a_clock_reading_from_just_before_doesnt_time_out() {
        // The worker read the clock at 999, then the game thread handled the secret at 1000.
        // 999 - 1000 must not wrap into "a very long time ago" (the bug seen in game).
        let mut s = Session::new();
        s.on_server_register(&svc_server(), 990);
        s.on_secret(secret(), 1000);
        assert!(s.poll(999).is_empty());
        assert!(matches!(s.phase, Phase::Authenticating { .. }));
        let mut c = connected();
        assert!(c.poll(100).is_empty(), "connected at 200, polled with 100");
        assert!(c.is_connected());
    }

    #[test]
    fn the_connection_check_is_retried_too() {
        let mut s = Session::new();
        s.on_secret(secret(), 0);
        s.on_udp(&Message::AuthenticateAck, 0);
        assert_eq!(s.poll(1000), [Action::Udp(Message::ConnectionCheck)]);
    }

    #[test]
    fn keep_alives_are_answered() {
        let mut s = connected();
        assert_eq!(s.on_udp(&Message::KeepAlive, 1200), [Action::Udp(Message::KeepAlive)]);
    }

    #[test]
    fn silence_from_the_server_loses_the_connection() {
        let mut s = connected();
        assert!(s.poll(9_000).is_empty());
        s.on_udp(&Message::KeepAlive, 9_000); // heard: the clock restarts
        assert!(s.poll(18_000).is_empty());
        assert!(matches!(s.poll(19_000)[..], [Action::Lost(_)]));
    }

    #[test]
    fn toggling_voice_sends_update_state_only_when_connected() {
        let mut s = Session::new();
        assert!(s.set_disabled(true).is_empty(), "not connected yet");
        let mut s = connected();
        assert_eq!(s.set_disabled(true), [Action::Plugin { channel: plugin::UPDATE_STATE, data: vec![1] }]);
        assert!(s.set_disabled(true).is_empty(), "no change, nothing sent");
        assert_eq!(s.set_disabled(false), [Action::Plugin { channel: plugin::UPDATE_STATE, data: vec![0] }]);
    }

    #[test]
    fn reset_keeps_the_on_off_choice() {
        let mut s = connected();
        s.set_disabled(true);
        s.reset();
        assert_eq!(s.phase, Phase::Idle);
        assert!(s.disabled);
    }

    #[test]
    fn after_a_failure_a_new_register_starts_over() {
        let mut s = Session::new();
        s.on_server_register(&svc_server(), 0);
        s.poll(20_000);
        assert!(matches!(s.phase, Phase::Failed(_)));
        assert_eq!(s.on_server_register(&svc_server(), 20_001).len(), 2);
    }
}
