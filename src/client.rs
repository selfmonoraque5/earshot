//! Earshot's client: the voice client's state between hooks, the settings it keeps, the
//! HUD, the name tag icons and the voice menu (V). Everything that doesn't need the game
//! is in `earshot_core` (the menu's layout and behaviour and the icons included:
//! `earshot_core::ui`).
//!
//! Earshot is an independent project, not affiliated with Simple Voice Chat.
use std::cell::RefCell;
use std::collections::HashMap;

use earshot_core::activation::{Mode, PttKey};
use earshot_core::engine::{GroupInfo, Settings, Status, VoiceClient};
use earshot_core::menu::{Menu, MenuAction, MenuKey, MenuModel};
use earshot_core::plugin;
use earshot_core::spatial::Listener;
use earshot_core::ui::{self, Draw, MicState};
use earshot_core::wire::Uuid;
use mod_api::{ClientHost, ConfigPath, Corner, Corners, DrawContext, Host, Identifier, ServerboundPlayPacket};

pub const ID: &str = "earshot";

/// The keys: id, English label, default.
pub const KEY_MENU: (&str, &str, &str) = ("menu", "Voice menu", "key.keyboard.v");
pub const KEY_PTT: (&str, &str, &str) = ("push_to_talk", "Push to talk", "key.mouse.4");
/// Held: talk as a whisper, heard only nearby (capture 07). The other side button by default.
pub const KEY_WHISPER: (&str, &str, &str) = ("whisper", "Whisper", "key.mouse.5");
pub const KEY_MUTE: (&str, &str, &str) = ("mute", "Mute microphone", "key.keyboard.m");
pub const KEY_DEAFEN: (&str, &str, &str) = ("deafen", "Deafen", "key.keyboard.n");
pub const KEYS: [(&str, &str, &str); 5] = [KEY_MENU, KEY_PTT, KEY_WHISPER, KEY_MUTE, KEY_DEAFEN];

/// The settings page's two switches (the menu holds the rest).
pub const SETTING_VOICE: &str = "voice_chat";
pub const SETTING_DEBUG: &str = "debug";

/// Players farther than this aren't listed in the menu (SVC's default voice distance).
const MENU_PLAYER_RANGE: f64 = 48.0;
/// The file the menu's settings and the players' volumes are kept in (the mod's data folder).
const SETTINGS_FILE: &str = "client.txt";

struct State {
    client: VoiceClient,
    settings: Settings,
    threshold: f32,
    /// The push-to-talk key (hold or toggle), and whether it says talk right now.
    ptt_key: PttKey,
    ptt: bool,
    /// The push-to-talk key's state last tick, to see presses and releases.
    ptt_down: bool,
    /// The whisper key is held.
    whisper: bool,
    /// The mic test is on (only while the menu is open).
    mic_test: bool,
    muted: bool,
    deafened: bool,
    disabled: bool,
    menu: Menu,
    /// Per-player volumes, kept in the settings file.
    volumes: HashMap<Uuid, f32>,
    /// Show the diagnostics (frame counters, UDP, devices). Off unless switched on.
    debug: bool,
    /// The settings file's other lines, as read.
    file: HashMap<String, String>,
    /// The players the voice server told of, by entity id, for the name tag icons.
    entities: HashMap<u32, Uuid>,
    /// The menu key was pressed on the menu: it closes on the next tick (closing a screen
    /// needs the tick's host).
    close_menu: bool,
}

thread_local! {
    // The window's game runs on one thread; the server's hooks never get here.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(host: &dyn ClientHost, f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(|| load(host));
        f(state)
    })
}

/// Only if the client already exists: drawing shouldn't start threads.
fn with_existing<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.borrow_mut().as_mut().map(f))
}

/// `key=value` lines.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn read_file(host: &dyn Host) -> HashMap<String, String> {
    let Ok(path) = ConfigPath::new(SETTINGS_FILE) else {
        return HashMap::new();
    };
    host.data_files()
        .read(&path)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|t| parse(&t))
        .unwrap_or_default()
}

