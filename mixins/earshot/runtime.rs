// Earshot's game glue, injected as `crate::game::earshot`. The helpers next to this file
// each call one function here; everything that doesn't need the game is in `earshot-core`
// (the menu's layout and behaviour and the icons included: `earshot_core::ui`).
// (Support modules can't have inner attributes or `//!` comments, hence plain comments.)
//
// Earshot is an independent project, not affiliated with Simple Voice Chat.

use std::cell::RefCell;
use std::collections::HashMap;

use earshot_core::activation::Mode;
use earshot_core::engine::{Settings, Status, VoiceClient};
use earshot_core::plugin;
use earshot_core::spatial::Listener;
use earshot_core::menu::{Menu, MenuAction, MenuKey, MenuModel};
use earshot_core::ui::{self, Draw, MicState};
use earshot_core::wire::Uuid;
use horizon_client::hud::HudBatch;
use horizon_client::mod_api::{ClientboundPlayPacket, Corner, Corners, NativeMod, NativeScreenView, ServerboundPlayPacket};
use horizon_client::world_primitives::WorldPrimitives;
use horizon_protocol::packets::configuration::CustomPayload;

use super::Game;

pub const ID: &str = "earshot";
const VERSION: &str = "1.0.1";

const KEY_MENU: (&str, &str, &str) = ("menu", "Voice menu", "key.keyboard.v");
const KEY_PTT: (&str, &str, &str) = ("push_to_talk", "Push to talk", "key.mouse.4");
const KEY_MUTE: (&str, &str, &str) = ("mute", "Mute microphone", "key.keyboard.m");
const KEY_DEAFEN: (&str, &str, &str) = ("deafen", "Deafen", "key.keyboard.n");

/// Players farther than this aren't listed in the menu (SVC's default voice distance).
const MENU_PLAYER_RANGE: f64 = 48.0;

struct State {
    client: VoiceClient,
    settings: Settings,
    threshold: f32,
    ptt: bool,
    muted: bool,
    deafened: bool,
    disabled: bool,
    menu: Menu,
    /// Per-player volumes, read from the settings as players show up.
    volumes: HashMap<Uuid, f32>,
    /// Show the diagnostics (frame counters, UDP, devices). Off unless switched on.
    debug: bool,
}

thread_local! {
    // The window's game runs on one thread; the server thread's `Game` never gets here
    // (every entry point checks `native_window`).
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(game: &Game<'_>, f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(|| load(game));
        f(state)
    })
}

/// Only if the client already exists: drawing shouldn't start threads.
fn with_existing<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.borrow_mut().as_mut().map(f))
}

fn load(game: &Game<'_>) -> State {
    let get = |k: &str| game.native_setting(ID, k);
    let num = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let threshold = num("threshold", -45.0);
    let settings = Settings {
        input_device: get("input_device").filter(|s| !s.is_empty()),
        output_device: get("output_device").filter(|s| !s.is_empty()),
        mic_volume: num("mic_volume", 1.0),
        output_volume: num("output_volume", 1.0),
        mode: if get("mode").as_deref() == Some("voice") { Mode::Voice { threshold_dbfs: threshold } } else { Mode::PushToTalk },
        noise_suppression: get("noise").as_deref() != Some("false"),
        use_audio_devices: true,
    };
    let flag = |k: &str| get(k).as_deref() == Some("true");
    let client = VoiceClient::new(settings.clone());
    client.apply_settings(settings.clone());
    let (muted, deafened, disabled) = (flag("muted"), flag("deafened"), flag("disabled"));
    client.set_muted(muted);
    client.set_deafened(deafened);
    client.set_disabled(disabled);
    let debug = flag("debug");
    State { client, settings, threshold, ptt: false, muted, deafened, disabled, menu: Menu::new(), volumes: HashMap::new(), debug }
}

fn save(game: &Game<'_>, s: &State) {
    let set = |k: &str, v: &str| game.native_set_setting(ID, k, v);
    set("mode", if matches!(s.settings.mode, Mode::Voice { .. }) { "voice" } else { "ptt" });
    set("threshold", &s.threshold.to_string());
    set("mic_volume", &s.settings.mic_volume.to_string());
    set("output_volume", &s.settings.output_volume.to_string());
    set("input_device", s.settings.input_device.as_deref().unwrap_or(""));
    set("output_device", s.settings.output_device.as_deref().unwrap_or(""));
    set("noise", &s.settings.noise_suppression.to_string());
    set("muted", &s.muted.to_string());
    set("deafened", &s.deafened.to_string());
    set("disabled", &s.disabled.to_string());
    set("debug", &s.debug.to_string());
}

