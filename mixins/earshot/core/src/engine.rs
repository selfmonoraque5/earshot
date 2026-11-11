//! The voice client: everything between the game and the network/sound card.
//!
//! The game calls in from its own thread with cheap, non-blocking calls: plugin messages it
//! receives ([`VoiceClient::on_plugin`]), the listener and player positions plus the push-to-talk
//! key once a tick ([`VoiceClient::tick`]), and it sends whatever [`VoiceClient::take_outgoing`]
//! returns over the game connection.
//!
//! One worker thread does the rest: the UDP socket, the handshake's retries, encoding the
//! microphone and keeping the speakers fed from the [`Mixer`]. Audio devices are opened by the
//! worker too, so the game never waits on them.

use std::collections::VecDeque;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::activation::{self, Activation, Mode};
use crate::codec::{Encoder, FRAME_SAMPLES};
use crate::mixer::Mixer;
use crate::plugin::{self, Secret, ServerMessage};
use crate::session::{Action, Phase, Session};
use crate::spatial::Listener;
use crate::udp::{self, Message, MicFrame};
use crate::wire::Uuid;

/// Most speaker audio queued (100 ms): past this the oldest is dropped, bounding the delay.
const PLAYBACK_LIMIT: usize = 5 * FRAME_SAMPLES;

/// Longest the worker spends receiving before it handles the microphone and speakers (half a
/// 20 ms frame, so neither falls behind however busy the voice server is).
const RECEIVE_BATCH: Duration = Duration::from_millis(10);

/// Player choices that affect the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// `None` = the system default.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub mic_volume: f32,
    pub output_volume: f32,
    pub mode: Mode,
    /// Clean the microphone (high-pass + adaptive noise gate, [crate::dsp]).
    pub noise_suppression: bool,
    /// Open the sound card. Off only in tests, which have no audio hardware to rely on.
    pub use_audio_devices: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { input_device: None, output_device: None, mic_volume: 1.0, output_volume: 1.0, mode: Mode::PushToTalk, noise_suppression: true, use_audio_devices: true }
    }
}

/// What the HUD and settings page show.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// A few words on the connection ("connected", "no voice server", the failure reason…).
    pub connection: String,
    pub connected: bool,
    /// Our microphone is being sent right now.
    pub transmitting: bool,
    /// Players we heard in the last 200 ms.
    pub talking: Vec<Uuid>,
    /// Our own player, once the server sent the voice key.
    pub me: Option<Uuid>,
    pub muted: bool,
    pub deafened: bool,
    pub disabled: bool,
    /// Microphone level of the latest frame, dBFS.
    pub input_level_dbfs: f32,
    pub input_device: String,
    pub output_device: String,
    /// Microphone frames sent and other players' voice frames received, this connection.
    pub mic_frames_sent: u64,
    pub voice_frames_received: u64,
    /// Voice groups on the server, the one we're in, and the last problem joining one.
    pub groups: Vec<GroupInfo>,
    pub my_group: Option<GroupInfo>,
    pub group_problem: Option<String>,
    /// UDP from the server by message type ("08x52 02x0"), unreadable packets, and the last problem.
    pub udp_summary: String,
}

/// A group as the menu shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupInfo {
    pub id: Uuid,
    pub name: String,
    pub has_password: bool,
    /// Players in it, as the server's states say.
    pub members: Vec<(Uuid, String)>,
}

