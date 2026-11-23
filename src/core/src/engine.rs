//! The voice client: everything between the game and the network/sound card.
//!
//! The game calls in from its own thread with cheap, non-blocking calls: plugin messages it
//! receives ([`VoiceClient::on_plugin`]), the listener and player positions plus the push-to-talk
//! key once a tick ([`VoiceClient::tick`]), and it sends whatever [`VoiceClient::take_outgoing`]
//! returns over the game connection.
//!
//! One worker thread does the rest: the UDP socket, the handshake's retries, encoding the
//! microphone and keeping the speakers fed from the [`Mixer`]. Each audio device has its own
//! worker, so the game never waits on hardware.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::activation::{self, Activation, Mode};
use crate::audio::{self, AudioIo, CaptureQueue, PlaybackQueue};
use crate::codec::{Encoder, FRAME_SAMPLES};
use crate::mixer::Mixer;
use crate::plasmo::{self, Plasmo};
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
    /// Everyone heard through our group, on top of their own volumes (1.0 = unchanged).
    pub group_volume: f32,
    /// A chime when someone joins or leaves our group ([crate::chime]).
    pub group_sounds: bool,
    /// Open the sound card. Off only in tests, which have no audio hardware to rely on.
    pub use_audio_devices: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            mic_volume: 1.0,
            output_volume: 1.0,
            mode: Mode::PushToTalk,
            noise_suppression: true,
            group_volume: 1.0,
            group_sounds: true,
            use_audio_devices: true,
        }
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
    /// ...as a whisper.
    pub whispering: bool,
    /// Players we heard in the last 200 ms.
    pub talking: Vec<Uuid>,
    /// Our own player, once the server sent the voice key.
    pub me: Option<Uuid>,
    /// Players who turned voice chat off, and players connected to voice (from the server's states).
    pub voice_off: Vec<Uuid>,
    pub has_voice: Vec<Uuid>,
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
    /// Each player's name, as the voice server gave it: on a remote server the game finds other
    /// players by name, not by UUID.
    pub names: Vec<(Uuid, String)>,
}

/// A group as the menu shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupInfo {
    pub id: Uuid,
    pub name: String,
    pub has_password: bool,
    pub kind: plugin::GroupType,
    /// Players in it, as the server's states say.
    pub members: Vec<(Uuid, String)>,
}

struct Inner {
    groups: Vec<plugin::Group>,
    my_group: Option<Uuid>,
    /// Each player's name and group, from `state`/`states`.
    players: std::collections::HashMap<Uuid, (String, Option<Uuid>)>,
    /// Each player's voice flags from the same states: (voice chat off, not connected to voice).
    voice_flags: std::collections::HashMap<Uuid, (bool, bool)>,
    group_problem: Option<String>,
    session: Session,
    /// A Plasmo Voice server's connection, when the server runs Plasmo Voice instead of SVC.
    plasmo: Option<Plasmo>,
    /// Plasmo Voice's largest audio packet, from its config.
    plasmo_mtu: usize,
    /// The server runs Plasmo Voice (it registered its channel).
    plasmo_server: bool,
    /// We registered Plasmo Voice's channels back: done only while voice chat is on.
    plasmo_registered: bool,
    /// Voice chat was switched back on: introduce ourselves once a key is ready, as the server
    /// only asks when a player joins (and when a connection times out).
    plasmo_resume: bool,
    /// The key pair of the Plasmo Voice connection under way: a fresh one for each, dropped
    /// with it.
    plasmo_key: Option<plasmo::KeyPair>,
    /// Our own player, as the game says: Plasmo Voice never names it (SVC's secret does).
    me: Option<Uuid>,
    mixer: Mixer,
    /// Plugin messages waiting for the game connection, each with the protocol it belongs to.
    outbox: Vec<(Owner, &'static str, Vec<u8>)>,
    /// The voice server's `host:port` and the protocol that owns it, resolved by the worker.
    target: Option<(Owner, String, u16)>,
    /// The socket to `target`, owned as it is: only its owner sends or reads on it.
    socket: Option<(Owner, UdpSocket)>,
    /// The Plasmo Voice session under way, numbered: anything queued for an older one is dropped.
    plasmo_epoch: u64,
    encoder: Option<Encoder>,
    sequence: u64,
    activation: Activation,
    suppressor: crate::dsp::NoiseSuppressor,
    settings: Settings,
    capture: CaptureQueue,
    ptt_down: bool,
    /// The whisper key is held: send whatever the mode, marked as a whisper.
    whisper_down: bool,
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
    /// Hear our own microphone (the settings' mic test): opens the sound card even with no voice server.
    mic_test: bool,
    /// Our group and its other members as last seen, to chime when they change.
    group_view: crate::chime::GroupView,
}

impl Inner {
    fn audio_devices(&self) -> (Option<Option<String>>, Option<Option<String>>) {
        let voice = matches!(self.session.phase, Phase::Authenticating { .. } | Phase::Checking { .. } | Phase::Connected { .. })
            || self.plasmo.as_ref().is_some_and(Plasmo::connecting);
        let test = self.mic_test && !self.muted;
        let active = self.settings.use_audio_devices && !self.session.disabled && (voice || test);
        let input = (active && !self.muted).then(|| self.settings.input_device.clone());
        let output = active.then(|| self.settings.output_device.clone());
        (input, output)
    }

    /// The microphone this client wants open now, with the queue it fills.
    fn wanted_input(&self) -> Option<InputDevice> {
        self.audio_devices().0.map(|name| InputDevice { name, queue: self.capture.clone() })
    }