/// The host part of `host:port` (or `[v6]:port`), as the player typed it.
fn host_of(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host.to_string(),
        _ => address.to_string(),
    }
}

/// `key.keyboard.v` -> `V`, `key.mouse.4` -> `Mouse 4`.
fn key_label(key: &str) -> String {
    if let Some(n) = key.strip_prefix("key.mouse.") {
        return format!("Mouse {}", n.replace('.', " "));
    }
    let k = key.strip_prefix("key.keyboard.").unwrap_or(key);
    k.split('.').map(|w| {
        let mut c = w.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    }).collect::<Vec<_>>().join(" ")
}

/// Sends what the voice client wants to say over the game connection.
fn flush(game: &mut Game<'_>) {
    let Some(out) = with_existing(|s| s.client.take_outgoing()) else { return };
    for (channel, data) in out {
        let packet = ServerboundPlayPacket::CustomPayload(CustomPayload { channel: channel.into(), data });
        if let Err(e) = game.native_send_packet(packet) {
            eprintln!("[earshot] couldn't send {channel}: {e}");
        }
    }
}

/// Where we see and hear from: eye position, yaw and pitch (degrees).
/// From the client state, not `native_camera`: Horizon only records the camera when typed-API
/// mods are loaded, so for a mixin mod like this one it's always empty.
fn view(game: &Game<'_>) -> Option<([f64; 3], f32, f32)> {
    let s = game.native_client_state()?;
    Some((s.eye_position?, s.yaw, s.pitch))
}

fn gui_size(game: &Game<'_>) -> [f32; 2] {
    game.native_client_state().map(|s| s.gui_size).unwrap_or([480.0, 270.0])
}

/// A player's saved volume, read once.
fn player_volume(game: &Game<'_>, s: &mut State, id: Uuid) -> f32 {
    if let Some(v) = s.volumes.get(&id) {
        return *v;
    }
    let v = game.native_setting(ID, &format!("volume.{id}")).and_then(|v| v.parse().ok()).unwrap_or(1.0);
    s.volumes.insert(id, v);
    s.client.set_player_volume(id, v);
    v
}

// --- hooks: connection --------------------------------------------------------------------

/// Every play packet from the server (`native_client_packet`).
pub fn client_packet(game: &mut Game<'_>, packet: &ClientboundPlayPacket) {
    if !game.native_window() {
        return;
    }
    let ClientboundPlayPacket::CustomPayload(p) = packet else { return };
    if p.channel != plugin::MINECRAFT_REGISTER && !p.channel.starts_with("voicechat:") {
        return;
    }
    let host = game.native_client_state().and_then(|s| s.server_address).map(|a| host_of(&a)).unwrap_or_default();
    with_state(game, |s| s.client.on_plugin(&p.channel, &p.data, &host));
    flush(game);
}

/// A chat line from the server, before it shows (the head of `chat_received`). A voice chat
/// server words some lines as `message.voicechat.` keys only its own client has text for (a
/// group invite is `message.voicechat.invite`), so the game would show the bare key: those get
/// Earshot's own wording, with the server's arguments and click actions kept
/// (`earshot_core::chat`). With Debug info on, the server's original line goes to the log.
pub fn chat_received(game: &Game<'_>, message: &mut horizon_client::text::Text) {
    if !game.native_window() {
        return;
    }
    let component = message.component();
    // A key the game's language or a resource pack has text for is left to the game.
    let translated = |key: &str| horizon_client::lang::current().is_some_and(|l| l.get(key).is_some());
    let Some(rewritten) = earshot_core::chat::rewrite(&component, &translated) else { return };
    if with_state(game, |s| s.debug) {
        eprintln!("[earshot] voice chat line from the server: {component}");
    }
    *message = horizon_client::component::to_text(&rewritten, &mut |_: &str| horizon_client::text::Text::empty());
}