fn load(host: &dyn ClientHost) -> State {
    let file = read_file(host);
    let get = |k: &str| file.get(k).cloned();
    let num = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let flag = |k: &str| get(k).as_deref() == Some("true");
    let threshold = num("threshold", -45.0);
    let settings = Settings {
        input_device: get("input_device").filter(|s| !s.is_empty()),
        output_device: get("output_device").filter(|s| !s.is_empty()),
        mic_volume: num("mic_volume", 1.0),
        output_volume: num("output_volume", 1.0),
        mode: if get("mode").as_deref() == Some("voice") { Mode::Voice { threshold_dbfs: threshold } } else { Mode::PushToTalk },
        noise_suppression: get("noise").as_deref() != Some("false"),
        group_volume: num("group_volume", 1.0),
        group_sounds: get("group_sounds").as_deref() != Some("false"),
        use_audio_devices: true,
    };
    let volumes = file
        .iter()
        .filter_map(|(k, v)| Some((Uuid::parse(k.strip_prefix("volume.")?)?, v.parse::<f32>().ok()?)))
        .collect::<HashMap<_, _>>();
    let client = VoiceClient::new(settings.clone());
    client.apply_settings(settings.clone());
    let (muted, deafened) = (flag("muted"), flag("deafened"));
    let disabled = !host.setting_bool(SETTING_VOICE).unwrap_or(true);
    let debug = host.setting_bool(SETTING_DEBUG).unwrap_or(false);
    client.set_muted(muted);
    client.set_deafened(deafened);
    client.set_disabled(disabled);
    for (id, v) in &volumes {
        client.set_player_volume(*id, *v);
    }
    let ptt_key = PttKey::new(flag("ptt_toggle"));
    State {
        client,
        settings,
        threshold,
        ptt_key,
        ptt: false,
        ptt_down: false,
        whisper: false,
        mic_test: false,
        muted,
        deafened,
        disabled,
        menu: Menu::new(),
        volumes,
        debug,
        file,
        entities: HashMap::new(),
        close_menu: false,
    }
}

/// Keeps the menu's settings and the players' volumes in the settings file.
fn save(host: &dyn Host, s: &mut State) {
    let mut set = |k: &str, v: String| {
        s.file.insert(k.to_owned(), v);
    };
    set("mode", if matches!(s.settings.mode, Mode::Voice { .. }) { "voice" } else { "ptt" }.into());
    set("threshold", s.threshold.to_string());
    set("mic_volume", s.settings.mic_volume.to_string());
    set("output_volume", s.settings.output_volume.to_string());
    set("input_device", s.settings.input_device.clone().unwrap_or_default());
    set("output_device", s.settings.output_device.clone().unwrap_or_default());
    set("noise", s.settings.noise_suppression.to_string());
    set("ptt_toggle", s.ptt_key.toggle.to_string());
    set("group_volume", s.settings.group_volume.to_string());
    set("group_sounds", s.settings.group_sounds.to_string());
    set("muted", s.muted.to_string());
    set("deafened", s.deafened.to_string());
    for (id, v) in &s.volumes {
        s.file.insert(format!("volume.{id}"), v.to_string());
    }
    let mut lines: Vec<String> = s.file.iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.sort();
    if let Ok(path) = ConfigPath::new(SETTINGS_FILE)
        && let Err(e) = host.data_files().write(&path, (lines.join("\n") + "\n").as_bytes())
    {
        eprintln!("[earshot] couldn't keep the settings: {e:?}");
    }
}

/// The host part of `host:port` (or `[v6]:port`), as the player typed it.
pub fn host_of(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host.to_string(),
        _ => address.to_string(),
    }
}