    fn reset_capture(&mut self) {
        // The old stream can still have a callback in flight. Give the next stream a
        // fresh queue so muted or disconnected audio cannot leak into a later session.
        self.capture = Default::default();
        self.level = -127.0;
        self.transmitting = false;
        self.activation = Activation::new(self.settings.mode);
    }
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

/// Which protocol a queued message, a voice address or a socket belongs to: Simple Voice Chat,
/// or Plasmo Voice's session `n` (0: its channel registration, before any session).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Svc,
    Plasmo(u64),
}

impl VoiceClient {
    pub fn new(settings: Settings) -> Self {
        plasmo_key_start();
        let inner = Inner {
            groups: Vec::new(),
            my_group: None,
            players: std::collections::HashMap::new(),
            voice_flags: std::collections::HashMap::new(),
            group_problem: None,
            session: Session::new(),
            plasmo: None,
            plasmo_mtu: 1024,
            plasmo_server: false,
            plasmo_registered: false,
            plasmo_resume: false,
            plasmo_key: None,
            me: None,
            mixer: Mixer::new(48.0),
            outbox: Vec::new(),
            target: None,
            socket: None,
            plasmo_epoch: 0,
            encoder: None,
            sequence: 0,
            activation: Activation::new(settings.mode),
            suppressor: crate::dsp::NoiseSuppressor::new(),
            settings,
            capture: Default::default(),
            ptt_down: false,
            whisper_down: false,
            muted: false,
            deafened: false,
            transmitting: false,
            level: -127.0,
            input_device: "closed".into(),
            output_device: "closed".into(),
            udp_sent: 0,
            udp_received: 0,
            udp_error: None,
            mic_frames_sent: 0,
            voice_frames_received: 0,
            udp_types: [0; 256],
            udp_unreadable: 0,
            mic_test: false,
            group_view: None,
        };
        let shared = Arc::new(Shared { inner: Mutex::new(inner), stop: AtomicBool::new(false), start: Instant::now() });
        let s = shared.clone();
        let worker = std::thread::Builder::new().name("earshot-voice".into()).spawn(move || worker(s)).ok();
        Self { shared, worker }
    }

    /// A custom payload from the server, any channel. `game_host` is the host the player joined,
    /// used when the server's `voice_host` doesn't name one.
    /// Applies a voice chat message from the server; returns the group change it applied, if
    /// any, for the voice menu.
    pub fn on_plugin(&self, channel: &str, data: &[u8], game_host: &str) -> Option<GroupEvent> {
        let now = self.shared.now();
        let mut inner = self.shared.lock();
        let mut event = None;
        let actions = if channel == plugin::MINECRAFT_REGISTER {
            let channels = plugin::decode_register(data);
            // A Plasmo Voice server: say we have the mod, and it asks for our player info.
            if channels.iter().any(|c| c == plasmo::CHANNEL) {
                inner.plasmo_server = true;
                // Plasmo Voice takes over: a whisper key held until now stops counting.
                inner.whisper_down = false;
                plasmo_register(&mut inner);
            }
            inner.session.on_server_register(&channels, now)
        } else if channel == plasmo::CHANNEL {
            // Voice chat off: nothing is negotiated or answered (the server asks again later).
            if !inner.session.disabled {
                if inner.plasmo_key.is_none() {
                    inner.plasmo_key = plasmo_key_take();
                }
                plasmo_start(&mut inner);
                let muted = inner.muted;
                let Inner { plasmo, plasmo_key, .. } = &mut *inner;
                let Some(p) = plasmo.as_mut() else { return None };
                (p.voice_disabled, p.mic_muted) = (false, muted);
                let todo = p.on_tcp(data, plasmo_key.as_ref(), game_host, now);
                run_plasmo(&mut inner, todo);
            }
            vec![]
        } else {
            match ServerMessage::decode(channel, data) {
                Some(Ok(ServerMessage::Secret(secret))) => on_secret(&mut inner, secret, game_host, now),
                Some(Ok(ServerMessage::State(st))) => {
                    inner.voice_flags.insert(st.player, (st.disabled, st.disconnected));
                    inner.players.insert(st.player, (st.name, st.group));
                    vec![]
                }
                Some(Ok(ServerMessage::States(list))) => {
                    for st in list {
                        inner.voice_flags.insert(st.player, (st.disabled, st.disconnected));
                        inner.players.insert(st.player, (st.name, st.group));
                    }
                    vec![]
                }
                Some(Ok(ServerMessage::RemoveState(id))) => {
                    inner.players.remove(&id);
                    inner.voice_flags.remove(&id);
                    vec![]
                }
                Some(Ok(ServerMessage::AddGroup(g))) => {
                    inner.groups.retain(|x| x.id != g.id);
                    inner.groups.push(g);
                    vec![]
                }
                Some(Ok(ServerMessage::RemoveGroup(id))) => {
                    inner.groups.retain(|x| x.id != id);
                    event = Some(GroupEvent::Removed(id));
                    vec![]
                }
                Some(Ok(ServerMessage::JoinedGroup(j))) => {
                    inner.my_group = j.group;
                    inner.group_problem = j.wrong_password.then(|| "wrong password".to_string());
                    event = j.group.map(GroupEvent::Joined);
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
        chime_on_group_change(&mut inner);
        event
    }

    /// Once a game tick: where we listen from, where the other players are, and the
    /// push-to-talk and whisper keys.
    pub fn tick(&self, listener: Option<Listener>, players: Vec<(Uuid, [f64; 3])>, ptt_down: bool, whisper_down: bool) {
        let mut inner = self.shared.lock();
        inner.mixer.set_listener(listener);
        inner.mixer.set_positions(players);
        inner.ptt_down = ptt_down;
        // Plasmo Voice has no whisper: the key does nothing there, and nothing says Whispering
        // (`whispering` checks again where it's used).
        inner.whisper_down = whisper_down && inner.plasmo.is_none() && !inner.plasmo_server;
        plasmo_resume(&mut inner);
    }

    /// Our own player, from the game.
    pub fn set_me(&self, me: Option<Uuid>) {
        self.shared.lock().me = me;
    }

    /// Plugin messages to send over the game connection now.
    /// Plasmo Voice's are checked again here: none goes out with voice chat off, and none for a
    /// session that has ended.
    pub fn take_outgoing(&self) -> Vec<(&'static str, Vec<u8>)> {
        let mut inner = self.shared.lock();
        let out = std::mem::take(&mut inner.outbox);
        let (off, server, epoch) = (inner.session.disabled, inner.plasmo_server, inner.plasmo.as_ref().map(|_| inner.plasmo_epoch));
        let mut sent = Vec::with_capacity(out.len());
        for (owner, channel, data) in out {
            let keep = match owner {
                Owner::Svc => true,
                Owner::Plasmo(0) => !off && server,
                Owner::Plasmo(n) => !off && epoch == Some(n),
            };
            if keep {
                sent.push((channel, data));
            } else if owner == Owner::Plasmo(0) {
                // A registration that never went out: it's made again when voice chat is on.
                inner.plasmo_registered = false;
            }
        }
        sent
    }

    pub fn set_muted(&self, muted: bool) {
        let mut inner = self.shared.lock();
        if muted && !inner.muted {
            inner.reset_capture();
        }
        inner.muted = muted;
        plasmo_state(&mut inner);
    }

    pub fn set_deafened(&self, deafened: bool) {
        let mut inner = self.shared.lock();
        inner.deafened = deafened;
        inner.mixer.deafened = inner.session.disabled || deafened;
    }

    /// Voice chat off entirely: the server is told, as SVC's own toggle does.
    pub fn set_disabled(&self, disabled: bool) {
        let mut inner = self.shared.lock();
        if disabled && !inner.session.disabled {
            inner.reset_capture();
        }
        let actions = inner.session.set_disabled(disabled);
        inner.mixer.deafened = disabled || inner.deafened;
        run_actions(&mut inner, actions);
        if disabled {
            inner.plasmo_resume = false;
            // Anything queued for Plasmo Voice, a registration included, even before a session.
            let had_register = inner.outbox.iter().any(|(o, ..)| *o == Owner::Plasmo(0));
            inner.outbox.retain(|(o, ..)| *o == Owner::Svc);
            if had_register {
                inner.plasmo_registered = false;
            }
            plasmo_drop(&mut inner);
        } else if inner.plasmo_server && inner.plasmo.is_none() {
            // The channels first if we never registered them, then our info, unasked: the server
            // may have stopped asking.
            plasmo_register(&mut inner);
            inner.plasmo_resume = true;
            plasmo_resume(&mut inner);
        }
    }

    /// Makes a group of this type and joins it (`create_group`, captures 06 and 07).
    pub fn create_group(&self, name: &str, password: Option<&str>, kind: plugin::GroupType) {
        let data = plugin::CreateGroup { name: name.to_string(), password: password.map(str::to_string), kind }.encode();
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
            inner.outbox.push((Owner::Svc, channel, data));
        }
    }

    pub fn set_player_volume(&self, player: Uuid, volume: f32) {
        self.shared.lock().mixer.player_volume.insert(player, volume.max(0.0));
    }

    /// The mic test: while on, the microphone (after its volume and the noise reduction, as
    /// others would hear it) plays back through our own speakers.
    pub fn set_mic_test(&self, on: bool) {
        self.shared.lock().mic_test = on;
    }

    pub fn apply_settings(&self, settings: Settings) {
        let mut inner = self.shared.lock();
        if settings.input_device != inner.settings.input_device || !settings.use_audio_devices && inner.settings.use_audio_devices {
            inner.reset_capture();
        }
        inner.activation.mode = settings.mode;
        inner.mixer.volume = settings.output_volume;
        inner.mixer.group_volume = settings.group_volume.max(0.0);
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
                let to = target.as_ref().map(|(_, h, p)| format!("{h}:{p}")).unwrap_or_else(|| "no address".into());
                let err = inner.udp_error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default();
                format!("{connection} [UDP {to}: sent {}, got {}{err}]", inner.udp_sent, inner.udp_received)
            }
        };
        let connection = match &inner.plasmo {
            Some(p) if !p.connected() && p.connecting() => {
                let to = inner.target.as_ref().map(|(_, h, port)| format!("{h}:{port}")).unwrap_or_else(|| "no address".into());
                let err = inner.udp_error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default();
                format!("{} [UDP {to}: sent {}, got {}{err}]", p.describe(), inner.udp_sent, inner.udp_received)
            }
            Some(p) => p.describe(),
            None => connection,
        };
        let (voice_off, has_voice) = match &inner.plasmo {
            Some(p) => (
                p.players.values().filter(|x| x.voice_disabled).map(|x| x.id).collect(),
                p.players.values().filter(|x| !x.voice_disabled).map(|x| x.id).collect(),
            ),
            None => (
                inner.voice_flags.iter().filter(|(_, f)| f.0).map(|(id, _)| *id).collect(),
                inner.voice_flags.iter().filter(|(_, f)| !f.1).map(|(id, _)| *id).collect(),
            ),
        };
        Status {
            connection,
            connected: inner.session.is_connected() || inner.plasmo.as_ref().is_some_and(Plasmo::connected),
            transmitting: inner.transmitting,
            whispering: inner.transmitting && whispering(&inner),
            talking: inner.mixer.talking(),
            me: inner.session.player().or(inner.me),
            voice_off,
            has_voice,
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
            names: match &inner.plasmo {
                Some(p) => p.players.values().map(|x| (x.id, x.nick.clone())).collect(),
                None => inner.players.iter().map(|(id, (name, _))| (*id, name.clone())).collect(),
            },
        }
    }

