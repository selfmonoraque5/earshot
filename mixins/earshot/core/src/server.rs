//! A voice server speaking SVC's protocol, for a Horizon dedicated server (or a LAN host), so
//! Earshot players, and Java players running Simple Voice Chat, can talk on a Horizon server.
//!
//! It is the server side of what PROTOCOL.md describes, built from the same observations:
//! - over the game connection, a player's `request_secret` gets a fresh random `secret` (and
//!   the list of everyone's voice state); `update_state` switches a player's voice on or off;
//! - over UDP: `Authenticate` is acknowledged when its secret matches, `ConnectionCheck` makes
//!   the player connected, keep-alives go out every `keep_alive_ms`, and each microphone frame
//!   is forwarded, re-sealed with each listener's own key, to the connected players in the
//!   same dimension within the voice distance (as the SVC server was seen to do, capture 05).
//!
//! The game feeds it player positions each tick and carries its plugin messages; this type
//! owns the UDP socket and its thread.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::crypto::VoiceKey;
use crate::plugin::{self, Codec, PlayerState, RequestSecret, Secret, UpdateState};
use crate::udp::{self, Message, PlayerSound};
use crate::wire::Uuid;

/// A player who hasn't been heard from for this long is no longer connected.
const SILENCE_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    /// UDP address to bind (`0.0.0.0:24454` by default; port 0 picks a free one).
    pub bind: SocketAddr,
    pub max_distance: f64,
    pub keep_alive_ms: u32,
    /// What clients are told to connect to (`voice_host`); empty = the game server's host.
    pub voice_host: String,
    pub mtu: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:24454".parse().unwrap(),
            max_distance: 48.0,
            keep_alive_ms: 1000,
            voice_host: String::new(),
            mtu: 1275,
        }
    }
}

struct Peer {
    name: String,
    secret: [u8; 16],
    key: VoiceKey,
    addr: Option<SocketAddr>,
    connected: bool,
    disabled: bool,
    last_heard: u64,
    last_keep_alive: u64,
    dimension: String,
    position: Option<[f64; 3]>,
}

impl Peer {
    fn state(&self, id: Uuid) -> PlayerState {
        PlayerState { disabled: self.disabled, disconnected: !self.connected, player: id, name: self.name.clone(), group: None }
    }
}

struct Inner {
    peers: HashMap<Uuid, Peer>,
    /// Plugin messages for the game to send: (player, channel, data).
    outbox: Vec<(Uuid, &'static str, Vec<u8>)>,
    config: ServerConfig,
    port: u16,
}

impl Inner {
    /// Tells every player with a secret about `id`'s voice state.
    fn broadcast_state(&mut self, id: Uuid) {
        let Some(p) = self.peers.get(&id) else { return };
        let data = p.state(id).encode();
        let to: Vec<Uuid> = self.peers.keys().copied().collect();
        for t in to {
            self.outbox.push((t, plugin::STATE, data.clone()));
        }
    }
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
    start: Instant,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

pub struct VoiceServer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceServer {
    /// Binds the UDP socket and starts serving.
    pub fn start(config: ServerConfig) -> io::Result<VoiceServer> {
        let socket = UdpSocket::bind(config.bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        let port = socket.local_addr()?.port();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner { peers: HashMap::new(), outbox: Vec::new(), config, port }),
            stop: AtomicBool::new(false),
            start: Instant::now(),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("earshot-voice-server".into()).spawn(move || serve(s, socket))?;
        Ok(VoiceServer { shared, thread: Some(thread) })
    }

    /// The UDP port actually in use.
    pub fn port(&self) -> u16 {
        self.shared.lock().port
    }

    /// The `minecraft:register` payload a server sends a joining player, naming the channels
    /// it takes (as SVC's server does, capture 02): it tells clients this server has voice.
    pub fn register_payload() -> Vec<u8> {
        plugin::encode_register(plugin::SERVER_RECEIVES)
    }