/// Once a tick (`native_tick`): positions, push-to-talk, and anything to send.
pub fn tick(game: &mut Game<'_>) {
    if !game.native_window() {
        server_tick(game);
        return;
    }
    let listener = view(game).map(|(eye, yaw, _)| Listener { pos: eye, yaw_degrees: yaw });
    let players: Vec<(Uuid, [f64; 3])> = game
        .native_client_entities()
        .into_iter()
        .filter(|e| e.alive)
        .filter_map(|e| {
            let id = e.player_uuid?;
            // Voices come from the head, not the feet.
            Some((Uuid::from_u128(id), [e.position[0], e.position[1] + e.height as f64 * 0.9, e.position[2]]))
        })
        .collect();
    let ids: Vec<Uuid> = players.iter().map(|p| p.0).collect();
    let g: &Game<'_> = game;
    let updated = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let s = slot.as_mut()?;
        // Apply saved volumes as players come into range.
        for id in ids {
            player_volume(g, s, id);
        }
        s.client.tick(listener, players, s.ptt);
        Some(())
    });
    if updated.is_some() {
        flush(game);
    }
}

/// Before the connection closes (`native_client_disconnecting`).
pub fn disconnecting() {
    let _ = with_existing(|s| {
        s.ptt = false;
        let _ = s.menu.release();
        s.client.disconnect();
    });
}

// --- hooks: keys --------------------------------------------------------------------------

/// A key or mouse button (`native_key`): on any screen for keys, while playing for buttons.
pub fn key(game: &mut Game<'_>, key: &str, pressed: bool) {
    if !game.native_window() {
        return;
    }
    let bound = |k: (&str, &str, &str)| game.native_mod_key(ID, k.0, k.2);
    let (menu, ptt, mute, deafen) = (bound(KEY_MENU), bound(KEY_PTT), bound(KEY_MUTE), bound(KEY_DEAFEN));
    let playing = game.native_playing();
    let ours_open = game.native_mod_screen() == Some(ID);

    if key == menu && pressed {
        if ours_open {
            game.native_mod_screen_close(ID);
        } else if playing {
            with_state(game, |s| {
                s.ptt = false;
                // Start on the main screen each time.
                s.menu.open();
            });
            game.native_mod_screen_open(ID);
        }
        return;
    }
    // Typing in chat or a sign mustn't key the microphone: presses count only while playing.
    let changed = with_existing(|s| {
        if key == ptt {
            s.ptt = pressed && playing;
            return false;
        }
        if !pressed || !playing {
            return false;
        }
        if key == mute {
            s.muted = !s.muted;
            s.client.set_muted(s.muted);
            true
        } else if key == deafen {
            s.deafened = !s.deafened;
            s.client.set_deafened(s.deafened);
            true
        } else {
            false
        }
    });
    if changed == Some(true) {
        let _ = with_existing(|s| save(game, s));
    }
}

// --- hooks: HUD and world -----------------------------------------------------------------

/// The HUD (`native_hud`): the microphone icon bottom left, a line while connecting, and in a
/// voice group the group panel top left (its members, who's talking lit up).
pub fn hud(game: &mut Game<'_>, batch: &mut HudBatch, corners: &mut Corners) {
    let Some(status) = with_existing(|s| s.client.status()) else { return };
    if status.connection == "no voice server" {
        return;
    }
    if !status.connected {
        let text = format!("Voice: {}", status.connection);
        let w = game.native_text_width(&text) + 8.0;
        let (x, y) = corners.place(Corner::TopLeft, w, 13.0);
        game.native_fill(batch, [x, y, w, 13.0], 0x7800_0000);
        game.native_text(batch, &text, [x + 4.0, y + 3.0], 0xffaa55, true);
        return;
    }
    let state = mic_state(&status);
    let icons = if status.deafened { 2.0 } else { 1.0 };
    let group = status.my_group.as_ref().map(|g| format!("Group: {}", g.name));
    let text_w = group.as_ref().map(|t| game.native_text_width(t) + 8.0).unwrap_or(0.0);
    let (x, y) = corners.place(Corner::BottomLeft, 22.0 * icons + text_w, 20.0);
    let mut draws = ui::mic_icon([x + 2.0, y + 2.0], state);
    if status.deafened {
        draws.extend(ui::deafened_icon([x + 24.0, y + 2.0]));
    }
    paint(game, batch, &draws);
    if let Some(text) = group {
        let tx = x + 22.0 * icons + 2.0;
        game.native_fill(batch, [tx - 2.0, y, text_w, 20.0], 0x7800_0000);
        game.native_text(batch, &text, [tx + 2.0, y + 6.0], 0x7CE38B, true);
    }
    group_panel(game, batch, corners, &status);
}