    /// Left the server: forget the session (the on/off choice stays).
    pub fn disconnect(&self) {
        let mut inner = self.shared.lock();
        inner.session.reset();
        inner.plasmo = None;
        inner.plasmo_key = None;
        inner.plasmo_server = false;
        inner.plasmo_registered = false;
        inner.plasmo_resume = false;
        inner.reset_capture();
        inner.socket = None;
        inner.target = None;
        inner.encoder = None;
        inner.outbox.clear();
        inner.groups.clear();
        inner.my_group = None;
        inner.players.clear();
        inner.voice_flags.clear();
        inner.group_problem = None;
        // Leaving the server isn't leaving the group: no chime.
        inner.group_view = None;
        let player_volume = std::mem::take(&mut inner.mixer.player_volume);
        inner.mixer = Mixer::new(48.0);
        inner.mixer.player_volume = player_volume;
        inner.mixer.volume = inner.settings.output_volume;
        inner.mixer.group_volume = inner.settings.group_volume.max(0.0);
        inner.mixer.deafened = inner.deafened || inner.session.disabled;
    }
}

/// Our group and its other members, from the server's states (sorted, so order never counts).
/// A group change the server confirmed: we joined `Joined`'s group, or `Removed`'s group is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupEvent {
    Joined(Uuid),
    Removed(Uuid),
}

fn group_view(inner: &Inner) -> crate::chime::GroupView {
    let group = inner.my_group?;
    let me = inner.session.player();
    let mut members: Vec<Uuid> = inner
        .players
        .iter()
        .filter(|(id, (_, g))| *g == Some(group) && Some(**id) != me)
        .map(|(id, _)| *id)
        .collect();
    members.sort_by_key(|u| u.0);
    Some((group, members))
}