    /// A `voicechat:` plugin message from a player.
    pub fn on_plugin(&self, player: Uuid, name: &str, channel: &str, data: &[u8]) {
        let mut inner = self.shared.lock();
        match channel {
            plugin::REQUEST_SECRET => {
                match RequestSecret::decode(data) {
                    Ok(r) if r.compatibility_version == plugin::COMPATIBILITY_VERSION => {}
                    Ok(r) => {
                        eprintln!(
                            "[earshot] {name} has voice chat compatibility {} (this server speaks {}): not sending a key",
                            r.compatibility_version,
                            plugin::COMPATIBILITY_VERSION
                        );
                        return;
                    }
                    Err(_) => return,
                }
                let mut secret = [0u8; 16];
                if getrandom::fill(&mut secret).is_err() {
                    eprintln!("[earshot] no random bytes for {name}'s voice key");
                    return;
                }
                let disabled = inner.peers.get(&player).is_some_and(|p| p.disabled);
                let (dimension, position) =
                    inner.peers.get(&player).map(|p| (p.dimension.clone(), p.position)).unwrap_or_default();
                inner.peers.insert(
                    player,
                    Peer {
                        name: name.to_string(),
                        secret,
                        key: VoiceKey::new(&secret),
                        addr: None,
                        connected: false,
                        disabled,
                        last_heard: 0,
                        last_keep_alive: 0,
                        dimension,
                        position,
                    },
                );
                let cfg = &inner.config;
                let msg = Secret {
                    secret,
                    server_port: inner.port as i32,
                    player,
                    codec: Codec::Voip,
                    mtu: cfg.mtu,
                    voice_distance: cfg.max_distance,
                    keep_alive_ms: cfg.keep_alive_ms as i32,
                    groups_enabled: false,
                    voice_host: cfg.voice_host.clone(),
                    recording_allowed: false,
                };
                inner.outbox.push((player, plugin::SECRET, msg.encode()));
                // Everyone's state for the newcomer (the SVC server sends `states` with the secret,
                // capture 05), then the newcomer's for everyone.
                let states: Vec<PlayerState> = inner.peers.iter().map(|(id, p)| p.state(*id)).collect();
                inner.outbox.push((player, plugin::STATES, PlayerState::encode_list(&states)));
                inner.broadcast_state(player);
            }
            plugin::UPDATE_STATE => {
                let Ok(u) = UpdateState::decode(data) else { return };
                if let Some(p) = inner.peers.get_mut(&player) {
                    p.disabled = u.disabled;
                    inner.broadcast_state(player);
                }
            }
            _ => {}
        }
    }

    /// The players online, with where they are. Players no longer listed are forgotten.
    pub fn set_players(&self, players: impl IntoIterator<Item = (Uuid, String, [f64; 3])>) {
        let mut inner = self.shared.lock();
        let mut seen = Vec::new();
        for (id, dimension, position) in players {
            seen.push(id);
            if let Some(p) = inner.peers.get_mut(&id) {
                p.dimension = dimension;
                p.position = Some(position);
            }
        }
        inner.peers.retain(|id, _| seen.contains(id));
    }

    /// Plugin messages to send: (player, channel, data).
    pub fn take_outgoing(&self) -> Vec<(Uuid, &'static str, Vec<u8>)> {
        std::mem::take(&mut self.shared.lock().outbox)
    }

    /// Who's connected to voice (for logs and tests).
    pub fn connected(&self) -> Vec<Uuid> {
        self.shared.lock().peers.iter().filter(|(_, p)| p.connected).map(|(id, _)| *id).collect()
    }
}

impl Drop for VoiceServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn send(socket: &UdpSocket, key: &VoiceKey, addr: SocketAddr, msg: &Message) {
    if let Ok(packet) = udp::seal_to_client(key, msg) {
        let _ = socket.send_to(&packet, addr);
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn serve(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = vec![0u8; 4096];
    while !shared.stop.load(Ordering::Relaxed) {
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            handle(&shared, &socket, &buf[..n], from);
        }

        // Keep-alives, and players who went quiet.
        let now = shared.now();
        let mut inner = shared.lock();
        let interval = inner.config.keep_alive_ms as u64;
        let mut lost = Vec::new();
        for (id, p) in inner.peers.iter_mut() {
            if !p.connected {
                continue;
            }
            if now.saturating_sub(p.last_heard) >= SILENCE_TIMEOUT_MS {
                p.connected = false;
                lost.push(*id);
                continue;
            }
            if now.saturating_sub(p.last_keep_alive) >= interval {
                p.last_keep_alive = now;
                if let Some(addr) = p.addr {
                    send(&socket, &p.key, addr, &Message::KeepAlive);
                }
            }
        }
        for id in lost {
            inner.broadcast_state(id);
        }
    }
}

fn handle(shared: &Shared, socket: &UdpSocket, packet: &[u8], from: SocketAddr) {
    let Ok((sender, sealed)) = udp::split_from_client(packet) else { return };
    let now = shared.now();
    let mut inner = shared.lock();
    let Some(peer) = inner.peers.get(&sender) else { return };
    let Ok(plain) = peer.key.open(sealed) else { return };
    let Ok(msg) = Message::decode(&plain) else { return };

    match msg {
        Message::Authenticate { player, secret } => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if player == sender && secret == p.secret {
                p.addr = Some(from);
                p.last_heard = now;
                send(socket, &p.key, from, &Message::AuthenticateAck);
            }
        }
        Message::ConnectionCheck => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr == Some(from) {
                let newly = !p.connected;
                p.connected = true;
                p.last_heard = now;
                p.last_keep_alive = now;
                send(socket, &p.key, from, &Message::ConnectionCheckAck);
                if newly {
                    eprintln!("[earshot] {} connected to voice chat", p.name);
                    inner.broadcast_state(sender);
                }
            }
        }
        Message::KeepAlive => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr == Some(from) {
                p.last_heard = now;
            }
        }
        Message::Mic(frame) => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr != Some(from) || !p.connected || p.disabled {
                return;
            }
            p.last_heard = now;
            let (Some(pos), dim) = (p.position, p.dimension.clone()) else { return };
            let range = inner.config.max_distance;
            let sound = Message::PlayerSound(PlayerSound {
                channel: sender,
                sender,
                opus: frame.opus,
                sequence: frame.sequence,
                distance: range as f32,
                flag: frame.flag,
            });
            for (id, l) in inner.peers.iter() {
                if *id == sender || !l.connected || l.disabled || l.dimension != dim {
                    continue;
                }
                let (Some(lpos), Some(addr)) = (l.position, l.addr) else { continue };
                if distance(pos, lpos) <= range {
                    send(socket, &l.key, addr, &sound);
                }
            }
        }
        // Server-to-client messages coming the wrong way: ignore.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Settings, VoiceClient};
    use crate::udp::MicFrame;