/// In a voice group: its members top left, those talking now lit up with a speaker (`ui::group_panel`).
/// Left out while the F3 debug screen takes that corner.
fn group_panel(game: &Game<'_>, batch: &mut HudBatch, corners: &mut Corners, status: &Status) {
    let Some(group) = &status.my_group else { return };
    if !game.native_corner_shown(Corner::TopLeft) {
        return;
    }
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
    let (size, draws) = ui::group_panel(&group.name, &members, &|t| game.native_text_width(t));
    let (x, y) = corners.place(Corner::TopLeft, size[0], size[1]);
    paint(game, batch, &ui::translate(draws, [x, y]));
}

fn mic_state(s: &Status) -> MicState {
    if s.disabled {
        MicState::Off
    } else if s.muted {
        MicState::Muted
    } else if s.transmitting {
        MicState::Talking
    } else {
        MicState::Idle
    }
}

fn paint(game: &Game<'_>, batch: &mut HudBatch, draws: &[Draw]) {
    for d in draws {
        match d {
            Draw::Fill { rect, argb } => game.native_fill(batch, *rect, *argb),
            Draw::Quad { points, argb } => game.native_quad(batch, *points, *argb),
            Draw::Text { text, at, rgb } => game.native_text(batch, text, *at, *rgb, true),
        }
    }
}

/// While the world is drawn (`native_world_primitives`): a speaker icon beside the name tag of
/// everyone talking.
pub fn world_primitives(game: &Game<'_>, partial: f32, primitives: &mut WorldPrimitives) {
    let Some(talking) = with_existing(|s| s.client.status().talking) else { return };
    if talking.is_empty() {
        return;
    }
    let Some((_, yaw, pitch)) = view(game) else { return };
    // The view's right and up, so the icon faces the viewer as a name tag does.
    let (yaw, pitch) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
    let forward = [-yaw.sin() * pitch.cos(), -pitch.sin(), yaw.cos() * pitch.cos()];
    let right = [-yaw.cos(), 0.0, -yaw.sin()];
    let up = [
        right[1] * forward[2] - right[2] * forward[1],
        right[2] * forward[0] - right[0] * forward[2],
        right[0] * forward[1] - right[1] * forward[0],
    ];
    // One font pixel of a name tag, in blocks.
    const UNIT: f64 = 0.025;
    let speakers: Vec<(u32, String)> = game
        .native_client_entities()
        .into_iter()
        .filter(|e| e.alive && !e.invisible)
        .filter(|e| e.player_uuid.is_some_and(|id| talking.contains(&Uuid::from_u128(id))))
        .map(|e| (e.id, e.player_name.clone().unwrap_or_default()))
        .collect();
    if speakers.is_empty() {
        return;
    }
    let ids: Vec<u32> = speakers.iter().map(|s| s.0).collect();
    // Horizon's own name tag placement: where each tag hangs and how wide it is.
    for head in game.native_client_heads(&ids, partial.clamp(0.0, 1.0) as f64) {
        let name = speakers.iter().find(|s| s.0 == head.id).map(|s| s.1.as_str()).unwrap_or("");
        // Half the tag's width in blocks; 0 when no tag is drawn (F1), then from the name.
        let half = if head.name_half_width > 0.0 {
            head.name_half_width as f64
        } else {
            game.native_text_width(name) as f64 * UNIT / 2.0
        };
        // The anchor is the tag's top edge; its text hangs about 4 font pixels lower in the middle.
        let tag = [
            head.name_anchor[0] - up[0] * 4.0 * UNIT,
            head.name_anchor[1] - up[1] * 4.0 * UNIT,
            head.name_anchor[2] - up[2] * 4.0 * UNIT,
        ];
        // Past the tag's right end: half its width, then the icon's own half width and a gap.
        let offset = half + 9.0 * UNIT;
        let centre = [tag[0] + right[0] * offset, tag[1] + right[1] * offset, tag[2] + right[2] * offset];
        let at = |p: [f32; 2]| {
            let (x, y) = (p[0] as f64 * UNIT, p[1] as f64 * UNIT);
            [centre[0] + right[0] * x + up[0] * y, centre[1] + right[1] * x + up[1] * y, centre[2] + right[2] * x + up[2] * y]
        };
        for (a, b) in ui::speaker_lines() {
            primitives.line(at(a), at(b), 0xff55_ff55, 2.0);
        }
    }
}

