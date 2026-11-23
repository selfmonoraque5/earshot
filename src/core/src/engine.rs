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
        let voice = matches!(self.session.phase, Phase::Authenticating { .. } | Phase::Checking { .. } | Phase::Connected { .. });
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

impl VoiceClient {
    pub fn new(settings: Settings) -> Self {
        let inner = Inner {
            groups: Vec::new(),
            my_group: None,
            players: std::collections::HashMap::new(),
            voice_flags: std::collections::HashMap::new(),
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
            inner.session.on_server_register(&plugin::decode_register(data), now)
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
        inner.whisper_down = whisper_down;
    }

    /// Plugin messages to send over the game connection now.
    pub fn take_outgoing(&self) -> Vec<(&'static str, Vec<u8>)> {
        std::mem::take(&mut self.shared.lock().outbox)
    }

    pub fn set_muted(&self, muted: bool) {
        let mut inner = self.shared.lock();
        if muted && !inner.muted {
            inner.reset_capture();
        }
        inner.muted = muted;
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
            inner.outbox.push((channel, data));
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
                let to = target.as_ref().map(|(h, p)| format!("{h}:{p}")).unwrap_or_else(|| "no address".into());
                let err = inner.udp_error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default();
                format!("{connection} [UDP {to}: sent {}, got {}{err}]", inner.udp_sent, inner.udp_received)
            }
        };
        Status {
            connection,
            connected: inner.session.is_connected(),
            transmitting: inner.transmitting,
            whispering: inner.transmitting && inner.whisper_down,
            talking: inner.mixer.talking(),
            me: inner.session.player(),
            voice_off: inner.voice_flags.iter().filter(|(_, f)| f.0).map(|(id, _)| *id).collect(),
            has_voice: inner.voice_flags.iter().filter(|(_, f)| !f.1).map(|(id, _)| *id).collect(),
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
                inner.reset_capture();
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
            if inner.socket.is_none() && matches!(inner.session.phase, Phase::Authenticating { .. }) {
                if let Some((host, port)) = inner.target.clone() {
                    drop(inner);
                    let opened = open_socket(&host, port);
                    inner = shared.lock();
                    if !matches!(inner.session.phase, Phase::Authenticating { .. })
                        || !inner.target.as_ref().is_some_and(|(h, p)| *h == host && *p == port)
                    {
                        continue;
                    }
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
    // Holding the whisper key sends whatever the mode (as SVC's whisper key did in capture 07).
    let wants = inner.activation.frame(inner.level, inner.ptt_down) || inner.whisper_down;
    let send = wants && inner.session.is_connected();
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
    // The last byte: 1 while whispering, 0 otherwise (capture 07).
    let msg = Message::Mic(MicFrame { opus, sequence: inner.sequence, flag: inner.whisper_down as u8 });
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
        inner.target = Some(("127.0.0.1".into(), 1));
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