/// After a message from the server: if our group changed, play the chime for it.
fn chime_on_group_change(inner: &mut Inner) {
    let now = group_view(inner);
    let change = crate::chime::group_change(&inner.group_view, &now);
    inner.group_view = now;
    if let Some(chime) = change
        && inner.settings.group_sounds
    {
        let samples = crate::chime::samples(chime);
        inner.mixer.push_local(&samples);
    }
}

/// How long dropping a client waits for its worker to see `stop` (it checks every few ms).
const STOP_WAIT: Duration = Duration::from_millis(500);

impl Drop for VoiceClient {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let Some(w) = self.worker.take() else {
            return;
        };
        // At process exit Windows ends the worker before thread-local destructors run, and
        // joining a thread the OS ended panics ("threads should not terminate unexpectedly").
        // Join only a worker that finished; one that never does is left to the exiting process.
        let deadline = Instant::now() + STOP_WAIT;
        while !w.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if w.is_finished() {
            let _ = w.join();
        }
    }
}

fn on_secret(inner: &mut Inner, secret: Secret, game_host: &str, now: u64) -> Vec<Action> {
    let (host, port) = secret.udp_target(game_host);
    inner.target = Some((Owner::Svc, host, port));
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
            Action::Plugin { channel, data } => inner.outbox.push((Owner::Svc, channel, data)),
            Action::Udp(msg) => send_udp(inner, &msg),
            Action::Connected => eprintln!("[earshot] voice connected"),
            Action::Lost(why) => {
                eprintln!("[earshot] voice: {why}");
                inner.socket = None;
                inner.reset_capture();
            }
        }
    }
}

// --- Plasmo Voice ------------------------------------------------------------------------------

/// The next RSA key pair a Plasmo Voice server seals the audio key with, made ahead in the
/// background (a 2048-bit key takes a moment). Each connection takes its own, never used again;
/// until one is ready the server's request waits (it repeats it).
static PLASMO_NEXT_KEY: Mutex<Option<plasmo::KeyPair>> = Mutex::new(None);
static PLASMO_KEY_MAKING: AtomicBool = AtomicBool::new(false);

/// Starts making the next key, unless one is ready or on its way.
fn plasmo_key_start() {
    if PLASMO_NEXT_KEY.lock().map_or(true, |k| k.is_some()) || PLASMO_KEY_MAKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let made = std::thread::Builder::new().name("earshot-key".into()).spawn(|| {
        match plasmo::KeyPair::generate(2048) {
            Ok(key) => {
                if let Ok(mut next) = PLASMO_NEXT_KEY.lock() {
                    *next = Some(key);
                }
            }
            Err(e) => eprintln!("[earshot] no Plasmo Voice key: {e}"),
        }
        PLASMO_KEY_MAKING.store(false, Ordering::Release);
    });
    if made.is_err() {
        PLASMO_KEY_MAKING.store(false, Ordering::Release);
    }
}

/// A key for one connection, if one is ready, and the next one started.
fn plasmo_key_take() -> Option<plasmo::KeyPair> {
    let key = PLASMO_NEXT_KEY.lock().ok().and_then(|mut next| next.take());
    plasmo_key_start();
    key
}

/// Registers Plasmo Voice's channels back, which makes the server ask for our player info: only
/// on a Plasmo Voice server, once, and while voice chat is on.
fn plasmo_register(inner: &mut Inner) {
    if inner.plasmo_server && !inner.plasmo_registered && !inner.session.disabled {
        inner.plasmo_registered = true;
        inner.outbox.push((Owner::Plasmo(0), plugin::MINECRAFT_REGISTER, plugin::encode_register(&[plasmo::CHANNEL, plasmo::INSTALLED])));
    }
}

/// Introduces us to the Plasmo Voice server after voice chat was switched back on, with a new
/// key, once one is ready (tried each tick until then).
fn plasmo_resume(inner: &mut Inner) {
    if !inner.plasmo_resume || inner.session.disabled || inner.plasmo.is_some() {
        return;
    }
    let Some(key) = plasmo_key_take() else { return };
    inner.plasmo_resume = false;
    plasmo_start(inner);
    let muted = inner.muted;
    let Some(p) = inner.plasmo.as_mut() else { return };
    p.mic_muted = muted;
    let todo = p.introduce(&key);
    inner.plasmo_key = Some(key);
    run_plasmo(inner, todo);
}

/// A new Plasmo Voice session, unless one is under way: numbered after the last, with nothing
/// of an earlier one carried over (no queued packets, key, voice address or microphone audio),
/// and no whisper.
fn plasmo_start(inner: &mut Inner) {
    if inner.plasmo.is_some() {
        return;
    }
    inner.plasmo_epoch += 1;
    inner.plasmo = Some(Plasmo::new());
    inner.outbox.retain(|(o, ..)| !matches!(o, Owner::Plasmo(n) if *n != 0));
    if inner.target.as_ref().is_some_and(|(o, ..)| matches!(o, Owner::Plasmo(_))) {
        inner.target = None;
        inner.socket = None;
    }
    inner.whisper_down = false;
    inner.transmitting = false;
    inner.reset_capture();
}

/// The owner of what the current Plasmo Voice session queues and sends.
fn plasmo_owner(inner: &Inner) -> Owner {
    Owner::Plasmo(inner.plasmo_epoch)
}

/// Whether we're whispering: the key is held on a Simple Voice Chat connection, never while
/// Plasmo Voice (which has no whisper) has the voice connection or the server.
fn whispering(inner: &Inner) -> bool {
    inner.whisper_down
        && !inner.plasmo_server
        && inner.plasmo.is_none()
        && inner.socket.as_ref().is_some_and(|(o, _)| *o == Owner::Svc)
}

/// Voice chat off on a Plasmo Voice server: the connection, its key, its address and socket and
/// anything not yet sent to it are dropped, so nothing more goes out. Turned on again, the next
/// request from the server starts a new connection with a new key.
fn plasmo_drop(inner: &mut Inner) {
    if inner.plasmo.take().is_none() {
        return;
    }
    inner.plasmo_key = None;
    inner.outbox.retain(|(o, ..)| *o == Owner::Svc);
    if inner.target.as_ref().is_some_and(|(o, ..)| matches!(o, Owner::Plasmo(_))) {
        inner.target = None;
        inner.socket = None;
        inner.encoder = None;
    }
    inner.transmitting = false;
    inner.reset_capture();
}