struct Inner {
    groups: Vec<plugin::Group>,
    my_group: Option<Uuid>,
    /// Each player's name and group, from `state`/`states`.
    players: std::collections::HashMap<Uuid, (String, Option<Uuid>)>,
    group_problem: Option<String>,
    session: Session,
    mixer: Mixer,
    outbox: Vec<(&'static str, Vec<u8>)>,
    /// `host:port` from the secret, resolved by the worker.
    target: Option<(String, u16)>,
    socket: Option<UdpSocket>,
    encoder: Option<Encoder>,
    sequence: u64,
    activation: Activation,
    suppressor: crate::dsp::NoiseSuppressor,
    settings: Settings,
    reopen_audio: bool,
    ptt_down: bool,
    muted: bool,
    deafened: bool,
    transmitting: bool,
    level: f32,
    input_device: String,
    output_device: String,
    /// For the status line: UDP packets sent and received, and the last socket error.
    udp_sent: u64,
    udp_received: u64,
    udp_error: Option<String>,
    mic_frames_sent: u64,
    voice_frames_received: u64,
    /// Received UDP messages by type byte, and packets that didn't open or decode.
    udp_types: [u64; 256],
    udp_unreadable: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
    start: Instant,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind; carry on with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

pub struct VoiceClient {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl VoiceClient {
    pub fn new(settings: Settings) -> Self {
        let inner = Inner {
            groups: Vec::new(),
            my_group: None,
            players: std::collections::HashMap::new(),
            group_problem: None,
            session: Session::new(),
            mixer: Mixer::new(48.0),
            outbox: Vec::new(),
            target: None,
            socket: None,
            encoder: None,
            sequence: 0,
            activation: Activation::new(settings.mode),
            suppressor: crate::dsp::NoiseSuppressor::new(),
            settings,
            reopen_audio: false,
            ptt_down: false,
            muted: false,
            deafened: false,
            transmitting: false,
            level: -127.0,
            input_device: String::new(),
            output_device: String::new(),
            udp_sent: 0,
            udp_received: 0,
            udp_error: None,
            mic_frames_sent: 0,
            voice_frames_received: 0,
            udp_types: [0; 256],
            udp_unreadable: 0,
        };
        let shared = Arc::new(Shared { inner: Mutex::new(inner), stop: AtomicBool::new(false), start: Instant::now() });
        let s = shared.clone();
        let worker = std::thread::Builder::new().name("earshot-voice".into()).spawn(move || worker(s)).ok();
        Self { shared, worker }
    }

    /// A custom payload from the server, any channel. `game_host` is the host the player joined,
    /// used when the server's `voice_host` doesn't name one.
    pub fn on_plugin(&self, channel: &str, data: &[u8], game_host: &str) {
        let now = self.shared.now();
        let mut inner = self.shared.lock();
        let actions = if channel == plugin::MINECRAFT_REGISTER {
            inner.session.on_server_register(&plugin::decode_register(data), now)
        } else {
            match ServerMessage::decode(channel, data) {
                Some(Ok(ServerMessage::Secret(secret))) => on_secret(&mut inner, secret, game_host, now),
                Some(Ok(ServerMessage::State(st))) => {
                    inner.players.insert(st.player, (st.name, st.group));
                    vec![]
                }
                Some(Ok(ServerMessage::States(list))) => {
                    for st in list {
                        inner.players.insert(st.player, (st.name, st.group));
                    }
                    vec![]
                }
                Some(Ok(ServerMessage::RemoveState(id))) => {
                    inner.players.remove(&id);
                    vec![]
                }
                Some(Ok(ServerMessage::AddGroup(g))) => {
                    inner.groups.retain(|x| x.id != g.id);
                    inner.groups.push(g);
                    vec![]
                }
                Some(Ok(ServerMessage::RemoveGroup(id))) => {
                    inner.groups.retain(|x| x.id != id);
                    vec![]
                }
                Some(Ok(ServerMessage::JoinedGroup(j))) => {
                    inner.my_group = j.group;
                    inner.group_problem = j.wrong_password.then(|| "wrong password".to_string());
                    vec![]
                }
                Some(Err(e)) => {
                    eprintln!("[earshot] couldn't read {channel}: {e}");
                    vec![]
                }
                // `state`/`states`/groups: not needed for proximity voice yet.
                _ => vec![],
            }
        };
        run_actions(&mut inner, actions);
    }

    /// Once a game tick: where we listen from, where the other players are, and the
    /// push-to-talk key.
    pub fn tick(&self, listener: Option<Listener>, players: Vec<(Uuid, [f64; 3])>, ptt_down: bool) {
        let mut inner = self.shared.lock();
        inner.mixer.set_listener(listener);
        inner.mixer.set_positions(players);
        inner.ptt_down = ptt_down;
    }

    /// Plugin messages to send over the game connection now.
    pub fn take_outgoing(&self) -> Vec<(&'static str, Vec<u8>)> {
        std::mem::take(&mut self.shared.lock().outbox)
    }

    pub fn set_muted(&self, muted: bool) {
        self.shared.lock().muted = muted;
    }

    pub fn set_deafened(&self, deafened: bool) {
        let mut inner = self.shared.lock();
        inner.deafened = deafened;
        inner.mixer.deafened = deafened;
    }

    /// Voice chat off entirely: the server is told, as SVC's own toggle does.
    pub fn set_disabled(&self, disabled: bool) {
        let mut inner = self.shared.lock();
        let actions = inner.session.set_disabled(disabled);
        inner.mixer.deafened = disabled || inner.deafened;
        run_actions(&mut inner, actions);
    }

    /// Makes a group and joins it (`create_group`, capture 06).
    pub fn create_group(&self, name: &str, password: Option<&str>) {
        let data = plugin::CreateGroup { name: name.to_string(), password: password.map(str::to_string) }.encode();
        self.send_when_connected(plugin::CREATE_GROUP, data);
    }

    /// Joins a group by id (`set_group`, capture 06).
    pub fn join_group(&self, group: Uuid, password: Option<&str>) {
        self.shared.lock().group_problem = None;
        let data = plugin::SetGroup { group, password: password.map(str::to_string) }.encode();
        self.send_when_connected(plugin::SET_GROUP, data);
    }

    /// Leaves our group (`leave_group`: no data, capture 06).
    pub fn leave_group(&self) {
        self.send_when_connected(plugin::LEAVE_GROUP, Vec::new());
    }

    fn send_when_connected(&self, channel: &'static str, data: Vec<u8>) {
        let mut inner = self.shared.lock();
        if inner.session.is_connected() {
            inner.outbox.push((channel, data));
        }
    }

    pub fn set_player_volume(&self, player: Uuid, volume: f32) {
        self.shared.lock().mixer.player_volume.insert(player, volume.max(0.0));
    }

    pub fn apply_settings(&self, settings: Settings) {
        let mut inner = self.shared.lock();
        if settings.input_device != inner.settings.input_device || settings.output_device != inner.settings.output_device {
            inner.reopen_audio = true;
        }
        inner.activation.mode = settings.mode;
        inner.mixer.volume = settings.output_volume;
        inner.settings = settings;
    }

    pub fn status(&self) -> Status {
        let inner = self.shared.lock();
        let connection = match &inner.session.phase {
            Phase::Idle => "no voice server".to_string(),
            Phase::Requested { .. } => "asking for a voice key".to_string(),
            Phase::Authenticating { .. } => "logging in to voice".to_string(),
            Phase::Checking { .. } => "checking the connection".to_string(),
            Phase::Connected { .. } => "connected".to_string(),
            Phase::Failed(why) => why.clone(),
        };
        let connection = match (&inner.session.phase, &inner.target) {
            (Phase::Idle | Phase::Requested { .. } | Phase::Connected { .. }, _) => connection,
            (_, target) => {
                let to = target.as_ref().map(|(h, p)| format!("{h}:{p}")).unwrap_or_else(|| "no address".into());
                let err = inner.udp_error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default();
                format!("{connection} [UDP {to}: sent {}, got {}{err}]", inner.udp_sent, inner.udp_received)
            }
        };
        Status {
            connection,
            connected: inner.session.is_connected(),
            transmitting: inner.transmitting,
            talking: inner.mixer.talking(),
            me: inner.session.player(),
            muted: inner.muted,
            deafened: inner.deafened,
            disabled: inner.session.disabled,
            input_level_dbfs: inner.level,
            input_device: inner.input_device.clone(),
            output_device: inner.output_device.clone(),
            mic_frames_sent: inner.mic_frames_sent,
            voice_frames_received: inner.voice_frames_received,
            udp_summary: udp_summary(&inner),
            groups: inner.groups.iter().map(|g| group_info(&inner, g)).collect(),
            my_group: inner.my_group.and_then(|id| inner.groups.iter().find(|g| g.id == id)).map(|g| group_info(&inner, g)),
            group_problem: inner.group_problem.clone(),
        }
    }

    /// Left the server: forget the session (the on/off choice stays).
    pub fn disconnect(&self) {
        let mut inner = self.shared.lock();
        inner.session.reset();
        inner.socket = None;
        inner.target = None;
        inner.encoder = None;
        inner.outbox.clear();
        inner.groups.clear();
        inner.my_group = None;
        inner.players.clear();
        inner.group_problem = None;
        let max = 48.0;
        inner.mixer = Mixer::new(max);
        inner.mixer.volume = inner.settings.output_volume;
        inner.mixer.deafened = inner.deafened || inner.session.disabled;
    }
}

impl Drop for VoiceClient {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn on_secret(inner: &mut Inner, secret: Secret, game_host: &str, now: u64) -> Vec<Action> {
    inner.target = Some(secret.udp_target(game_host));
    inner.socket = None;
    inner.sequence = 0;
    inner.udp_sent = 0;
    inner.udp_received = 0;
    inner.udp_error = None;
    inner.mic_frames_sent = 0;
    inner.voice_frames_received = 0;
    inner.udp_types = [0; 256];
    inner.udp_unreadable = 0;
    inner.mixer.set_max_distance(secret.voice_distance);
    inner.encoder = match Encoder::new(secret.codec) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("[earshot] {e}");
            None
        }
    };
    inner.session.on_secret(secret, now)
}

/// Carries out what the session asked for. UDP needs the socket, so UDP actions before it
/// exists are dropped: the session repeats them each second.
fn run_actions(inner: &mut Inner, actions: Vec<Action>) {
    for action in actions {
        match action {
            Action::Plugin { channel, data } => inner.outbox.push((channel, data)),
            Action::Udp(msg) => send_udp(inner, &msg),
            Action::Connected => eprintln!("[earshot] voice connected"),
            Action::Lost(why) => {
                eprintln!("[earshot] voice: {why}");
                inner.socket = None;
            }
        }
    }
}

fn send_udp(inner: &mut Inner, msg: &Message) {
    let (Some(socket), Some(key), Some(me)) = (&inner.socket, inner.session.key(), inner.session.player()) else { return };
    let result = udp::seal_to_server(key, &me, msg).map_err(|e| e.to_string()).and_then(|packet| socket.send(&packet).map_err(|e| e.to_string()));
    match result {
        Ok(_) => inner.udp_sent += 1,
        Err(e) => {
            if inner.udp_error.as_deref() != Some(e.as_str()) {
                eprintln!("[earshot] sending voice: {e}");
            }
            inner.udp_error = Some(e);
        }
    }
}

fn group_info(inner: &Inner, g: &plugin::Group) -> GroupInfo {
    let mut members: Vec<(Uuid, String)> = inner
        .players
        .iter()
        .filter(|(_, (_, grp))| *grp == Some(g.id))
        .map(|(id, (name, _))| (*id, name.clone()))
        .collect();
    members.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    GroupInfo { id: g.id, name: g.name.clone(), has_password: g.has_password, members }
}

/// "got 08x52 02x3, 1 unreadable (last: …)" for the settings page.
fn udp_summary(inner: &Inner) -> String {
    let types: Vec<String> = inner
        .udp_types
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(t, n)| format!("{t:02x}x{n}"))
        .collect();
    let mut s = format!("UDP got {}", if types.is_empty() { "nothing".to_string() } else { types.join(" ") });
    if inner.udp_unreadable > 0 {
        s.push_str(&format!(", {} unreadable", inner.udp_unreadable));
    }
    if let Some(e) = &inner.udp_error {
        s.push_str(&format!(" (last problem: {e})"));
    }
    s
}

/// The first bytes of a packet in hex, for the status line.
fn short_hex(b: &[u8]) -> String {
    let mut s: String = b.iter().take(12).map(|x| format!("{x:02x}")).collect();
    if b.len() > 12 {
        s.push('…');
    }
    s
}

fn open_socket(host: &str, port: u16) -> Result<UdpSocket, String> {
    let addr: SocketAddr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("can't find voice server {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("can't find voice server {host}"))?;
    let local: SocketAddr = if addr.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
    let socket = UdpSocket::bind(local).map_err(|e| e.to_string())?;
    socket.connect(addr).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(5))).map_err(|e| e.to_string())?;
    Ok(socket)
}