    fn server() -> VoiceServer {
        VoiceServer::start(ServerConfig { bind: "127.0.0.1:0".parse().unwrap(), ..ServerConfig::default() }).unwrap()
    }

    /// A hand-driven client: its socket and key.
    struct Raw {
        id: Uuid,
        key: VoiceKey,
        socket: UdpSocket,
    }

    impl Raw {
        fn send(&self, msg: &Message) {
            self.socket.send(&udp::seal_to_server(&self.key, &self.id, msg).unwrap()).unwrap();
        }

        fn recv(&self) -> Option<Message> {
            let mut buf = [0u8; 4096];
            let n = self.socket.recv(&mut buf).ok()?;
            udp::open_from_server(&self.key, &buf[..n]).ok()
        }

        /// The next message that isn't a keep-alive.
        fn recv_not_keep_alive(&self) -> Option<Message> {
            for _ in 0..50 {
                match self.recv()? {
                    Message::KeepAlive => continue,
                    m => return Some(m),
                }
            }
            None
        }
    }

    /// Joins `name` at `pos` and runs the whole handshake by hand.
    fn join(srv: &VoiceServer, n: u8, dim: &str, pos: [f64; 3], everyone: &mut Vec<(Uuid, String, [f64; 3])>) -> Raw {
        let id = Uuid([n; 16]);
        everyone.push((id, dim.to_string(), pos));
        srv.on_plugin(id, &format!("P{n}"), plugin::REQUEST_SECRET, &RequestSecret { compatibility_version: 20 }.encode());
        srv.set_players(everyone.clone());
        let out = srv.take_outgoing();
        let secret = out.iter().find(|(to, ch, _)| *to == id && *ch == plugin::SECRET).expect("a secret");
        let secret = Secret::decode(&secret.2).unwrap();
        assert_eq!(secret.player, id);
        assert_eq!(secret.server_port, srv.port() as i32);

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(("127.0.0.1", srv.port())).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let raw = Raw { id, key: VoiceKey::new(&secret.secret), socket };
        raw.send(&Message::Authenticate { player: id, secret: secret.secret });
        assert_eq!(raw.recv_not_keep_alive(), Some(Message::AuthenticateAck));
        raw.send(&Message::ConnectionCheck);
        assert_eq!(raw.recv_not_keep_alive(), Some(Message::ConnectionCheckAck));
        raw
    }

    fn mic(seq: u64) -> Message {
        Message::Mic(MicFrame { opus: vec![0x78, seq as u8, 1, 2], sequence: seq, flag: 0 })
    }