/// Our mute and voice-off state, for a Plasmo Voice server.
fn plasmo_state(inner: &mut Inner) {
    let (muted, disabled) = (inner.muted, inner.session.disabled);
    if let Some(p) = inner.plasmo.as_mut() {
        (p.mic_muted, p.voice_disabled) = (muted, disabled);
        if p.connected() {
            let owner = plasmo_owner(inner);
            inner.outbox.push((owner, plasmo::CHANNEL, plasmo::player_state(disabled, muted)));
        }
    }
}

/// Carries out what the Plasmo Voice connection asked for.
fn run_plasmo(inner: &mut Inner, todo: Vec<plasmo::Do>) {
    for d in todo {
        match d {
            plasmo::Do::Tcp(data) => {
                let owner = plasmo_owner(inner);
                inner.outbox.push((owner, plasmo::CHANNEL, data));
            }
            plasmo::Do::Udp(data) => send_raw(inner, &data),
            plasmo::Do::Open { host, port } => {
                // This session's address alone: a new one never reuses an earlier socket or audio.
                inner.target = Some((plasmo_owner(inner), host, port));
                inner.socket = None;
                inner.transmitting = false;
                inner.reset_capture();
                inner.udp_sent = 0;
                inner.udp_received = 0;
                inner.udp_error = None;
                inner.mic_frames_sent = 0;
                inner.voice_frames_received = 0;
            }
            plasmo::Do::Ready { codec, mtu, distance } => {
                eprintln!("[earshot] voice connected (Plasmo Voice)");
                inner.plasmo_mtu = mtu;
                inner.mixer.set_max_distance(distance as f64);
                inner.encoder = match Encoder::new(codec) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        eprintln!("[earshot] {e}");
                        None
                    }
                };
            }
            plasmo::Do::Audio { source, player, sequence, opus, distance } => {
                inner.voice_frames_received += 1;
                match player {
                    // A player's voice comes from where that player is.
                    Some(p) => inner.mixer.push_ranged(p, sequence, opus, distance),
                    // Anything else (not yet known, or not placed): heard as is.
                    None => inner.mixer.push_group(source, sequence, opus),
                }
            }
            plasmo::Do::Lost(why) => {
                eprintln!("[earshot] voice: {why}");
                // The next connection gets a new key, and the address the server gives then.
                inner.socket = None;
                inner.target = None;
                inner.plasmo_key = None;
                inner.reset_capture();
            }
        }
    }
}

/// A ready-made datagram to the current Plasmo Voice session's server, on its own socket only.
fn send_raw(inner: &mut Inner, data: &[u8]) {
    let owner = plasmo_owner(inner);
    let Some((_, socket)) = inner.socket.as_ref().filter(|(o, _)| *o == owner && inner.plasmo.is_some()) else { return };
    match socket.send(data) {
        Ok(_) => inner.udp_sent += 1,
        Err(e) => inner.udp_error = Some(e.to_string()),
    }
}

fn send_udp(inner: &mut Inner, msg: &Message) {
    // Simple Voice Chat's own socket only.
    let socket = inner.socket.as_ref().filter(|(o, _)| *o == Owner::Svc).map(|(_, s)| s);
    let (Some(socket), Some(key), Some(me)) = (socket, inner.session.key(), inner.session.player()) else { return };
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
    GroupInfo { id: g.id, name: g.name.clone(), has_password: g.has_password, kind: g.kind, members }
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

struct InputDevice {
    name: Option<String>,
    queue: CaptureQueue,
}

impl PartialEq for InputDevice {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && Arc::ptr_eq(&self.queue, &other.queue)
    }
}

fn start_audio(shared: &Arc<Shared>, playback: PlaybackQueue) -> (AudioIo, AudioIo) {
    let (input, input_gate, input_status) = (shared.clone(), shared.clone(), shared.clone());
    let (output, output_gate, output_status) = (shared.clone(), shared.clone(), shared.clone());
    let play = |stream: &cpal::Stream| cpal::traits::StreamTrait::play(stream).map_err(|e| e.to_string());
    (
        AudioIo::start(
            "earshot-input",
            move || input.lock().wanted_input(),
            |host, device| audio::open_input(host, device.name.as_deref(), device.queue.clone()),
            // Checked and started under the lock `set_muted` and `set_disabled` take.
            move |device, stream| {
                let inner = input_gate.lock();
                audio::start_if_wanted(device, inner.wanted_input(), || play(stream))
            },
            move |status| input_status.lock().input_device = status,
        ),
        AudioIo::start(
            "earshot-output",
            move || output.lock().audio_devices().1,
            move |host, device| audio::open_output(host, device.as_deref(), playback.clone()),
            move |device, stream| {
                let inner = output_gate.lock();
                audio::start_if_wanted(device, inner.audio_devices().1, || play(stream))
            },
            move |status| output_status.lock().output_device = status,
        ),
    )
}