#[cfg(feature = "audio")]
type Audio = crate::audio::AudioIo;
#[cfg(not(feature = "audio"))]
type Audio = ();

fn worker(shared: Arc<Shared>) {
    let capture: Arc<Mutex<VecDeque<f32>>> = Default::default();
    let playback: Arc<Mutex<VecDeque<[f32; 2]>>> = Default::default();
    // Filled by a helper thread, so opening slow devices never holds up the network.
    let audio: Arc<Mutex<Option<Audio>>> = Default::default();
    let opening_audio = Arc::new(AtomicBool::new(false));
    let mut buf = vec![0u8; 2048];
    let mut mixed_frames = shared.now() / 20;

    while !shared.stop.load(Ordering::Relaxed) {
        let now = shared.now();

        // The socket first: made once we know where the voice server is.
        let socket = {
            let mut inner = shared.lock();
            if inner.socket.is_none() && matches!(inner.session.phase, Phase::Authenticating { .. }) {
                if let Some((host, port)) = inner.target.clone() {
                    drop(inner);
                    let opened = open_socket(&host, port);
                    inner = shared.lock();
                    match opened {
                        Ok(s) => {
                            eprintln!("[earshot] voice server: udp {host}:{port}, from {:?}", s.local_addr());
                            inner.socket = Some(s);
                            // Don't wait for the next retry: log in now.
                            let auth = inner
                                .session
                                .secret()
                                .map(|s| Message::Authenticate { player: s.player, secret: s.secret });
                            if let Some(auth) = auth {
                                send_udp(&mut inner, &auth);
                            }
                        }
                        Err(e) => {
                            eprintln!("[earshot] {e}");
                            inner.udp_error = Some(e.clone());
                            let actions = inner.session.fail(&e);
                            run_actions(&mut inner, actions);
                        }
                    }
                }
            }
            inner.socket.as_ref().and_then(|s| s.try_clone().ok())
        };

        // Audio devices: opened once a voice server exists, reopened when the player picks others.
        {
            let mut inner = shared.lock();
            let wanted = inner.target.is_some() && inner.settings.use_audio_devices;
            let have = audio.lock().map(|a| a.is_some()).unwrap_or(false);
            if ((wanted && !have) || inner.reopen_audio) && !opening_audio.swap(true, Ordering::SeqCst) {
                inner.reopen_audio = false;
                let (i, o) = (inner.settings.input_device.clone(), inner.settings.output_device.clone());
                drop(inner);
                let (audio, opening, shared) = (audio.clone(), opening_audio.clone(), shared.clone());
                let (capture, playback) = (capture.clone(), playback.clone());
                let _ = std::thread::Builder::new().name("earshot-audio-open".into()).spawn(move || {
                    // Release the old devices before opening the new ones.
                    if let Ok(mut a) = audio.lock() {
                        a.take();
                    }
                    let io = open_audio(i, o, capture, playback, &shared);
                    if let Ok(mut a) = audio.lock() {
                        *a = Some(io);
                    }
                    opening.store(false, Ordering::SeqCst);
                });
            } else if !wanted && have && !opening_audio.load(Ordering::SeqCst) {
                drop(inner);
                if let Ok(mut a) = audio.lock() {
                    a.take();
                }
            }
        }

        // Receive what's waiting (the 5 ms read timeout paces this loop), for at most
        // RECEIVE_BATCH: with several voices, packets can arrive closer together than the
        // timeout, and the microphone and speakers below must still get their turn. What's left
        // waits in the socket for the next pass.
        if let Some(socket) = &socket {
            let batch_end = Instant::now() + RECEIVE_BATCH;
            while Instant::now() < batch_end {
                let Ok(n) = socket.recv(&mut buf) else { break };
                let mut inner = shared.lock();
                let Some(key) = inner.session.key().cloned() else { break };
                inner.udp_received += 1;
                let decoded = udp::open_raw_from_server(&key, &buf[..n])
                    .map_err(|e| format!("{e} ({n} bytes, starts {})", short_hex(&buf[..n])))
                    .and_then(|plain| Message::decode(&plain).map_err(|e| format!("can't read reply: {e} (plain {})", short_hex(&plain))));
                match decoded {
                    Ok(msg) => {
                        let ty = msg.encode()[0];
                        inner.udp_types[ty as usize] += 1;
                        if let Message::PlayerSound(s) = &msg {
                            inner.voice_frames_received += 1;
                            inner.mixer.push(s.sender, s.sequence, s.opus.clone());
                        }
                        if let Message::GroupSound(s) = &msg {
                            inner.voice_frames_received += 1;
                            inner.mixer.push_group(s.sender, s.sequence, s.opus.clone());
                        }
                        let actions = inner.session.on_udp(&msg, now);
                        run_actions(&mut inner, actions);
                    }
                    // Not ours, damaged, or a message we don't know: skip it, but show why.
                    Err(e) => {
                        inner.udp_unreadable += 1;
                        if inner.udp_error.as_deref() != Some(e.as_str()) {
                            eprintln!("[earshot] {e}");
                        }
                        inner.udp_error = Some(e);
                    }
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut inner = shared.lock();
        // A fresh reading: the game thread may have moved the session on since `now`.
        let actions = inner.session.poll(shared.now());
        run_actions(&mut inner, actions);

        // Microphone: one Opus frame per 960 captured samples.
        loop {
            let frame: [f32; FRAME_SAMPLES] = {
                let Ok(mut q) = capture.lock() else { break };
                if q.len() < FRAME_SAMPLES {
                    break;
                }
                let mut f = [0f32; FRAME_SAMPLES];
                for (dst, src) in f.iter_mut().zip(q.drain(..FRAME_SAMPLES)) {
                    *dst = src;
                }
                f
            };
            send_mic_frame(&mut inner, frame);
        }

        // Speakers: the mixer runs on the wall clock, one frame per 20 ms, whether or not a
        // device is draining the queue (the talking indicator depends on it too). If nothing
        // drains it, or the device runs slow, the oldest frames go, which bounds the delay.
        let due = now / 20;
        if let Ok(mut q) = playback.lock() {
            while mixed_frames < due {
                let mixed = inner.mixer.mix();
                q.extend(mixed.chunks_exact(2).map(|c| [c[0], c[1]]));
                mixed_frames += 1;
            }
            let excess = q.len().saturating_sub(PLAYBACK_LIMIT);
            q.drain(..excess);
        }
    }
    // Close the devices with the client.
    if let Ok(mut a) = audio.lock() {
        a.take();
    }
}

fn send_mic_frame(inner: &mut Inner, mut frame: [f32; FRAME_SAMPLES]) {
    let gain = inner.settings.mic_volume;
    if gain != 1.0 {
        for s in frame.iter_mut() {
            *s = (*s * gain).clamp(-1.0, 1.0);
        }
    }
    // The level (for the meter and voice activation) is the microphone as it is; the clean-up
    // runs on every frame either way, so its background estimate stays current.
    inner.level = activation::level_dbfs(&frame);
    if inner.settings.noise_suppression {
        inner.suppressor.process(&mut frame);
    }
    let wants = inner.activation.frame(inner.level, inner.ptt_down);
    let send = wants && !inner.muted && !inner.session.disabled && inner.session.is_connected();
    inner.transmitting = send;
    if !send {
        return;
    }
    let mtu = inner.session.secret().map(|s| s.mtu.max(1) as usize).unwrap_or(1275);
    let Some(encoder) = inner.encoder.as_mut() else { return };
    let opus = match encoder.encode(&frame, mtu) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[earshot] {e}");
            return;
        }
    };
    let msg = Message::Mic(MicFrame { opus, sequence: inner.sequence, flag: 0 });
    inner.sequence += 1;
    inner.mic_frames_sent += 1;
    send_udp(inner, &msg);
}

#[cfg(feature = "audio")]
fn open_audio(
    input: Option<String>,
    output: Option<String>,
    capture: Arc<Mutex<VecDeque<f32>>>,
    playback: Arc<Mutex<VecDeque<[f32; 2]>>>,
    shared: &Shared,
) -> Audio {
    let io = crate::audio::AudioIo::start(input, output, capture, playback);
    let mut inner = shared.lock();
    inner.input_device = io.input.clone().unwrap_or_else(|e| format!("none ({e})"));
    inner.output_device = io.output.clone().unwrap_or_else(|e| format!("none ({e})"));
    io
}

#[cfg(not(feature = "audio"))]
fn open_audio(
    _input: Option<String>,
    _output: Option<String>,
    _capture: Arc<Mutex<VecDeque<f32>>>,
    _playback: Arc<Mutex<VecDeque<[f32; 2]>>>,
    _shared: &Shared,
) -> Audio {
}

#[cfg(test)]
mod tests {
    //! End to end against a fake SVC voice server on localhost: the real UDP path, sockets,
    //! threads and encryption, with the game side driven by hand. Built without the `audio`
    //! feature's devices in mind: these tests don't need a sound card.

    use super::*;
    use crate::crypto::VoiceKey;
    use crate::plugin::Codec;
    use std::time::Duration;

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn connects_to_a_fake_voice_server_and_answers_keep_alives() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let port = server.local_addr().unwrap().port();
        let me = Uuid([7; 16]);
        let key_bytes = [9u8; 16];
        let key = VoiceKey::new(&key_bytes);

        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });

        // 1. The server's register makes the client ask for the secret.
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(plugin::SERVER_RECEIVES), "127.0.0.1");
        let out = client.take_outgoing();
        assert_eq!(out.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [plugin::MINECRAFT_REGISTER, plugin::REQUEST_SECRET]);

        // 2. The secret: voice host left empty, so the joined host and the port are used.
        let secret = Secret {
            secret: key_bytes,
            server_port: port as i32,
            player: me,
            codec: Codec::Voip,
            mtu: 1275,
            voice_distance: 48.0,
            keep_alive_ms: 1000,
            groups_enabled: true,
            voice_host: String::new(),
            recording_allowed: true,
        };
        client.on_plugin(plugin::SECRET, &secret.encode(), "127.0.0.1");

        // 3. The client logs in over UDP (retrying until we answer).
        let mut buf = [0u8; 2048];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        let (sender, msg) = udp::open_from_client(&key, &buf[..n]).unwrap();
        assert_eq!(sender, me);
        assert_eq!(msg, Message::Authenticate { player: me, secret: key_bytes });
        server.send_to(&udp::seal_to_client(&key, &Message::AuthenticateAck).unwrap(), from).unwrap();

        // 4. Connection check, then connected.
        loop {
            let (n, _) = server.recv_from(&mut buf).unwrap();
            if udp::open_from_client(&key, &buf[..n]).unwrap().1 == Message::ConnectionCheck {
                break;
            }
        }
        server.send_to(&udp::seal_to_client(&key, &Message::ConnectionCheckAck).unwrap(), from).unwrap();
        wait_for("connected", || client.status().connected);
        assert_eq!(client.take_outgoing(), [(plugin::UPDATE_STATE, vec![0])]);

        // 5. A keep-alive is answered.
        server.send_to(&udp::seal_to_client(&key, &Message::KeepAlive).unwrap(), from).unwrap();
        loop {
            let (n, _) = server.recv_from(&mut buf).unwrap();
            if udp::open_from_client(&key, &buf[..n]).unwrap().1 == Message::KeepAlive {
                break;
            }
        }

        // 6. Another player's voice reaches the mixer.
        let alice = Uuid([1; 16]);
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        for seq in 0..5 {
            let opus = enc.encode(&[0.1; FRAME_SAMPLES], 1275).unwrap();
            let sound = Message::PlayerSound(udp::PlayerSound {
                channel: alice,
                sender: alice,
                opus,
                sequence: seq,
                distance: 48.0,
                flag: 0,
            });
            server.send_to(&udp::seal_to_client(&key, &sound).unwrap(), from).unwrap();
        }
        wait_for("alice talking", || client.status().talking.contains(&alice));

        // 7. Switching voice off tells the server over the game connection.
        client.set_disabled(true);
        assert_eq!(client.take_outgoing(), [(plugin::UPDATE_STATE, vec![1])]);
        assert!(client.status().disabled);

        client.disconnect();
        assert!(!client.status().connected);
    }

    #[test]
    fn tracks_groups_from_the_servers_messages() {
        use crate::wire::from_hex;
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let group = Uuid::parse("453d1ee3-0610-47f2-bfb2-27f017fc5782").unwrap();
        // Capture 06, 511.20 s onwards: the group appears, NullPort404 is in it, then we join.
        client.on_plugin(plugin::ADD_GROUP, &from_hex("453d1ee3061047f2bfb227f017fc5782 0c 5465737450617373776f7264 01 00000000"), "h");
        client.on_plugin(
            plugin::STATE,
            &from_hex("0000 c5c602ae651d38579a04b3e776be1219 0b 4e756c6c506f7274343034 01 453d1ee3061047f2bfb227f017fc5782"),
            "h",
        );
        let s = client.status();
        assert_eq!(s.groups.len(), 1);
        assert_eq!((s.groups[0].name.as_str(), s.groups[0].has_password), ("TestPassword", true));
        assert_eq!(s.groups[0].members, [(Uuid::parse("c5c602ae-651d-3857-9a04-b3e776be1219").unwrap(), "NullPort404".to_string())]);
        assert!(s.my_group.is_none());

        client.on_plugin(plugin::JOINED_GROUP, &from_hex("01 453d1ee3061047f2bfb227f017fc5782 00"), "h");
        assert_eq!(client.status().my_group.map(|g| g.id), Some(group));

        // Leaving: joined_group 00 00, then the group goes away.
        client.on_plugin(plugin::JOINED_GROUP, &from_hex("0000"), "h");
        client.on_plugin(plugin::REMOVE_GROUP, &from_hex("453d1ee3061047f2bfb227f017fc5782"), "h");
        let s = client.status();
        assert!(s.my_group.is_none() && s.groups.is_empty());

        // Not connected: group requests aren't sent.
        client.join_group(group, Some("1221"));
        assert!(client.take_outgoing().is_empty());
    }

    #[test]
    fn ignores_servers_without_voice() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        client.on_plugin(plugin::MINECRAFT_REGISTER, b"minecraft:brand", "example.test");
        client.on_plugin("other:thing", &[1, 2, 3], "example.test");
        assert!(client.take_outgoing().is_empty());
        assert_eq!(client.status().connection, "no voice server");
    }
}