// --- hooks: the voice menu (V) ------------------------------------------------------------

fn ours(game: &Game<'_>) -> bool {
    game.native_mod_screen() == Some(ID)
}

/// Nearby players for the menu, nearest first.
fn nearby_players(game: &Game<'_>) -> Vec<(Uuid, String, f64)> {
    let Some((eye, _, _)) = view(game) else { return vec![] };
    let mut out: Vec<(Uuid, String, f64)> = game
        .native_client_entities()
        .into_iter()
        .filter_map(|e| {
            let id = Uuid::from_u128(e.player_uuid?);
            let d = ((e.position[0] - eye[0]).powi(2) + (e.position[1] - eye[1]).powi(2) + (e.position[2] - eye[2]).powi(2)).sqrt();
            (d <= MENU_PLAYER_RANGE).then(|| (id, e.player_name.clone().unwrap_or_else(|| id.to_string()), d))
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

fn model(game: &Game<'_>, s: &mut State) -> MenuModel {
    let status = s.client.status();
    let players = nearby_players(game)
        .into_iter()
        .map(|(id, name, _)| {
            let v = player_volume(game, s, id);
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
    MenuModel {
        status: connection,
        connected: status.connected,
        muted: s.muted,
        deafened: s.deafened,
        disabled: s.disabled,
        push_to_talk: matches!(s.settings.mode, Mode::PushToTalk),
        ptt_key: key_label(&game.native_mod_key(ID, KEY_PTT.0, KEY_PTT.2)),
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
            .map(|g| (g.id, g.name.clone(), g.has_password, g.members.len(), status.my_group.as_ref().is_some_and(|m| m.id == g.id)))
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
fn apply(game: &Game<'_>, s: &mut State, action: MenuAction) -> bool {
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
            settings_changed = false;
            true
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
        MenuAction::CreateGroup { name, password } => {
            s.client.create_group(&name, password.as_deref());
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
            game.native_set_setting(ID, &format!("volume.{id}"), &v.to_string());
            settings_changed = false;
            false
        }
    };
    if settings_changed {
        s.client.apply_settings(s.settings.clone());
    }
    save_now
}

/// Draws the menu over the world (`native_screen_render_post`).
pub fn screen_render(game: &Game<'_>, batch: &mut HudBatch, screen: &NativeScreenView<'_>) {
    if !ours(game) {
        return;
    }
    let gui = gui_size(game);
    let draws = with_state(game, |s| {
        let m = model(game, s);
        s.menu.draw(&m, gui, screen.mouse, &|t: &str| game.native_text_width(t))
    });
    paint(game, batch, &draws);
}

/// A click in the menu (`native_screen_mouse_button_pressed_pre`).
pub fn screen_press(game: &Game<'_>, screen: &NativeScreenView<'_>, button: i32, canceled: &mut bool) {
    if !ours(game) || button != 0 {
        return;
    }
    let gui = gui_size(game);
    with_state(game, |s| {
        let m = model(game, s);
        if s.menu.contains(&m, gui, screen.mouse) {
            *canceled = true;
        }
        if let Some(action) = s.menu.press(&m, gui, screen.mouse) {
            if apply(game, s, action) {
                save(game, s);
            }
        }
    });
}

/// A drag in the menu (`native_screen_mouse_dragged_pre`): sliders follow it.
pub fn screen_drag(game: &Game<'_>, screen: &NativeScreenView<'_>, canceled: &mut bool) {
    if !ours(game) {
        return;
    }
    let gui = gui_size(game);
    with_state(game, |s| {
        let m = model(game, s);
        if let Some(action) = s.menu.drag(&m, gui, screen.mouse) {
            *canceled = true;
            apply(game, s, action);
        }
    });
}

/// The button came up (`native_screen_mouse_button_released_pre`): a slider was let go.
pub fn screen_release(game: &Game<'_>, canceled: &mut bool) {
    if !ours(game) {
        return;
    }
    let was_dragging = with_existing(|s| {
        let dragging = s.menu.release();
        if dragging {
            save(game, s);
        }
        dragging
    });
    if was_dragging == Some(true) {
        *canceled = true;
    }
}

/// A key on the menu (`native_screen_key_pressed_pre`). While a text box has focus the keys are
/// the box's: Backspace, Enter and Tab work it, and every other key is held back from the game
/// (and from Earshot's own V/M/N), so typing a group name doesn't act on anything. Escape still
/// closes the menu.
pub fn screen_key(game: &Game<'_>, key: &str, canceled: &mut bool) {
    if !ours(game) {
        return;
    }
    with_state(game, |s| {
        if !s.menu.typing() {
            return;
        }
        let menu_key = match key {
            "key.keyboard.backspace" => Some(MenuKey::Backspace),
            "key.keyboard.enter" | "key.keyboard.keypad.enter" => Some(MenuKey::Enter),
            "key.keyboard.tab" => Some(MenuKey::Tab),
            "key.keyboard.escape" => {
                let _ = s.menu.key(MenuKey::Escape);
                return;
            }
            _ => None,
        };
        *canceled = true;
        if let Some(k) = menu_key {
            if let Some(action) = s.menu.key(k) {
                if apply(game, s, action) {
                    save(game, s);
                }
            }
        }
    });
}

/// A character typed on the menu (`native_screen_char_typed_pre`): into the focused text box.
pub fn screen_char(game: &Game<'_>, c: char, canceled: &mut bool) {
    if !ours(game) {
        return;
    }
    let _ = with_existing(|s| {
        if s.menu.typing() {
            s.menu.type_char(c);
            *canceled = true;
        }
    });
}

// --- the server side: voice on a Horizon dedicated server ------------------------------------
//
// On a dedicated server (and single player's server thread, `native_window` false) Earshot runs
// an SVC-compatible voice server ([`earshot_core::server`]), so Earshot players and Java
// players with Simple Voice Chat can talk there. Its settings are files in the server's mod
// data folder (`horizon-mods/.horizon-mod-data/earshot/`): `server_port` (24454),
// `voice_host` (what clients connect to; empty = the address they joined), `max_distance` (48)
// and `server_voice` (`false` turns it off).

struct ServerSide {
    voice: Option<earshot_core::server::VoiceServer>,
    /// Connections already told this server has voice.
    announced: std::collections::HashSet<u32>,
    /// Starting failed (port taken…): don't retry every packet.
    failed: bool,
}

thread_local! {
    static SERVER: RefCell<ServerSide> = RefCell::new(ServerSide { voice: None, announced: Default::default(), failed: false });
}

fn start_server(game: &Game<'_>, side: &mut ServerSide) {
    if side.voice.is_some() || side.failed {
        return;
    }
    let get = |k: &str| game.native_setting(ID, k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if get("server_voice").as_deref() == Some("false") {
        side.failed = true;
        return;
    }
    let port: u16 = get("server_port").and_then(|v| v.parse().ok()).unwrap_or(24454);
    let config = earshot_core::server::ServerConfig {
        bind: std::net::SocketAddr::from(([0, 0, 0, 0], port)),
        max_distance: get("max_distance").and_then(|v| v.parse().ok()).unwrap_or(48.0),
        voice_host: get("voice_host").unwrap_or_default(),
        ..Default::default()
    };
    match earshot_core::server::VoiceServer::start(config) {
        Ok(v) => {
            eprintln!("[earshot] voice server on UDP port {}", v.port());
            side.voice = Some(v);
        }
        Err(e) => {
            eprintln!("[earshot] couldn't start the voice server on UDP port {port}: {e}");
            side.failed = true;
        }
    }
}

/// Every play packet a player sends the server (HEAD of `lan_guest_packet`): SVC's plugin
/// messages go to the voice server, and what it has to say goes out.
pub fn server_packet(
    game: &mut Game<'_>,
    host: &mut super::lan::LanHost,
    conn: u32,
    packet: &horizon_protocol::packets::play::Serverbound,
) {
    use horizon_protocol::packets::play::{Clientbound as C, Serverbound as S};
    if game.native_window() {
        return;
    }
    let g: &Game<'_> = game;
    SERVER.with(|cell| {
        let mut guard = cell.borrow_mut();
        // A plain &mut so its fields can be borrowed separately.
        let side: &mut ServerSide = &mut guard;
        start_server(g, side);
        let Some(voice) = side.voice.as_ref() else { return };
        let Some((uuid, name)) = host.hub.guest(conn).map(|guest| (guest.uuid, guest.name.clone())) else { return };

        // Tell each player once that this server has voice: clients ask for a key when they see it.
        if !side.announced.contains(&conn) {
            host.hub.send_to(
                conn,
                &C::CustomPayload(CustomPayload {
                    channel: plugin::MINECRAFT_REGISTER.into(),
                    data: earshot_core::server::VoiceServer::register_payload(),
                }),
            );
            side.announced.insert(conn);
        }
        if let S::CustomPayload(p) = packet {
            if p.channel.starts_with("voicechat:") {
                voice.on_plugin(Uuid::from_u128(uuid), &name, &p.channel, &p.data);
            }
        }
        for (to, channel, data) in voice.take_outgoing() {
            let target = host.hub.guests.iter().find(|guest| Uuid::from_u128(guest.uuid) == to).map(|guest| guest.conn);
            if let Some(c) = target {
                host.hub.send_to(c, &C::CustomPayload(CustomPayload { channel: channel.into(), data }));
            }
        }
    });
}

/// The server's tick: where everyone is, for proximity.
fn server_tick(game: &Game<'_>) {
    SERVER.with(|cell| {
        let mut guard = cell.borrow_mut();
        let side: &mut ServerSide = &mut guard;
        let Some(voice) = side.voice.as_ref() else { return };
        let players: Vec<(Uuid, String, [f64; 3])> = game
            .native_server_players()
            .into_iter()
            .map(|p| (Uuid::from_u128(p.uuid), p.dimension.clone(), p.position))
            .collect();
        let empty = players.is_empty();
        voice.set_players(players);
        if empty {
            side.announced.clear();
        }
    });
}

// --- Options > Mods -----------------------------------------------------------------------

/// Lists Earshot and its keys (`native_mods`).
pub fn mods(mods: &mut Vec<NativeMod>) {
    let key = |k: (&str, &str, &str)| [k.0.to_string(), k.1.to_string(), k.2.to_string()];
    mods.push(NativeMod {
        id: ID.into(),
        title: format!("Earshot {VERSION}"),
        description: "Proximity voice chat, compatible with Simple Voice Chat servers. \
                      An independent project, not affiliated with Simple Voice Chat."
            .into(),
        keys: vec![key(KEY_MENU), key(KEY_PTT), key(KEY_MUTE), key(KEY_DEAFEN)],
        ..Default::default()
    });
}

fn or_waiting(s: &str) -> &str {
    if s.is_empty() { "not open yet" } else { s }
}

/// The settings page (`native_settings`): the menu has the settings; this page has the keys
/// (listed by Horizon above the buttons), voice on/off, the status, and the diagnostics when
/// "Debug info" is on.
pub fn settings(game: &mut Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    if mod_id != ID || !game.native_window() {
        return;
    }
    let menu_key = key_label(&game.native_mod_key(ID, KEY_MENU.0, KEY_MENU.2));
    let lines = with_state(game, |s| {
        let status = s.client.status();
        let mut lines = vec![
            format!("Press {menu_key} in game for the voice menu"),
            format!("Voice chat: {}", if s.disabled { "off" } else { "on" }),
            format!("Debug info: {}", if s.debug { "on" } else { "off" }),
            format!("Status: {}", status.connection),
        ];
        if s.debug {
            lines.extend([
                format!("Mic level {:.0} dB", status.input_level_dbfs),
                format!("Using mic: {}", or_waiting(&status.input_device)),
                format!("Using speakers: {}", or_waiting(&status.output_device)),
                format!("Sent {} mic frames, received {} voice frames", status.mic_frames_sent, status.voice_frames_received),
                status.udp_summary.clone(),
            ]);
        }
        lines
    });
    buttons.extend(lines);
}

/// A settings page button was clicked (`native_settings_click`): "Voice chat" (1) and
/// "Debug info" (2) switch; the other lines only show things.
pub fn settings_click(game: &mut Game<'_>, mod_id: &str, button: usize) {
    if mod_id != ID || !game.native_window() {
        return;
    }
    match button {
        1 => with_state(game, |s| {
            s.disabled = !s.disabled;
            s.client.set_disabled(s.disabled);
        }),
        2 => with_state(game, |s| s.debug = !s.debug),
        _ => return,
    }
    let _ = with_existing(|s| save(game, s));
    // Turning voice off or on is a message to the server.
    flush(game);
}