fn worker(shared: Arc<Shared>) {
    let playback = PlaybackQueue::default();
    let _audio = start_audio(&shared, playback.clone());
    let mut buf = vec![0u8; 2048];
    let mut mixed_frames = shared.now() / 20;

    while !shared.stop.load(Ordering::Relaxed) {
        let now = shared.now();

        // The socket first: made once we know where the voice server is.
        let socket = {
            let mut inner = shared.lock();
            // The socket is wanted by the protocol that owns the address, and only while it's connecting.
            let wants = |inner: &Inner, owner: Owner| match owner {
                Owner::Svc => matches!(inner.session.phase, Phase::Authenticating { .. }),
                Owner::Plasmo(_) => owner == plasmo_owner(inner) && inner.plasmo.as_ref().is_some_and(Plasmo::connecting),
            };
            if inner.socket.is_none() {
                if let Some((owner, host, port)) = inner.target.clone().filter(|(o, ..)| wants(&inner, *o)) {
                    drop(inner);
                    let opened = open_socket(&host, port);
                    inner = shared.lock();
                    if !wants(&inner, owner) || inner.target.as_ref() != Some(&(owner, host.clone(), port)) {
                        continue;
                    }
                    match opened {
                        Ok(s) => {
                            eprintln!("[earshot] voice server: udp {host}:{port}, from {:?}", s.local_addr());
                            inner.socket = Some((owner, s));
                            if owner == Owner::Svc {
                                // Don't wait for the next retry: log in now.
                                let auth = inner
                                    .session
                                    .secret()
                                    .map(|s| Message::Authenticate { player: s.player, secret: s.secret });
                                if let Some(auth) = auth {
                                    send_udp(&mut inner, &auth);
                                }
                            } else if let Some(p) = inner.plasmo.as_mut() {
                                // Plasmo Voice: say hello now.
                                let todo = p.poll(now);
                                run_plasmo(&mut inner, todo);
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
            inner.socket.as_ref().and_then(|(o, s)| s.try_clone().ok().map(|s| (*o, s)))
        };

        // Receive what's waiting (the 5 ms read timeout paces this loop), for at most
        // RECEIVE_BATCH: with several voices, packets can arrive closer together than the
        // timeout, and the microphone and speakers below must still get their turn. What's left
        // waits in the socket for the next pass.
        if let Some((owner, socket)) = &socket {
            let batch_end = Instant::now() + RECEIVE_BATCH;
            while Instant::now() < batch_end {
                let Ok(n) = socket.recv(&mut buf) else { break };
                let mut inner = shared.lock();
                // A socket replaced since (another protocol or session took over): not read on.
                if inner.socket.as_ref().map(|(o, _)| *o) != Some(*owner) {
                    break;
                }
                if let Owner::Plasmo(_) = owner {
                    if *owner != plasmo_owner(&inner) {
                        break;
                    }
                    let Some(p) = inner.plasmo.as_mut() else { break };
                    let todo = p.on_udp(&buf[..n], shared.now());
                    inner.udp_received += 1;
                    run_plasmo(&mut inner, todo);
                    continue;
                }
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
                            // Each frame names its range: shorter for a whisper (capture 07).
                            inner.mixer.push_ranged(s.sender, s.sequence, s.opus.clone(), s.distance as f64);
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
        if let Some(p) = inner.plasmo.as_mut() {
            let todo = p.poll(shared.now());
            run_plasmo(&mut inner, todo);
        }

        // Microphone: one Opus frame per 960 captured samples.
        loop {
            let frame: [f32; FRAME_SAMPLES] = {
                let Ok(mut q) = inner.capture.lock() else { break };
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
}

fn send_mic_frame(inner: &mut Inner, mut frame: [f32; FRAME_SAMPLES]) {
    if inner.muted || inner.session.disabled {
        return;
    }
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
    // The mic test hears the microphone as the others would, whether or not it's being sent.
    if inner.mic_test {
        inner.mixer.push_local(&frame);
    }
    // Holding the whisper key sends whatever the mode (as SVC's whisper key did in capture 07),
    // on a Simple Voice Chat connection only: checked here, at the moment of sending.
    let whisper = whispering(inner);
    let wants = inner.activation.frame(inner.level, inner.ptt_down) || whisper;
    // Each protocol sends on its own socket only.
    let owner = plasmo_owner(inner);
    let plasmo_on = inner.plasmo.as_ref().is_some_and(Plasmo::connected) && inner.socket.as_ref().is_some_and(|(o, _)| *o == owner);
    let svc_on = inner.session.is_connected() && inner.plasmo.is_none() && inner.socket.as_ref().is_some_and(|(o, _)| *o == Owner::Svc);
    let send = wants && (svc_on || plasmo_on);
    let was = inner.transmitting;
    inner.transmitting = send;
    if !send {
        // Plasmo Voice hears when we stop.
        if was && let Some(end) = inner.plasmo.as_ref().and_then(Plasmo::stopped) {
            inner.outbox.push((owner, plasmo::CHANNEL, end));
        }
        return;
    }
    // Plasmo Voice's MTU bounds the sealed packet: leave room for the IV and padding.
    let mtu = if plasmo_on { inner.plasmo_mtu.saturating_sub(64).max(64) } else { inner.session.secret().map(|s| s.mtu.max(1) as usize).unwrap_or(1275) };
    let Some(encoder) = inner.encoder.as_mut() else { return };
    let opus = match encoder.encode(&frame, mtu) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[earshot] {e}");
            return;
        }
    };
    if plasmo_on && let Some(p) = inner.plasmo.as_mut() {
        if let Some(packet) = p.audio(&opus) {
            inner.mic_frames_sent += 1;
            send_raw(inner, &packet);
        }
        return;
    }
    // The last byte: 1 while whispering, 0 otherwise (capture 07).
    let msg = Message::Mic(MicFrame { opus, sequence: inner.sequence, flag: whisper as u8 });
    inner.sequence += 1;
    inner.mic_frames_sent += 1;
    send_udp(inner, &msg);
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
    fn audio_devices_follow_mute_and_voice_chat() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let mut inner = client.shared.lock();
        inner.session.phase = Phase::Connected { last_heard: 0 };
        assert_eq!(inner.audio_devices(), (None, None));
        inner.settings.use_audio_devices = true;
        assert_eq!(inner.audio_devices(), (Some(None), Some(None)));
        inner.muted = true;
        assert_eq!(inner.audio_devices(), (None, Some(None)));
        inner.session.disabled = true;
        assert_eq!(inner.audio_devices(), (None, None));
        inner.muted = false;
        assert_eq!(inner.audio_devices(), (None, None));
        inner.session.disabled = false;
        assert_eq!(inner.audio_devices(), (Some(None), Some(None)));

        inner.session.reset();
        assert_eq!(inner.audio_devices(), (None, None));
        inner.mic_test = true;
        assert_eq!(inner.audio_devices(), (Some(None), Some(None)));
        inner.muted = true;
        assert_eq!(inner.audio_devices(), (None, None));
        inner.muted = false;
        inner.session.disabled = true;
        assert_eq!(inner.audio_devices(), (None, None));
        inner.settings.use_audio_devices = false;
    }

    #[test]
    fn voice_timeout_stops_audio_without_disabling_the_mic_test() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let mut inner = client.shared.lock();
        inner.settings.use_audio_devices = true;
        inner.target = Some((Owner::Svc, "127.0.0.1".into(), 1));
        inner.session.phase = Phase::Authenticating { since: 0, last_sent: 0 };
        let before = inner.capture.clone();
        assert_eq!(inner.audio_devices(), (Some(None), Some(None)));

        let actions = inner.session.poll(15_000);
        run_actions(&mut inner, actions);
        assert!(matches!(inner.session.phase, Phase::Failed(_)));
        assert_eq!(inner.audio_devices(), (None, None));
        assert!(!Arc::ptr_eq(&before, &inner.capture));

        inner.mic_test = true;
        assert_eq!(inner.audio_devices(), (Some(None), Some(None)));
        inner.settings.use_audio_devices = false;
    }

    #[test]
    fn player_volume_survives_leaving_a_server() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let player = Uuid([1; 16]);
        client.set_player_volume(player, 0.0);
        client.disconnect();
        assert_eq!(client.shared.lock().mixer.player_volume.get(&player), Some(&0.0));
    }

    #[test]
    fn a_microphone_opened_as_the_player_mutes_is_never_started() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let mut inner = client.shared.lock();
        inner.session.phase = Phase::Connected { last_heard: 0 };
        inner.settings.use_audio_devices = true;
        // The worker read this request, then opened the device while the player muted.
        let opening = inner.wanted_input().unwrap();
        drop(inner);
        client.set_muted(true);
        let mut played = false;
        let started = audio::start_if_wanted(&opening, client.shared.lock().wanted_input(), || {
            played = true;
            Ok(())
        });
        assert_eq!(started, Ok(false));
        assert!(!played, "a muted microphone's stream is never played");
        // Unmuted again with a fresh queue: the stream opened for the old one isn't started either.
        client.set_muted(false);
        let mut inner = client.shared.lock();
        assert_eq!(audio::start_if_wanted(&opening, inner.wanted_input(), || Ok(())), Ok(false));
        let fresh = inner.wanted_input().unwrap();
        assert_eq!(audio::start_if_wanted(&fresh, inner.wanted_input(), || Ok(())), Ok(true));
        inner.settings.use_audio_devices = false;
        inner.session.reset();
    }

    #[test]
    fn rapid_unmute_and_reenable_never_reuse_the_old_capture_queue() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let before_mute = client.shared.lock().capture.clone();
        client.set_muted(true);
        before_mute.lock().unwrap().push_back(1.0);
        client.set_muted(false);
        let before_disable = client.shared.lock().capture.clone();
        assert!(!Arc::ptr_eq(&before_mute, &before_disable));
        assert!(before_disable.lock().unwrap().is_empty());
        client.set_disabled(true);
        before_disable.lock().unwrap().push_back(1.0);
        client.set_disabled(false);
        let after_disable = client.shared.lock().capture.clone();
        assert!(!Arc::ptr_eq(&before_disable, &after_disable));
        assert!(after_disable.lock().unwrap().is_empty());
    }

    #[test]
    fn dropping_a_client_stops_its_worker_promptly() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let started = Instant::now();
        drop(client);
        assert!(started.elapsed() < STOP_WAIT, "the worker saw `stop` and was joined");
    }

    #[test]
    fn dropping_a_client_whose_worker_never_ends_does_not_hang_or_panic() {
        // As at process exit, where the OS has ended the worker: a handle that never finishes.
        let mut client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let (_keep, wait) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let real = client.worker.replace(stuck);
        let started = Instant::now();
        drop(client);
        let took = started.elapsed();
        assert!(took >= STOP_WAIT && took < STOP_WAIT * 4, "waited {took:?}");
        let _ = real.map(JoinHandle::join);
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
    fn connects_to_a_fake_plasmo_voice_server() {
        use crate::plasmo::{AudioCipher, fake_server as fake};
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let port = server.local_addr().unwrap().port();
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });

        // 1. The server's register: we register Plasmo Voice's channels back.
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(&[plasmo::CHANNEL]), "127.0.0.1");
        assert_eq!(client.take_outgoing(), [(plugin::MINECRAFT_REGISTER, plugin::encode_register(&[plasmo::CHANNEL, plasmo::INSTALLED]))]);

        // 2. Asked for our info (once the key is made), we send the public key.
        let deadline = Instant::now() + Duration::from_secs(60);
        while PLASMO_NEXT_KEY.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "no key");
            std::thread::sleep(Duration::from_millis(20));
        }
        client.on_plugin(plasmo::CHANNEL, &fake::request(), "127.0.0.1");
        let out = client.take_outgoing();
        assert_eq!(out.len(), 1);
        let public = fake::public_key_of(&out[0].1);

        // 3. The secret and address: the client pings the voice server.
        let secret = Uuid([5; 16]);
        client.on_plugin(plasmo::CHANNEL, &fake::connection(&secret, "0.0.0.0", port), "127.0.0.1");
        let mut buf = [0u8; 2048];
        let (n, from) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..4], &0x4e90_04e9i32.to_be_bytes());
        assert_eq!(buf[4], 1, "a ping");
        assert_eq!(&buf[5..21], &secret.0);
        assert!(n > 29, "with the address it reached");

        // 4. The config, its audio key sealed to our public key: connected.
        let aes_key = [3u8; 16];
        client.on_plugin(plasmo::CHANNEL, &fake::config_packet(&Uuid([1; 16]), &fake::seal_key(&public, &aes_key)), "127.0.0.1");
        wait_for("connected", || client.status().connected);
        assert!(client.status().connection.contains("Plasmo"));

        // 5. Alice's voice: once the server says the source is hers, it reaches the mixer as hers.
        let (source, alice) = (Uuid([7; 16]), Uuid([8; 16]));
        client.on_plugin(plasmo::CHANNEL, &fake::player_source(&source, &alice, "Alice"), "127.0.0.1");
        let cipher = AudioCipher::new(&aes_key).unwrap();
        let mut enc = Encoder::new(Codec::Voip).unwrap();
        for seq in 1..=5 {
            let opus = enc.encode(&[0.1; FRAME_SAMPLES], 1275).unwrap();
            server.send_to(&fake::source_audio(&secret, &cipher, seq, &source, &opus), from).unwrap();
        }
        wait_for("alice talking", || client.status().talking.contains(&alice));

        // 6. Muting tells the server.
        client.set_muted(true);
        assert_eq!(client.take_outgoing(), [(plasmo::CHANNEL, plasmo::player_state(false, true))]);

        // 7. Voice chat off: the connection and anything queued for it are dropped, and the
        // server's requests go unanswered.
        client.set_muted(false);
        client.set_disabled(true);
        assert!(client.take_outgoing().is_empty(), "queued state dropped, nothing new");
        assert!(!client.status().connected);
        {
            let inner = client.shared.lock();
            assert!(inner.socket.is_none() && inner.target.is_none() && inner.plasmo_key.is_none());
        }
        client.on_plugin(plasmo::CHANNEL, &fake::request(), "127.0.0.1");
        assert!(client.take_outgoing().is_empty(), "no reply while off");
        assert!(client.shared.lock().plasmo.is_none());

        // 8. On again: we introduce ourselves unasked (the server only asks on join), with a new key.
        let deadline = Instant::now() + Duration::from_secs(60);
        while PLASMO_NEXT_KEY.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "no second key");
            std::thread::sleep(Duration::from_millis(20));
        }
        client.set_disabled(false);
        let out = client.take_outgoing();
        assert_eq!(out.len(), 1);
        assert_ne!(fake::public_key_of(&out[0].1), public, "a fresh key for each connection");

        client.disconnect();
        assert!(!client.status().connected);
    }

    #[test]
    fn voice_off_never_starts_plasmo_voice() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        client.set_disabled(true);
        client.take_outgoing();
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(&[plasmo::CHANNEL]), "127.0.0.1");
        assert!(client.take_outgoing().is_empty(), "not registered while off");
        client.on_plugin(plasmo::CHANNEL, &plasmo::fake_server::request(), "127.0.0.1");
        assert!(client.take_outgoing().is_empty());
        assert!(client.shared.lock().plasmo.is_none());
        // Turned on, it registers, and the server's next request starts the connection.
        client.set_disabled(false);
        assert!(client.take_outgoing().contains(&(plugin::MINECRAFT_REGISTER, plugin::encode_register(&[plasmo::CHANNEL, plasmo::INSTALLED]))));
    }

    #[test]
    fn queued_plasmo_messages_never_outlive_voice_chat_or_their_session() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        // Simple Voice Chat's register and Plasmo Voice's, queued together.
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(&[plasmo::CHANNEL, "voicechat:secret"]), "h");
        client.set_disabled(true);
        let out = client.take_outgoing();
        assert!(out.iter().all(|(c, d)| *c != plasmo::CHANNEL && *d != plugin::encode_register(&[plasmo::CHANNEL, plasmo::INSTALLED])), "{out:?}");
        assert!(!client.shared.lock().plasmo_registered, "made again when voice is on");

        // Switched off after a message was queued but before the game took it: rechecked then.
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(&[plasmo::CHANNEL]), "h");
        client.shared.lock().session.disabled = true;
        assert!(client.take_outgoing().is_empty());
        client.shared.lock().session.disabled = false;

        // A message of an ended session is dropped, the new session's goes out.
        let mut inner = client.shared.lock();
        inner.outbox.clear();
        plasmo_start(&mut inner);
        let old = plasmo_owner(&inner);
        inner.outbox.push((old, plasmo::CHANNEL, vec![1]));
        inner.plasmo = None;
        plasmo_start(&mut inner);
        assert!(inner.outbox.is_empty(), "the old session's queue goes with it");
        inner.outbox.push((old, plasmo::CHANNEL, vec![1]));
        let new = plasmo_owner(&inner);
        inner.outbox.push((new, plasmo::CHANNEL, vec![2]));
        drop(inner);
        assert_eq!(client.take_outgoing(), [(plasmo::CHANNEL, vec![2])]);
    }

    #[test]
    fn each_protocol_sends_only_on_its_own_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let port = server.local_addr().unwrap().port();
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let mut inner = client.shared.lock();
        plasmo_start(&mut inner);
        // Simple Voice Chat's socket: Plasmo Voice's data never goes out on it.
        inner.socket = Some((Owner::Svc, open_socket("127.0.0.1", port).unwrap()));
        send_raw(&mut inner, b"plasmo");
        // An earlier Plasmo session's socket: not this session's either.
        inner.socket = Some((Owner::Plasmo(inner.plasmo_epoch - 1), open_socket("127.0.0.1", port).unwrap()));
        send_raw(&mut inner, b"plasmo");
        let mut buf = [0u8; 16];
        assert!(server.recv(&mut buf).is_err(), "nothing sent");
        // Its own: sent.
        inner.socket = Some((plasmo_owner(&inner), open_socket("127.0.0.1", port).unwrap()));
        send_raw(&mut inner, b"plasmo");
        assert_eq!(server.recv(&mut buf).unwrap(), 6);
    }

    #[test]
    fn whisper_is_checked_where_it_is_used() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        let mut inner = client.shared.lock();
        inner.whisper_down = true;
        inner.socket = Some((Owner::Svc, UdpSocket::bind("127.0.0.1:0").unwrap()));
        assert!(whispering(&inner), "Simple Voice Chat");
        // Plasmo Voice takes over before the next tick: the held key no longer counts.
        inner.plasmo_server = true;
        assert!(!whispering(&inner));
        inner.plasmo_server = false;
        plasmo_start(&mut inner);
        assert!(!inner.whisper_down && !whispering(&inner));
    }

    #[test]
    fn no_whisper_on_plasmo_voice() {
        let client = VoiceClient::new(Settings { use_audio_devices: false, ..Settings::default() });
        client.tick(None, vec![], false, true);
        assert!(client.shared.lock().whisper_down, "SVC: the whisper key works");
        client.on_plugin(plugin::MINECRAFT_REGISTER, &plugin::encode_register(&[plasmo::CHANNEL]), "127.0.0.1");
        client.tick(None, vec![], false, true);
        assert!(!client.shared.lock().whisper_down, "Plasmo Voice: it does nothing");
        assert!(!client.status().whispering);
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

        let joined = client.on_plugin(plugin::JOINED_GROUP, &from_hex("01 453d1ee3061047f2bfb227f017fc5782 00"), "h");
        assert_eq!(joined, Some(GroupEvent::Joined(group)));
        assert_eq!(client.status().my_group.map(|g| g.id), Some(group));

        // Leaving: joined_group 00 00, then the group goes away.
        assert_eq!(client.on_plugin(plugin::JOINED_GROUP, &from_hex("0000"), "h"), None);
        let removed = client.on_plugin(plugin::REMOVE_GROUP, &from_hex("453d1ee3061047f2bfb227f017fc5782"), "h");
        assert_eq!(removed, Some(GroupEvent::Removed(group)));
        assert_eq!(client.on_plugin(plugin::REMOVE_GROUP, &[1, 2], "h"), None, "an unreadable message changes nothing");
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