/// `key.keyboard.v` -> `V`, `key.mouse.4` -> `Mouse 4`.
pub fn key_label(key: &str) -> String {
    if let Some(n) = key.strip_prefix("key.mouse.") {
        return format!("Mouse {}", n.replace('.', " "));
    }
    let k = key.strip_prefix("key.keyboard.").unwrap_or(key);
    k.split('.')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A custom payload's packet body: the channel as a string (VarInt length, UTF-8), then
/// the bytes.
pub fn payload_body(channel: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(channel.len() + data.len() + 5);
    let mut n = channel.len() as u32;
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(channel.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Sends what the voice client wants to say over the game connection.
pub fn flush(host: &mut dyn ClientHost) {
    let Some(out) = with_existing(|s| s.client.take_outgoing()) else { return };
    let Ok(id) = Identifier::parse("minecraft:custom_payload") else { return };
    for (channel, data) in out {
        let packet = ServerboundPlayPacket::new(id.clone(), payload_body(channel, &data));
        if let Err(e) = host.send_packet(packet) {
            eprintln!("[earshot] couldn't send {channel}: {e:?}");
        }
    }
}

// --- connection -------------------------------------------------------------------------------

/// A custom payload from the server.
pub fn plugin_message(host: &mut dyn ClientHost, channel: &str, data: &[u8], server: &str) {
    if channel != plugin::MINECRAFT_REGISTER && !channel.starts_with("voicechat:") && !channel.starts_with("plasmo:voice/") {
        return;
    }
    let host_name = host_of(server);
    let menu_open = host.mod_screen() == Some(ID);
    with_state(&*host, |s| {
        // A confirmed join or group removal moves the open menu's form; nothing is scanned.
        if let Some(event) = s.client.on_plugin(channel, data, &host_name)
            && menu_open
        {
            s.menu.on_group_event(event);
        }
    });
    flush(host);
}

/// A chat line from the server, before it shows: voice chat lines worded as
/// `message.voicechat.` keys only its own client has text for get Earshot's own wording
/// (`earshot_core::chat`), with the server's arguments and click actions kept.
pub fn chat_line(host: &dyn ClientHost, message: &mut mod_api::Text) {
    let component = crate::text_json::to_json(message);
    let translated = |key: &str| host.translate(key).is_some();
    let Some(rewritten) = earshot_core::chat::rewrite(&component, &translated) else { return };
    if with_state(host, |s| s.debug) {
        eprintln!("[earshot] voice chat line from the server: {component}");
    }
    *message = crate::text_json::from_json(&rewritten);
}

/// The players the voice client knows of: their entity, and where their voice comes from.
fn players(host: &dyn ClientHost, status: &Status) -> Vec<(Uuid, u32, [f64; 3])> {
    let mut ids: Vec<Uuid> = status.has_voice.iter().chain(&status.voice_off).chain(&status.talking).copied().collect();
    if let Some(g) = &status.my_group {
        ids.extend(g.members.iter().map(|m| m.0));
    }
    ids.sort();
    ids.dedup();
    let reads: &dyn Host = host;
    ids.into_iter()
        .filter(|id| Some(*id) != status.me)
        .filter_map(|id| {
            let entity = reads.player(u128::from_be_bytes(id.0))?.entity();
            let pos = entity.position(reads)?;
            let height = entity.dimensions(reads).map_or(1.8, |d| d[1]);
            // Voices come from the head, not the feet.
            Some((id, entity.id(), [pos[0], pos[1] + height * 0.9, pos[2]]))
        })
        .collect()
}

/// Once a tick: the keys, positions, push-to-talk, and anything to send.
pub fn tick(host: &mut dyn ClientHost) {
    let reads: &dyn ClientHost = &*host;
    let playing = reads.playing();
    let menu_open = reads.mod_screen() == Some(ID);
    let listener = reads.camera().map(|c| Listener { pos: c.position, yaw_degrees: c.yaw });
    let ptt_down = reads.is_down(ID, KEY_PTT.0);
    let whisper_down = reads.is_down(ID, KEY_WHISPER.0);
    let status = with_state(reads, |s| s.client.status());
    let players = players(reads, &status);
    with_state(reads, |s| {
        // The mic test lasts only while the menu is open.
        if s.mic_test && !menu_open {
            s.mic_test = false;
            s.client.set_mic_test(false);
        }
        // Typing in chat or a sign mustn't key the microphone: presses count only while playing.
        if ptt_down != s.ptt_down {
            s.ptt_key.key(ptt_down, playing);
            s.ptt_down = ptt_down;
        }
        s.ptt = s.ptt_key.is_on();
        s.whisper = whisper_down && playing;
        s.entities = players.iter().map(|p| (p.1, p.0)).collect();
        let positions = players.iter().map(|p| (p.0, p.2)).collect();
        s.client.tick(listener, positions, s.ptt, s.whisper);
    });
    if with_existing(|s| std::mem::take(&mut s.close_menu)) == Some(true) {
        host.mod_screen_close(ID);
    }
    // The keys pressed while playing: the menu, mute and deafen.
    if host.consume_click(ID, KEY_MENU.0) {
        with_state(&*host, |s| {
            s.ptt_key.release();
            s.ptt = false;
            s.whisper = false;
            // Start on the main screen each time.
            s.menu.open();
        });
        host.mod_screen_open(ID);
    }
    let mut changed = false;
    while host.consume_click(ID, KEY_MUTE.0) {
        with_state(&*host, |s| {
            s.muted = !s.muted;
            s.client.set_muted(s.muted);
        });
        changed = true;
    }
    while host.consume_click(ID, KEY_DEAFEN.0) {
        with_state(&*host, |s| {
            s.deafened = !s.deafened;
            s.client.set_deafened(s.deafened);
        });
        changed = true;
    }
    if changed {
        let reads: &dyn Host = &*host;
        let _ = with_existing(|s| save(reads, s));
    }
    flush(host);
}

/// The settings page changed: voice on or off, debug info.
pub fn page_changed(host: &dyn ClientHost, key: &str) {
    with_state(host, |s| match key {
        SETTING_VOICE => {
            s.disabled = !host.setting_bool(SETTING_VOICE).unwrap_or(true);
            s.client.set_disabled(s.disabled);
        }
        SETTING_DEBUG => s.debug = host.setting_bool(SETTING_DEBUG).unwrap_or(false),
        _ => {}
    });
}

/// Before the connection closes.
pub fn disconnecting() {
    let _ = with_existing(|s| {
        s.ptt_key.release();
        s.ptt = false;
        s.whisper = false;
        let _ = s.menu.release();
        s.client.disconnect();
        s.entities.clear();
    });
}

// --- HUD and name tags --------------------------------------------------------------------------

fn width(host: &dyn ClientHost, text: &str) -> f32 {
    host.text_width(&mod_api::Text::literal(text)).map_or(text.len() as f32 * 6.0, |w| w as f32)
}

fn paint(draw: &mut dyn DrawContext, draws: &[Draw]) {
    for d in draws {
        match d {
            Draw::Fill { rect, argb } => draw.fill(*rect, *argb),
            Draw::Quad { points, argb } => draw.quad(*points, *argb),
            Draw::Label { text, at, rgb } => draw.text(text, *at, 0xFF00_0000 | rgb, true),
        }
    }
}

fn mic_state(s: &Status) -> MicState {
    if s.disabled {
        MicState::Off
    } else if s.muted {
        MicState::Muted
    } else if s.whispering {
        MicState::Whispering
    } else if s.transmitting {
        MicState::Talking
    } else {
        MicState::Idle
    }
}

/// The HUD: the microphone icon bottom left, a line while connecting, and in a voice
/// group the group panel top left (its members, who's talking lit up).
pub fn hud(host: &dyn ClientHost, draw: &mut dyn DrawContext, corners: &mut Corners) {
    let Some(status) = with_existing(|s| s.client.status()) else { return };
    if status.connection == "no voice server" {
        return;
    }
    if !status.connected {
        let text = format!("Voice: {}", status.connection);
        let w = width(host, &text) + 8.0;
        let (x, y) = corners.place(Corner::TopLeft, w, 13.0);
        draw.fill([x, y, w, 13.0], 0x7800_0000);
        draw.text(&text, [x + 4.0, y + 3.0], 0xFFFF_AA55, true);
        return;
    }
    let state = mic_state(&status);
    let icons = if status.deafened { 2.0 } else { 1.0 };
    let group = status.my_group.as_ref().map(|g| format!("Group: {}", g.name));
    let text_w = group.as_ref().map(|t| width(host, t) + 8.0).unwrap_or(0.0);
    let (x, y) = corners.place(Corner::BottomLeft, 22.0 * icons + text_w, 20.0);
    let mut draws = ui::mic_icon([x + 2.0, y + 2.0], state);
    if status.deafened {
        draws.extend(ui::deafened_icon([x + 24.0, y + 2.0]));
    }
    paint(draw, &draws);
    if let Some(text) = group {
        let tx = x + 22.0 * icons + 2.0;
        draw.fill([tx - 2.0, y, text_w, 20.0], 0x7800_0000);
        draw.text(&text, [tx + 2.0, y + 6.0], 0xFF7C_E38B, true);
    }
    group_panel(host, draw, corners, &status);
}

/// In a voice group: its members top left, those talking now lit up with a speaker.
fn group_panel(host: &dyn ClientHost, draw: &mut dyn DrawContext, corners: &mut Corners, status: &Status) {
    let Some(group) = &status.my_group else { return };
    let members: Vec<ui::PanelMember> = group
        .members
        .iter()
        .map(|(id, name)| {
            let me = status.me == Some(*id);
            // Our own voice isn't played back to us: we're talking while the microphone sends.
            let talking = if me { status.transmitting } else { status.talking.contains(id) };
            ui::PanelMember { name: name.clone(), talking, me }
        })
        .collect();
    let (size, draws) = ui::group_panel(&group.name, &members, &|t| width(host, t));
    let (x, y) = corners.place(Corner::TopLeft, size[0], size[1]);
    paint(draw, &ui::translate(draws, [x, y]));
}

/// The icon beside player `entity`'s name tag: a green speaker while they talk, a red one
/// if they turned voice chat off, a grey crossed-out one if they aren't connected to voice
/// (on a server that runs it) (`ui::tag_icon_for`). Its lines, in tag font pixels from the
/// icon's centre, and their colour.
pub fn tag_icon(entity: u32) -> Option<(Vec<([f32; 2], [f32; 2])>, u32)> {
    with_existing(|s| {
        let status = s.client.status();
        let on_voice_server = status.connected;
        let id = s.entities.get(&entity).copied();
        let icon = match id {
            Some(id) => ui::tag_icon_for(
                status.talking.contains(&id),
                status.voice_off.contains(&id),
                status.has_voice.contains(&id),
                on_voice_server,
            ),
            None if on_voice_server => ui::tag_icon_for(false, false, false, true),
            None => None,
        }?;
        let (lines, argb) = ui::tag_icon(icon);
        Some((lines, argb))
    })
    .flatten()
}

// --- the voice menu (V) ----------------------------------------------------------------------

pub fn ours(host: &dyn ClientHost) -> bool {
    host.mod_screen() == Some(ID)
}

/// Nearby players for the menu, nearest first: the players the voice server told of.
fn nearby_players(host: &dyn ClientHost, status: &Status) -> Vec<(Uuid, String, f64)> {
    let Some(camera) = host.camera() else { return vec![] };
    let eye = camera.position;
    let reads: &dyn Host = host;
    let mut out: Vec<(Uuid, String, f64)> = players(host, status)
        .into_iter()
        .filter_map(|(id, _, at)| {
            let d = ((at[0] - eye[0]).powi(2) + (at[1] - eye[1]).powi(2) + (at[2] - eye[2]).powi(2)).sqrt();
            let name = reads
                .player(u128::from_be_bytes(id.0))
                .and_then(|p| p.entity().name(reads))
                .unwrap_or_else(|| id.to_string());
            (d <= MENU_PLAYER_RANGE).then_some((id, name, d))
        })
        .collect();
    out.sort_by(|a, b| a.2.total_cmp(&b.2));
    out
}

fn device_label(chosen: &Option<String>, in_use: &str) -> String {
    match chosen {
        None => format!("system default{}", if in_use.is_empty() { String::new() } else { format!(" ({in_use})") }),
        Some(name) if in_use.contains("failed") || in_use.starts_with("none") => format!("{name} - {in_use}"),
        Some(name) => name.clone(),
    }
}

/// A group's name for the list, with its type when it isn't normal.
fn group_label(g: &GroupInfo) -> String {
    match g.kind {
        plugin::GroupType::Normal => g.name.clone(),
        kind => format!("{} ({})", g.name, kind.label().to_lowercase()),
    }
}

fn model(host: &dyn ClientHost, s: &State) -> MenuModel {
    let status = s.client.status();
    let players = nearby_players(host, &status)
        .into_iter()
        .map(|(id, name, _)| {
            let v = s.volumes.get(&id).copied().unwrap_or(1.0);
            (id, name, v, status.talking.contains(&id))
        })
        .collect();
    let connection = if status.connected && s.debug {
        format!("Connected - sent {} / heard {} voice frames", status.mic_frames_sent, status.voice_frames_received)
    } else if status.connected {
        "Connected".to_string()
    } else {
        format!("Voice: {}", status.connection)
    };
    let ptt_key = host.mod_key(ID, KEY_PTT.0).map_or_else(|| KEY_PTT.2.to_owned(), |k| k.0);
    MenuModel {
        status: connection,
        connected: status.connected,
        muted: s.muted,
        deafened: s.deafened,
        disabled: s.disabled,
        push_to_talk: matches!(s.settings.mode, Mode::PushToTalk),
        ptt_toggle: s.ptt_key.toggle,
        mic_test: s.mic_test,
        group_volume: s.settings.group_volume,
        group_sounds: s.settings.group_sounds,
        ptt_key: key_label(&ptt_key),
        threshold_db: s.threshold,
        level_db: status.input_level_dbfs,
        noise_suppression: s.settings.noise_suppression,
        mic_volume: s.settings.mic_volume,
        voice_volume: s.settings.output_volume,
        microphone: device_label(&s.settings.input_device, &status.input_device),
        speakers: device_label(&s.settings.output_device, &status.output_device),
        players,
        group: status.my_group.as_ref().map(|g| g.name.clone()),
        groups: status
            .groups
            .iter()
            // A group that isn't normal says its type: "Raid (open)".
            .map(|g| (g.id, group_label(g), g.has_password, g.members.len(), status.my_group.as_ref().is_some_and(|m| m.id == g.id)))
            .collect(),
        group_members: status
            .my_group
            .as_ref()
            .map(|g| g.members.iter().map(|(id, name)| (*id, name.clone(), status.talking.contains(id))).collect())
            .unwrap_or_default(),
        group_problem: status.group_problem.clone(),
    }
}

fn next_device(devices: Vec<String>, current: &Option<String>) -> Option<String> {
    // System default, then each device in turn.
    let mut all: Vec<Option<String>> = vec![None];
    all.extend(devices.into_iter().map(Some));
    let i = all.iter().position(|d| d == current).map(|i| i + 1).unwrap_or(0);
    all[i % all.len()].clone()
}

/// Applies a menu action. Returns whether it's a change worth saving now (sliders save when
/// let go instead, so a drag doesn't write the settings dozens of times).
fn apply(host: &dyn Host, s: &mut State, action: MenuAction) -> bool {
    let mut settings_changed = true;
    let save_now = match action {
        MenuAction::ToggleMute => {
            s.muted = !s.muted;
            s.client.set_muted(s.muted);
            settings_changed = false;
            true
        }
        MenuAction::ToggleDeafen => {
            s.deafened = !s.deafened;
            s.client.set_deafened(s.deafened);
            settings_changed = false;
            true
        }
        MenuAction::ToggleVoiceChat => {
            s.disabled = !s.disabled;
            s.client.set_disabled(s.disabled);
            host.set_setting(ID, SETTING_VOICE, if s.disabled { "false" } else { "true" });
            settings_changed = false;
            false
        }
        MenuAction::ToggleMode => {
            s.settings.mode = match s.settings.mode {
                Mode::PushToTalk => Mode::Voice { threshold_dbfs: s.threshold },
                Mode::Voice { .. } => Mode::PushToTalk,
            };
            true
        }
        MenuAction::ToggleNoiseSuppression => {
            s.settings.noise_suppression = !s.settings.noise_suppression;
            true
        }
        MenuAction::TogglePttStyle => {
            s.ptt_key = PttKey::new(!s.ptt_key.toggle);
            s.ptt = false;
            settings_changed = false;
            true
        }
        MenuAction::ToggleMicTest => {
            s.mic_test = !s.mic_test;
            s.client.set_mic_test(s.mic_test);
            settings_changed = false;
            false
        }
        MenuAction::ToggleGroupSounds => {
            s.settings.group_sounds = !s.settings.group_sounds;
            true
        }
        MenuAction::SetGroupVolume(v) => {
            s.settings.group_volume = v;
            false
        }
        MenuAction::SetThreshold(v) => {
            s.threshold = v;
            if let Mode::Voice { .. } = s.settings.mode {
                s.settings.mode = Mode::Voice { threshold_dbfs: v };
            }
            false
        }
        MenuAction::SetMicVolume(v) => {
            s.settings.mic_volume = v;
            false
        }
        MenuAction::SetVoiceVolume(v) => {
            s.settings.output_volume = v;
            false
        }
        MenuAction::NextMicrophone => {
            s.settings.input_device = next_device(earshot_core::audio::input_devices(), &s.settings.input_device);
            true
        }
        MenuAction::NextSpeakers => {
            s.settings.output_device = next_device(earshot_core::audio::output_devices(), &s.settings.output_device);
            true
        }
        MenuAction::JoinGroup(id, password) => {
            s.client.join_group(id, password.as_deref());
            settings_changed = false;
            false
        }
        MenuAction::CreateGroup { name, password, kind } => {
            s.client.create_group(&name, password.as_deref(), kind);
            settings_changed = false;
            false
        }
        MenuAction::LeaveGroup => {
            s.client.leave_group();
            settings_changed = false;
            false
        }
        MenuAction::SetPlayerVolume(id, v) => {
            s.volumes.insert(id, v);
            s.client.set_player_volume(id, v);
            settings_changed = false;
            false
        }
    };
    if settings_changed {
        s.client.apply_settings(s.settings.clone());
    }
    save_now
}

/// The GUI's size in GUI pixels.
fn gui(draw_size: Option<[f32; 2]>, screen_rect: [f32; 4]) -> [f32; 2] {
    draw_size.unwrap_or([screen_rect[0] + screen_rect[2], screen_rect[1] + screen_rect[3]])
}

/// Draws the menu.
pub fn screen_render(host: &dyn ClientHost, draw: &mut dyn DrawContext, mouse: [f32; 2]) {
    let size = draw.size();
    let draws = with_state(host, |s| {
        let m = model(host, s);
        s.menu.draw(&m, size, mouse, &|t: &str| width(host, t))
    });
    paint(draw, &draws);
}

/// A click in the menu: whether it was the menu's.
pub fn screen_press(host: &dyn ClientHost, rect: [f32; 4], mouse: [f32; 2]) -> bool {
    let size = gui(None, rect);
    with_state(host, |s| {
        let m = model(host, s);
        let inside = s.menu.contains(&m, size, mouse);
        if let Some(action) = s.menu.press(&m, size, mouse)
            && apply(host, s, action)
        {
            save(host, s);
        }
        inside
    })
}

/// A drag in the menu: sliders follow it. Whether a slider took it.
pub fn screen_drag(host: &dyn ClientHost, rect: [f32; 4], mouse: [f32; 2]) -> bool {
    let size = gui(None, rect);
    with_state(host, |s| {
        let m = model(host, s);
        match s.menu.drag(&m, size, mouse) {
            Some(action) => {
                apply(host, s, action);
                true
            }
            None => false,
        }
    })
}

/// The button came up: a slider was let go (whether one was).
pub fn screen_release(host: &dyn ClientHost) -> bool {
    with_existing(|s| {
        let dragging = s.menu.release();
        if dragging {
            save(host, s);
        }
        dragging
    })
    .unwrap_or(false)
}

/// A key on the menu: whether it is the menu's. While a text box has focus the keys are the
/// box's: Backspace, Enter and Tab work it, every other key is held back from the game (and
/// from Earshot's own keys), so typing a group name doesn't act on anything. Escape still
/// closes the menu, and the menu key closes it when nothing is being typed.
pub fn screen_key(host: &dyn ClientHost, key: &str) -> bool {
    let typing = with_state(host, |s| s.menu.typing());
    if !typing {
        let menu_key = host.mod_key(ID, KEY_MENU.0).map_or_else(|| KEY_MENU.2.to_owned(), |k| k.0);
        if key == menu_key {
            with_state(host, |s| s.close_menu = true);
            return true;
        }
        return false;
    }
    with_state(host, |s| {
        let menu_key = match key {
            "key.keyboard.backspace" => Some(MenuKey::Backspace),
            "key.keyboard.enter" | "key.keyboard.keypad.enter" => Some(MenuKey::Enter),
            "key.keyboard.tab" => Some(MenuKey::Tab),
            "key.keyboard.escape" => {
                let _ = s.menu.key(MenuKey::Escape);
                return false;
            }
            _ => None,
        };
        if let Some(k) = menu_key
            && let Some(action) = s.menu.key(k)
            && apply(host, s, action)
        {
            save(host, s);
        }
        true
    })
}

/// A character typed on the menu: into the focused text box (whether there was one).
pub fn screen_char(c: char) -> bool {
    with_existing(|s| {
        if s.menu.typing() {
            s.menu.type_char(c);
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mod_api::prelude::{ConfigClientHost, ConfigHost};
    use mod_api::{ConfigEntry, ConfigFileError, ConfigFiles, ConfigStat};
    use std::cell::Cell;

    struct EmptyData;

    impl ConfigFiles for EmptyData {
        fn read(&self, _: &ConfigPath) -> Result<Vec<u8>, ConfigFileError> {
            Ok(Vec::new())
        }

        fn write(&self, _: &ConfigPath, _: &[u8]) -> Result<(), ConfigFileError> {
            unreachable!()
        }

        fn list(&self, _: &ConfigPath, _: Option<&str>, _: u32) -> Result<Vec<ConfigEntry>, ConfigFileError> {
            unreachable!()
        }

        fn stat(&self, _: &ConfigPath) -> Result<Option<ConfigStat>, ConfigFileError> {
            unreachable!()
        }
    }

    struct SettingsHost {
        voice: Cell<bool>,
        debug: Cell<bool>,
    }

    impl Host for SettingsHost {}
    mod_api::impl_host_areas!(SettingsHost, except(config));
    impl ClientHost for SettingsHost {}
    mod_api::impl_client_host_areas!(SettingsHost, except(config));

    impl ConfigHost for SettingsHost {
        fn data_files(&self) -> &'static dyn ConfigFiles {
            &EmptyData
        }

        fn set_setting(&self, mod_id: &str, key: &str, value: &str) {
            assert_eq!((mod_id, key), (ID, SETTING_VOICE));
            self.voice.set(value.parse().unwrap());
        }
    }

    impl ConfigClientHost for SettingsHost {
        fn setting_bool(&self, key: &str) -> Option<bool> {
            match key {
                SETTING_VOICE => Some(self.voice.get()),
                SETTING_DEBUG => Some(self.debug.get()),
                _ => None,
            }
        }
    }

    #[test]
    fn the_menu_and_mod_config_share_the_saved_voice_switch() {
        let host = SettingsHost { voice: Cell::new(true), debug: Cell::new(false) };
        let mut state = load(&host);
        assert!(!state.client.status().disabled);
        apply(&host, &mut state, MenuAction::ToggleVoiceChat);
        assert!(state.client.status().disabled);
        assert!(!host.voice.get());

        let restarted = load(&host);
        assert!(restarted.client.status().disabled);
        STATE.with(|slot| *slot.borrow_mut() = Some(restarted));
        host.debug.set(true);
        page_changed(&host, SETTING_DEBUG);
        with_existing(|s| {
            assert!(s.debug);
            assert!(s.client.status().disabled);
        }).unwrap();

        host.voice.set(true);
        page_changed(&host, SETTING_VOICE);
        assert_eq!(with_existing(|s| s.client.status().disabled), Some(false));
        STATE.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    fn addresses_keys_and_payloads() {
        assert_eq!(host_of("play.example.org:25565"), "play.example.org");
        assert_eq!(host_of("[::1]:25565"), "::1");
        assert_eq!(host_of("example.org"), "example.org");
        assert_eq!(key_label("key.keyboard.v"), "V");
        assert_eq!(key_label("key.mouse.4"), "Mouse 4");
        assert_eq!(key_label("key.keyboard.left.shift"), "Left Shift");
        let body = payload_body("voicechat:ping", &[1, 2]);
        assert_eq!(body[0] as usize, "voicechat:ping".len());
        assert_eq!(&body[1..15], b"voicechat:ping");
        assert_eq!(&body[15..], &[1, 2]);
        // A channel past 127 bytes takes two VarInt bytes.
        let long = "a".repeat(200);
        assert_eq!(&payload_body(&long, &[])[..2], &[0xC8, 0x01]);
    }

    #[test]
    fn the_settings_file_is_key_value_lines() {
        let f = parse("mode=voice\nthreshold = -40\nbroken\nvolume.0=0.5\n");
        assert_eq!(f.get("mode").map(String::as_str), Some("voice"));
        assert_eq!(f.get("threshold").map(String::as_str), Some("-40"));
        assert_eq!(f.len(), 3);
    }

    #[test]
    fn devices_cycle_through_the_default() {
        let devices = || vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(next_device(devices(), &None), Some("A".to_owned()));
        assert_eq!(next_device(devices(), &Some("B".to_owned())), None);
        assert_eq!(next_device(devices(), &Some("gone".to_owned())), None);
    }
}