    #[test]
    fn hands_out_a_secret_and_states_and_announces_itself() {
        let srv = server();
        let id = Uuid([1; 16]);
        srv.set_players([(id, "overworld".into(), [0.0; 3])]);
        srv.on_plugin(id, "Alice", plugin::REQUEST_SECRET, &RequestSecret { compatibility_version: 20 }.encode());
        let out = srv.take_outgoing();
        let channels: Vec<&str> = out.iter().map(|(_, c, _)| *c).collect();
        assert_eq!(channels, [plugin::SECRET, plugin::STATES, plugin::STATE]);
        let states = PlayerState::decode_list(&out[1].2).unwrap();
        assert_eq!(states[0].name, "Alice");
        assert!(states[0].disconnected);
        assert!(plugin::server_has_voice(&plugin::decode_register(&VoiceServer::register_payload())));
    }

    #[test]
    fn refuses_another_compatibility_version() {
        let srv = server();
        srv.on_plugin(Uuid([1; 16]), "Old", plugin::REQUEST_SECRET, &RequestSecret { compatibility_version: 19 }.encode());
        assert!(srv.take_outgoing().is_empty());
    }

    #[test]
    fn a_wrong_secret_gets_no_ack() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let impostor = Raw { id: a.id, key: VoiceKey::new(&[0; 16]), socket: UdpSocket::bind("127.0.0.1:0").unwrap() };
        impostor.socket.connect(("127.0.0.1", srv.port())).unwrap();
        impostor.socket.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        impostor.send(&Message::Authenticate { player: a.id, secret: [0; 16] });
        assert_eq!(impostor.recv(), None);
    }

    #[test]
    fn forwards_voice_to_players_in_range_only() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0, 64.0, 0.0], &mut all);
        let near = join(&srv, 2, "overworld", [10.0, 64.0, 0.0], &mut all);
        let far = join(&srv, 3, "overworld", [100.0, 64.0, 0.0], &mut all);
        let nether = join(&srv, 4, "the_nether", [1.0, 64.0, 0.0], &mut all);
        srv.set_players(all.clone());
        assert_eq!(srv.connected().len(), 4);

        a.send(&mic(7));
        let Some(Message::PlayerSound(s)) = near.recv_not_keep_alive() else { panic!("near player heard nothing") };
        assert_eq!((s.sender, s.channel, s.sequence, s.distance), (a.id, a.id, 7, 48.0));
        assert_eq!(s.opus, [0x78, 7, 1, 2], "forwarded as sent");
        assert_eq!(far.recv_not_keep_alive(), None, "beyond 48 blocks");
        assert_eq!(nether.recv_not_keep_alive(), None, "another dimension");
        assert_eq!(a.recv_not_keep_alive(), None, "not echoed back");
    }

    #[test]
    fn a_disabled_player_is_neither_heard_nor_sent_to() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        srv.on_plugin(a.id, "P1", plugin::UPDATE_STATE, &UpdateState { disabled: true }.encode());
        a.send(&mic(1));
        assert_eq!(b.recv_not_keep_alive(), None);
        b.send(&mic(2));
        assert_eq!(a.recv_not_keep_alive(), None);
        // And everyone heard that A switched off.
        let states: Vec<PlayerState> = srv
            .take_outgoing()
            .into_iter()
            .filter(|(_, c, _)| *c == plugin::STATE)
            .map(|(_, _, d)| PlayerState::decode(&d).unwrap())
            .collect();
        assert!(states.iter().any(|s| s.player == a.id && s.disabled));
    }

    #[test]
    fn players_who_leave_are_forgotten() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        srv.set_players([all[1].clone()]); // A left
        assert_eq!(srv.connected(), [b.id]);
        a.send(&mic(1));
        assert_eq!(b.recv_not_keep_alive(), None);
    }

    #[test]
    fn sends_keep_alives() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let mut got = false;
        for _ in 0..8 {
            if a.recv() == Some(Message::KeepAlive) {
                got = true;
                break;
            }
        }
        assert!(got, "a keep-alive within about 2 s");
    }

    #[test]
    fn earshots_own_client_connects_to_it() {
        // The real client engine against this server over localhost UDP, with the test carrying
        // the plugin messages both ways as the game would.
        let srv = server();
        let me = Uuid([9; 16]);
        srv.set_players([(me, "overworld".into(), [0.0; 3])]);
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        client.on_plugin(plugin::MINECRAFT_REGISTER, &VoiceServer::register_payload(), "127.0.0.1");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.status().connected {
            assert!(Instant::now() < deadline, "client didn't connect: {}", client.status().connection);
            for (ch, data) in client.take_outgoing() {
                srv.on_plugin(me, "Me", ch, &data);
            }
            for (_, ch, data) in srv.take_outgoing() {
                client.on_plugin(ch, &data, "127.0.0.1");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(srv.connected(), [me]);
    }
}
