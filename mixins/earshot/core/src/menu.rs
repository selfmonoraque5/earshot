//! The voice menu (opened with V): a small main screen, and separate screens for settings,
//! nearby players, groups (a group's members once you're in one), creating a group and a
//! group's password.
//!
//! Everything here is independent of the game: it lays out widgets in GUI pixels, produces
//! [`Draw`] commands, and turns clicks, drags, keys and typed characters into [`MenuAction`]s.
//! The game glue only paints and forwards input, so layout and behaviour are unit-tested here.
//! The look and wording are Earshot's own.

use crate::ui::{Draw, lock_icon};
use crate::wire::Uuid;

/// What a click, drag or key in the menu asks for. Moving between screens is the menu's own
/// business and produces nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    ToggleMute,
    ToggleDeafen,
    ToggleVoiceChat,
    ToggleMode,
    ToggleNoiseSuppression,
    SetThreshold(f32),
    SetMicVolume(f32),
    SetVoiceVolume(f32),
    NextMicrophone,
    NextSpeakers,
    SetPlayerVolume(Uuid, f32),
    /// Join a group, with its password if it has one.
    JoinGroup(Uuid, Option<String>),
    CreateGroup { name: String, password: Option<String> },
    LeaveGroup,
}

/// Keys the menu handles itself while a text box has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    Backspace,
    Enter,
    /// Moves between the name and password boxes.
    Tab,
    /// Leaves the text box (the game closes the menu on Escape too).
    Escape,
}

/// What the menu shows. Built fresh each frame from the client's state.
#[derive(Debug, Clone, Default)]
pub struct MenuModel {
    pub status: String,
    pub connected: bool,
    pub muted: bool,
    pub deafened: bool,
    pub disabled: bool,
    pub push_to_talk: bool,
    pub ptt_key: String,
    pub threshold_db: f32,
    pub level_db: f32,
    pub noise_suppression: bool,
    pub mic_volume: f32,
    pub voice_volume: f32,
    pub microphone: String,
    pub speakers: String,
    /// Nearby players: id, name, volume, talking now.
    pub players: Vec<(Uuid, String, f32, bool)>,
    /// The group we're in, by name.
    pub group: Option<String>,
    /// Its members: id, name, talking now.
    pub group_members: Vec<(Uuid, String, bool)>,
    /// The server's groups: id, name, has a password, members, it's ours.
    pub groups: Vec<(Uuid, String, bool, usize, bool)>,
    /// Why joining failed (wrong password).
    pub group_problem: Option<String>,
}

/// The menu's screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Settings,
    Players,
    /// The group list, or our group's members when we're in one.
    Groups,
    CreateGroup,
    /// The password for joining this group.
    Password(Uuid),
}

impl Page {
    /// Where Back goes.
    fn parent(self) -> Page {
        match self {
            Page::Main | Page::Settings | Page::Players | Page::Groups => Page::Main,
            Page::CreateGroup | Page::Password(_) => Page::Groups,
        }
    }
}

/// Slider ranges.
pub const DB_MIN: f32 = -60.0;
pub const DB_MAX: f32 = 0.0;
pub const VOLUME_MAX: f32 = 2.0;

/// Longest group name and password the boxes take.
pub const MAX_NAME: usize = 24;
pub const MAX_PASSWORD: usize = 32;

const PANEL_W: f32 = 232.0;
const ROW_H: f32 = 18.0;
const GAP: f32 = 4.0;
const PAD: f32 = 10.0;
const TITLE_H: f32 = 18.0;
const TEXT_H: f32 = 8.0;

const BG: u32 = 0xF016_161D;
const TITLE_BG: u32 = 0xFF24_2430;
const BUTTON: u32 = 0xFF33_3340;
const BUTTON_HOVER: u32 = 0xFF4A_4A5C;
const ON: u32 = 0xFF2F_7D46;
const ON_HOVER: u32 = 0xFF3C_9A57;
const OFF_RED: u32 = 0xFF8A_2E2E;
const TRACK: u32 = 0xFF22_2229;
const FILL: u32 = 0xFF4A_6FD0;
const KNOB: u32 = 0xFFE6_E6F0;
const LEVEL: u32 = 0xC046_C86A;
const WHITE: u32 = 0xFFFFFF;
const GREY: u32 = 0xA0A0A8;
const TITLE: u32 = 0xFFE7A8;
const GREEN_TEXT: u32 = 0x7CE38B;
const RED_TEXT: u32 = 0xFF7777;

/// Which text box has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    NewName,
    NewPassword,
    JoinPassword,
}

/// What's being typed: kept by the menu between frames.
#[derive(Debug, Clone, Default)]
struct Editing {
    focus: Option<Field>,
    name: String,
    password: String,
    join_password: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Button,
    /// A slider from `min` to `max` showing `value`.
    Slider { min: f32, max: f32, value: f32 },
    /// A text box: the widget's label is its text, shown as `placeholder` when empty, as
    /// asterisks when `secret`, with a caret when focused.
    Field { field: Field, placeholder: &'static str, secret: bool },
    /// A line that shows something and can't be clicked (a group member).
    Label,
}

#[derive(Debug, Clone, PartialEq)]
enum Id {
    Go(Page),
    Back,
    Mute,
    Deafen,
    VoiceChat,
    Mode,
    Noise,
    Threshold,
    MicVolume,
    VoiceVolume,
    Microphone,
    Speakers,
    Player(Uuid),
    Group(Uuid),
    Member(Uuid),
    Text(Field),
    Create,
    Join,
    Leave,
}

#[derive(Debug, Clone)]
struct Widget {
    id: Id,
    rect: [f32; 4],
    kind: Kind,
    label: String,
    /// Drawn in the "on" colour (toggles that are on, ready-to-press buttons).
    on: bool,
    /// Drawn in red (mute/deafen/voice-off when active).
    alert: bool,
    /// A padlock at the right end (password groups).
    lock: bool,
    /// A coloured initial at the left end (group members), from this player's id.
    avatar: Option<Uuid>,
}

impl Widget {
    fn new(id: Id, kind: Kind, label: impl Into<String>) -> Self {
        Self { id, rect: [0.0; 4], kind, label: label.into(), on: false, alert: false, lock: false, avatar: None }
    }

    fn on(mut self, on: bool) -> Self {
        self.on = on;
        self
    }

    fn alert(mut self, alert: bool) -> Self {
        self.alert = alert;
        self
    }
}

/// One screen, laid out.
struct Layout {
    panel: [f32; 4],
    title: String,
    widgets: Vec<Widget>,
    /// Lines of text between the widgets: text, top-left, colour.
    texts: Vec<(String, [f32; 2], u32)>,
}

/// Lays a screen out top to bottom, then centres it.
struct Builder {
    x: f32,
    w: f32,
    y: f32,
    widgets: Vec<Widget>,
    texts: Vec<(String, [f32; 2], u32)>,
}

impl Builder {
    fn row(&mut self, cells: Vec<Widget>) {
        let n = cells.len() as f32;
        let cw = (self.w - GAP * (n - 1.0)) / n;
        for (i, mut c) in cells.into_iter().enumerate() {
            c.rect = [self.x + i as f32 * (cw + GAP), self.y, cw, ROW_H];
            self.widgets.push(c);
        }
        self.y += ROW_H + GAP;
    }

    fn text(&mut self, text: impl Into<String>, rgb: u32) {
        self.texts.push((text.into(), [self.x, self.y + 1.0], rgb));
        self.y += TEXT_H + 5.0;
    }

    fn space(&mut self, h: f32) {
        self.y += h;
    }
}

fn inside(r: [f32; 4], p: [f32; 2]) -> bool {
    p[0] >= r[0] && p[0] < r[0] + r[2] && p[1] >= r[1] && p[1] < r[1] + r[3]
}

fn pct(v: f32) -> String {
    format!("{:.0}%", v * 100.0)
}

/// How many list rows (players, groups, members) fit on a screen of height `gui_h` next to
/// `other_rows` of fixed rows.
fn list_room(gui_h: f32, other_rows: usize) -> usize {
    let fixed = TITLE_H + 2.0 * PAD + 16.0 + (other_rows as f32 + 1.0) * (ROW_H + GAP);
    (((gui_h - fixed) / (ROW_H + GAP)).floor().max(1.0) as usize).min(8)
}

/// The voice menu: which screen is open, the slider being dragged and what's typed.
#[derive(Debug)]
pub struct Menu {
    page: Page,
    dragging: Option<Id>,
    edit: Editing,
}

impl Default for Menu {
    fn default() -> Self {
        Self { page: Page::Main, dragging: None, edit: Editing::default() }
    }
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    /// The menu was opened: start on the main screen, nothing focused.
    pub fn open(&mut self) {
        self.page = Page::Main;
        self.dragging = None;
        self.edit.focus = None;
    }

    /// The screen shown: a create or password screen gives way to the group once we're in one,
    /// and a password screen to the list once its group is gone.
    pub fn page(&self, m: &MenuModel) -> Page {
        match self.page {
            Page::CreateGroup | Page::Password(_) if m.group.is_some() => Page::Groups,
            Page::Password(id) if !m.groups.iter().any(|g| g.0 == id) => Page::Groups,
            p => p,
        }
    }

    fn layout(&self, m: &MenuModel, gui: [f32; 2]) -> Layout {
        let w = PANEL_W.min(gui[0] - 8.0);
        let x0 = ((gui[0] - w) / 2.0).floor();
        let mut b = Builder { x: x0 + PAD, w: w - 2.0 * PAD, y: TITLE_H + PAD, widgets: Vec::new(), texts: Vec::new() };
        let page = self.page(m);
        let title = match page {
            Page::Main => "Earshot".to_string(),
            Page::Settings => "Settings".to_string(),
            Page::Players => "Players nearby".to_string(),
            Page::Groups => m.group.clone().map(|g| format!("Group: {g}")).unwrap_or_else(|| "Groups".to_string()),
            Page::CreateGroup => "Create group".to_string(),
            Page::Password(id) => {
                let name = m.groups.iter().find(|g| g.0 == id).map(|g| g.1.as_str()).unwrap_or("group");
                format!("Join {name}")
            }
        };

        match page {
            Page::Main => {
                b.text(&m.status, if m.connected { GREEN_TEXT } else { 0xFFAA55 });
                b.row(vec![Widget::new(Id::Go(Page::Settings), Kind::Button, "Settings")]);
                let groups = match &m.group {
                    Some(g) => format!("Group: {g}"),
                    None => "Groups".to_string(),
                };
                b.row(vec![Widget::new(Id::Go(Page::Groups), Kind::Button, groups).on(m.group.is_some())]);
                b.row(vec![Widget::new(Id::Go(Page::Players), Kind::Button, format!("Players ({})", m.players.len()))]);
                b.space(4.0);
                b.row(vec![
                    Widget::new(Id::Mute, Kind::Button, if m.muted { "Muted" } else { "Mic on" }).alert(m.muted),
                    Widget::new(Id::Deafen, Kind::Button, if m.deafened { "Deafened" } else { "Sound on" }).alert(m.deafened),
                    Widget::new(Id::VoiceChat, Kind::Button, if m.disabled { "Voice off" } else { "Voice on" }).alert(m.disabled),
                ]);
            }
            Page::Settings => {
                b.row(vec![Widget::new(
                    Id::VoiceVolume,
                    Kind::Slider { min: 0.0, max: VOLUME_MAX, value: m.voice_volume },
                    format!("Voice volume: {}", pct(m.voice_volume)),
                )]);
                b.row(vec![Widget::new(
                    Id::MicVolume,
                    Kind::Slider { min: 0.0, max: VOLUME_MAX, value: m.mic_volume },
                    format!("Mic volume: {}", pct(m.mic_volume)),
                )]);
                let mode = if m.push_to_talk { format!("Push to talk ({})", m.ptt_key) } else { "Voice activation".to_string() };
                b.row(vec![Widget::new(Id::Mode, Kind::Button, mode)]);
                let meter = if m.push_to_talk {
                    format!("Mic level {:.0} dB", m.level_db)
                } else {
                    format!("Threshold {:.0} dB (level {:.0})", m.threshold_db, m.level_db)
                };
                b.row(vec![Widget::new(Id::Threshold, Kind::Slider { min: DB_MIN, max: DB_MAX, value: m.threshold_db }, meter)]);
                b.row(vec![Widget::new(
                    Id::Noise,
                    Kind::Button,
                    format!("Noise reduction: {}", if m.noise_suppression { "on" } else { "off" }),
                )
                .on(m.noise_suppression)]);
                b.row(vec![Widget::new(Id::Microphone, Kind::Button, format!("Input: {}", m.microphone))]);
                b.row(vec![Widget::new(Id::Speakers, Kind::Button, format!("Output: {}", m.speakers))]);
            }
            Page::Players => {
                if m.players.is_empty() {
                    b.text("No players nearby", GREY);
                }
                let room = list_room(gui[1], 1);
                for (id, name, volume, talking) in m.players.iter().take(room) {
                    let label = format!("{}{name}: {}", if *talking { "> " } else { "" }, pct(*volume));
                    b.row(vec![Widget::new(Id::Player(*id), Kind::Slider { min: 0.0, max: VOLUME_MAX, value: *volume }, label)]);
                }
                if m.players.len() > room {
                    b.text(format!("and {} more", m.players.len() - room), GREY);
                }
            }
            Page::Groups if m.group.is_some() => {
                b.text("Members", GREY);
                let room = list_room(gui[1], 2);
                for (id, name, talking) in m.group_members.iter().take(room) {
                    let mut w = Widget::new(Id::Member(*id), Kind::Label, if *talking { format!("{name}  (talking)") } else { name.clone() });
                    w.on = *talking;
                    w.avatar = Some(*id);
                    b.row(vec![w]);
                }
                if m.group_members.len() > room {
                    b.text(format!("and {} more", m.group_members.len() - room), GREY);
                }
                b.row(vec![Widget::new(Id::Leave, Kind::Button, "Leave group").alert(true)]);
            }
            Page::Groups => {
                if let Some(p) = &m.group_problem {
                    b.text(format!("Couldn't join: {p}"), RED_TEXT);
                }
                let others: Vec<_> = m.groups.iter().filter(|g| !g.4).collect();
                if others.is_empty() {
                    b.text("No groups yet", GREY);
                }
                let room = list_room(gui[1], 2);
                for (id, name, has_password, members, _) in others.iter().take(room) {
                    let mut w = Widget::new(Id::Group(*id), Kind::Button, format!("{name} ({members})"));
                    w.lock = *has_password;
                    b.row(vec![w]);
                }
                if others.len() > room {
                    b.text(format!("and {} more", others.len() - room), GREY);
                }
                b.row(vec![Widget::new(Id::Go(Page::CreateGroup), Kind::Button, "Create group")]);
            }
            Page::CreateGroup => {
                b.text("Group name", GREY);
                b.row(vec![Widget::new(
                    Id::Text(Field::NewName),
                    Kind::Field { field: Field::NewName, placeholder: "", secret: false },
                    self.edit.name.clone(),
                )]);
                b.text("Password (optional)", GREY);
                b.row(vec![Widget::new(
                    Id::Text(Field::NewPassword),
                    Kind::Field { field: Field::NewPassword, placeholder: "", secret: true },
                    self.edit.password.clone(),
                )]);
                b.row(vec![Widget::new(Id::Create, Kind::Button, "Create").on(!self.edit.name.trim().is_empty())]);
            }
            Page::Password(_) => {
                b.text("Password", GREY);
                b.row(vec![Widget::new(
                    Id::Text(Field::JoinPassword),
                    Kind::Field { field: Field::JoinPassword, placeholder: "", secret: true },
                    self.edit.join_password.clone(),
                )]);
                if let Some(p) = &m.group_problem {
                    b.text(format!("Couldn't join: {p}"), RED_TEXT);
                }
                b.row(vec![Widget::new(Id::Join, Kind::Button, "Join").on(!self.edit.join_password.is_empty())]);
            }
        }
        if page != Page::Main {
            b.space(2.0);
            b.row(vec![Widget::new(Id::Back, Kind::Button, "Back")]);
        }

        // Centre vertically: everything was laid out from the panel's top.
        let h = b.y - GAP + PAD;
        let y0 = ((gui[1] - h) / 2.0).max(4.0).floor();
        for w in &mut b.widgets {
            w.rect[1] += y0;
        }
        for t in &mut b.texts {
            t.1[1] += y0;
        }
        Layout { panel: [x0, y0, w, h], title, widgets: b.widgets, texts: b.texts }
    }

    /// Everything to paint this frame. `text_width` measures a string in GUI pixels.
    pub fn draw(&self, m: &MenuModel, gui: [f32; 2], mouse: [f32; 2], text_width: &dyn Fn(&str) -> f32) -> Vec<Draw> {
        let l = self.layout(m, gui);
        let p = l.panel;
        let mut out = vec![
            Draw::Fill { rect: p, argb: BG },
            Draw::Fill { rect: [p[0], p[1], p[2], TITLE_H], argb: TITLE_BG },
            Draw::Text {
                text: l.title.clone(),
                at: [p[0] + ((p[2] - text_width(&l.title)) / 2.0).max(4.0), p[1] + (TITLE_H - TEXT_H) / 2.0],
                rgb: TITLE,
            },
        ];
        for (text, at, rgb) in &l.texts {
            out.push(Draw::Text { text: fit(text, p[2] - 2.0 * PAD, text_width), at: *at, rgb: *rgb });
        }
        for w in &l.widgets {
            let hover = inside(w.rect, mouse) || self.dragging.as_ref() == Some(&w.id);
            match w.kind {
                Kind::Field { field, placeholder, secret } => {
                    let focused = self.edit.focus == Some(field);
                    out.push(Draw::Fill { rect: w.rect, argb: if focused { 0xFF70_7088 } else { 0xFF3A_3A44 } });
                    out.push(Draw::Fill {
                        rect: [w.rect[0] + 1.0, w.rect[1] + 1.0, w.rect[2] - 2.0, w.rect[3] - 2.0],
                        argb: 0xFF0E_0E12,
                    });
                    push_field_text(&mut out, w, placeholder, secret, focused, text_width);
                    continue;
                }
                Kind::Label => {
                    out.push(Draw::Fill { rect: w.rect, argb: if w.on { 0xFF24_4A30 } else { 0xFF22_222A } });
                    let mut text_x = w.rect[0] + 5.0;
                    if let Some(id) = w.avatar {
                        out.extend(avatar(id, &w.label, [w.rect[0] + 3.0, w.rect[1] + 3.0], text_width));
                        text_x = w.rect[0] + 3.0 + 12.0 + 5.0;
                    }
                    let text = fit(&w.label, w.rect[0] + w.rect[2] - text_x - 4.0, text_width);
                    out.push(Draw::Text { text, at: [text_x, w.rect[1] + (ROW_H - TEXT_H) / 2.0], rgb: if w.on { GREEN_TEXT } else { WHITE } });
                    continue;
                }
                Kind::Button => {
                    let argb = match (w.alert, w.on, hover) {
                        (true, _, true) => 0xFFA0_3A3A,
                        (true, _, false) => OFF_RED,
                        (_, true, true) => ON_HOVER,
                        (_, true, false) => ON,
                        (_, false, true) => BUTTON_HOVER,
                        _ => BUTTON,
                    };
                    out.push(Draw::Fill { rect: w.rect, argb });
                    if w.lock {
                        out.extend(lock_icon([w.rect[0] + w.rect[2] - 12.0, w.rect[1] + (ROW_H - 9.0) / 2.0], 0xFFE0C060));
                    }
                }
                Kind::Slider { min, max, value } => {
                    out.push(Draw::Fill { rect: w.rect, argb: TRACK });
                    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
                    if w.id == Id::Threshold {
                        // The live level meter fills the track; the knob is the threshold.
                        let level_t = ((m.level_db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
                        out.push(Draw::Fill { rect: [w.rect[0], w.rect[1], w.rect[2] * level_t, w.rect[3]], argb: LEVEL });
                        if m.push_to_talk {
                            // No threshold in push-to-talk: just the meter.
                            push_label(&mut out, w, text_width);
                            continue;
                        }
                    } else {
                        out.push(Draw::Fill { rect: [w.rect[0], w.rect[1], w.rect[2] * t, w.rect[3]], argb: FILL });
                    }
                    let kx = w.rect[0] + (w.rect[2] - 4.0) * t;
                    out.push(Draw::Fill { rect: [kx, w.rect[1], 4.0, w.rect[3]], argb: if hover { 0xFFFFFFFF } else { KNOB } });
                }
            }
            push_label(&mut out, w, text_width);
        }
        out
    }

    /// A mouse button went down at `p` (GUI pixels). Returns what to do, if anything; moving
    /// between screens is handled here. Whether it hit the panel at all is [`Menu::contains`].
    pub fn press(&mut self, m: &MenuModel, gui: [f32; 2], p: [f32; 2]) -> Option<MenuAction> {
        let l = self.layout(m, gui);
        let Some(w) = l.widgets.into_iter().find(|w| inside(w.rect, p)) else {
            self.edit.focus = None;
            return None;
        };
        if let Kind::Field { field, .. } = w.kind {
            self.edit.focus = Some(field);
            return None;
        }
        self.edit.focus = None;
        let page = self.page(m);
        match w.id {
            Id::Go(target) => {
                self.go(target);
                None
            }
            Id::Back => {
                self.go(page.parent());
                None
            }
            Id::Mute => Some(MenuAction::ToggleMute),
            Id::Deafen => Some(MenuAction::ToggleDeafen),
            Id::VoiceChat => Some(MenuAction::ToggleVoiceChat),
            Id::Mode => Some(MenuAction::ToggleMode),
            Id::Noise => Some(MenuAction::ToggleNoiseSuppression),
            Id::Microphone => Some(MenuAction::NextMicrophone),
            Id::Speakers => Some(MenuAction::NextSpeakers),
            Id::Leave => Some(MenuAction::LeaveGroup),
            Id::Create => self.create(),
            Id::Join => match page {
                Page::Password(id) => self.join_with_password(id),
                _ => None,
            },
            Id::Group(g) => {
                if m.groups.iter().any(|x| x.0 == g && x.2) {
                    // A password group: ask for it on its own screen.
                    self.go(Page::Password(g));
                    None
                } else {
                    Some(MenuAction::JoinGroup(g, None))
                }
            }
            Id::Threshold if m.push_to_talk => None,
            Id::Threshold | Id::MicVolume | Id::VoiceVolume | Id::Player(_) => {
                let Kind::Slider { min, max, .. } = w.kind else { return None };
                self.dragging = Some(w.id.clone());
                slider_action(&w.id, slider_value(w.rect, min, max, p[0]))
            }
            Id::Member(_) | Id::Text(_) => None,
        }
    }

    fn go(&mut self, page: Page) {
        self.page = page;
        self.dragging = None;
        self.edit.focus = match page {
            Page::CreateGroup => Some(Field::NewName),
            Page::Password(_) => {
                self.edit.join_password.clear();
                Some(Field::JoinPassword)
            }
            _ => None,
        };
    }

    /// The pointer moved to `p` with the button held.
    pub fn drag(&mut self, m: &MenuModel, gui: [f32; 2], p: [f32; 2]) -> Option<MenuAction> {
        let id = self.dragging.clone()?;
        let w = self.layout(m, gui).widgets.into_iter().find(|w| w.id == id)?;
        let Kind::Slider { min, max, .. } = w.kind else { return None };
        slider_action(&id, slider_value(w.rect, min, max, p[0]))
    }

    /// The button came up. Returns whether a slider was being dragged (only then was the press
    /// taken from the game, so only then should the release be).
    pub fn release(&mut self) -> bool {
        self.dragging.take().is_some()
    }

    /// Whether `p` is on the panel (clicks there shouldn't reach the game).
    pub fn contains(&self, m: &MenuModel, gui: [f32; 2], p: [f32; 2]) -> bool {
        inside(self.layout(m, gui).panel, p)
    }

    /// Whether a text box has focus: then keys belong to the menu, not to the game.
    pub fn typing(&self) -> bool {
        self.edit.focus.is_some()
    }

    /// A character typed into the focused text box.
    pub fn type_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let (text, max) = match self.edit.focus {
            Some(Field::NewName) => (&mut self.edit.name, MAX_NAME),
            Some(Field::NewPassword) => (&mut self.edit.password, MAX_PASSWORD),
            Some(Field::JoinPassword) => (&mut self.edit.join_password, MAX_PASSWORD),
            None => return,
        };
        if text.chars().count() < max {
            text.push(c);
        }
    }

    /// A key the focused text box handles.
    pub fn key(&mut self, key: MenuKey) -> Option<MenuAction> {
        let focus = self.edit.focus?;
        match key {
            MenuKey::Backspace => {
                match focus {
                    Field::NewName => self.edit.name.pop(),
                    Field::NewPassword => self.edit.password.pop(),
                    Field::JoinPassword => self.edit.join_password.pop(),
                };
                None
            }
            MenuKey::Enter => match (focus, self.page) {
                (Field::NewName | Field::NewPassword, _) => self.create(),
                (Field::JoinPassword, Page::Password(id)) => self.join_with_password(id),
                _ => None,
            },
            MenuKey::Tab => {
                self.edit.focus = Some(match focus {
                    Field::NewName => Field::NewPassword,
                    Field::NewPassword => Field::NewName,
                    Field::JoinPassword => Field::JoinPassword,
                });
                None
            }
            MenuKey::Escape => {
                self.edit.focus = None;
                None
            }
        }
    }

    /// The Create button, or Enter on the create screen.
    fn create(&mut self) -> Option<MenuAction> {
        let name = self.edit.name.trim().to_string();
        if name.is_empty() {
            self.edit.focus = Some(Field::NewName);
            return None;
        }
        let password = Some(self.edit.password.clone()).filter(|p| !p.is_empty());
        self.edit.name.clear();
        self.edit.password.clear();
        self.go(Page::Groups);
        Some(MenuAction::CreateGroup { name, password })
    }

    /// The Join button, or Enter on the password screen. It stays on that screen, so a wrong
    /// password can be typed again; joining moves on to the group's members by itself.
    fn join_with_password(&mut self, id: Uuid) -> Option<MenuAction> {
        if self.edit.join_password.is_empty() {
            self.edit.focus = Some(Field::JoinPassword);
            return None;
        }
        let password = std::mem::take(&mut self.edit.join_password);
        self.edit.focus = Some(Field::JoinPassword);
        Some(MenuAction::JoinGroup(id, Some(password)))
    }
}

/// `text`, shortened with an ellipsis to fit `max` pixels.
fn fit(text: &str, max: f32, text_width: &dyn Fn(&str) -> f32) -> String {
    if text_width(text) <= max {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() && text_width(&(chars.iter().collect::<String>() + "…")) > max {
        chars.pop();
    }
    chars.into_iter().collect::<String>() + "…"
}

/// A button's or slider's label, centred and shortened to fit.
fn push_label(out: &mut Vec<Draw>, w: &Widget, text_width: &dyn Fn(&str) -> f32) {
    let room = if w.lock { w.rect[2] - 24.0 } else { w.rect[2] - 8.0 };
    let label = fit(&w.label, room, text_width);
    let tw = text_width(&label);
    let at = [w.rect[0] + (room + 8.0 - tw) / 2.0, w.rect[1] + (w.rect[3] - TEXT_H) / 2.0];
    out.push(Draw::Text { text: label, at, rgb: WHITE });
}

/// A text box's text: left-aligned, its end kept in view, asterisks for a secret, a caret when
/// focused, and the placeholder in grey when empty.
fn push_field_text(
    out: &mut Vec<Draw>,
    w: &Widget,
    placeholder: &str,
    secret: bool,
    focused: bool,
    text_width: &dyn Fn(&str) -> f32,
) {
    let at = [w.rect[0] + 4.0, w.rect[1] + (w.rect[3] - TEXT_H) / 2.0];
    if w.label.is_empty() && !focused {
        if !placeholder.is_empty() {
            out.push(Draw::Text { text: placeholder.into(), at, rgb: 0x777777 });
        }
        return;
    }
    let shown: String = if secret { "*".repeat(w.label.chars().count()) } else { w.label.clone() };
    let mut text = if focused { format!("{shown}_") } else { shown };
    let max = w.rect[2] - 8.0;
    while text_width(&text) > max && !text.is_empty() {
        text.remove(0);
    }
    out.push(Draw::Text { text, at, rgb: WHITE });
}

/// A player's coloured square with their initial (group members): 12 x 12 at `at`.
fn avatar(id: Uuid, name: &str, at: [f32; 2], text_width: &dyn Fn(&str) -> f32) -> Vec<Draw> {
    // A steady colour per player, from their id.
    let h = id.0.iter().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(*b as u32));
    let palette = [0xFF3F_7FBF, 0xFFBF_6F3F, 0xFF4F_9F5F, 0xFF9F_4FAF, 0xFFBF_9F3F, 0xFF3F_AFAF, 0xFFAF_4F6F];
    let initial: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    let tw = text_width(&initial);
    vec![
        Draw::Fill { rect: [at[0], at[1], 12.0, 12.0], argb: palette[h as usize % palette.len()] },
        Draw::Text { text: initial, at: [at[0] + (12.0 - tw) / 2.0, at[1] + 2.0], rgb: WHITE },
    ]
}

/// Where along a slider `x` falls, as a value.
fn slider_value(rect: [f32; 4], min: f32, max: f32, x: f32) -> f32 {
    let t = ((x - rect[0]) / rect[2]).clamp(0.0, 1.0);
    let v = min + (max - min) * t;
    // Snap volumes to 5 % and decibels to 1 dB, so values read cleanly.
    if max - min <= VOLUME_MAX + 0.001 { (v * 20.0).round() / 20.0 } else { v.round() }
}

fn slider_action(id: &Id, v: f32) -> Option<MenuAction> {
    Some(match id {
        Id::Threshold => MenuAction::SetThreshold(v),
        Id::MicVolume => MenuAction::SetMicVolume(v),
        Id::VoiceVolume => MenuAction::SetVoiceVolume(v),
        Id::Player(u) => MenuAction::SetPlayerVolume(*u, v),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUI: [f32; 2] = [480.0, 270.0];

    fn width(s: &str) -> f32 {
        s.chars().count() as f32 * 6.0
    }

    fn model() -> MenuModel {
        MenuModel {
            status: "Connected".into(),
            connected: true,
            push_to_talk: false,
            ptt_key: "Mouse 4".into(),
            threshold_db: -40.0,
            level_db: -30.0,
            mic_volume: 1.0,
            voice_volume: 1.0,
            microphone: "system default".into(),
            speakers: "system default".into(),
            players: vec![(Uuid([1; 16]), "Alice".into(), 1.0, true), (Uuid([2; 16]), "Bob".into(), 0.5, false)],
            groups: vec![(Uuid([8; 16]), "Open".into(), false, 1, false), (Uuid([9; 16]), "Secret".into(), true, 3, false)],
            ..MenuModel::default()
        }
    }

    fn in_group() -> MenuModel {
        let mut m = model();
        m.group = Some("Open".into());
        m.groups[0].4 = true;
        m.group_members = vec![(Uuid([1; 16]), "Alice".into(), true), (Uuid([3; 16]), "Carol".into(), false)];
        m
    }

    /// The centre of the widget with this id on the current screen.
    fn at(menu: &Menu, m: &MenuModel, id: &Id) -> [f32; 2] {
        let w = menu.layout(m, GUI).widgets.into_iter().find(|w| &w.id == id).unwrap_or_else(|| panic!("no {id:?} on {:?}", menu.page(m)));
        [w.rect[0] + w.rect[2] / 2.0, w.rect[1] + w.rect[3] / 2.0]
    }

    fn click(menu: &mut Menu, m: &MenuModel, id: Id) -> Option<MenuAction> {
        let p = at(menu, m, &id);
        menu.press(m, GUI, p)
    }

    fn texts(d: &[Draw]) -> Vec<String> {
        d.iter().filter_map(|x| if let Draw::Text { text, .. } = x { Some(text.clone()) } else { None }).collect()
    }

    #[test]
    fn opens_on_the_main_screen_and_navigates() {
        let m = model();
        let mut menu = Menu::new();
        assert_eq!(menu.page(&m), Page::Main);
        for (target, title) in [(Page::Settings, "Settings"), (Page::Players, "Players nearby"), (Page::Groups, "Groups")] {
            assert_eq!(click(&mut menu, &m, Id::Go(target)), None);
            assert_eq!(menu.page(&m), target);
            assert!(texts(&menu.draw(&m, GUI, [0.0; 2], &width)).contains(&title.to_string()));
            click(&mut menu, &m, Id::Back);
            assert_eq!(menu.page(&m), Page::Main);
        }
        // The main screen has no Back; reopening starts on it.
        assert!(menu.layout(&m, GUI).widgets.iter().all(|w| w.id != Id::Back));
        click(&mut menu, &m, Id::Go(Page::Settings));
        menu.open();
        assert_eq!(menu.page(&m), Page::Main);
    }

    #[test]
    fn the_main_screen_toggles() {
        let mut m = model();
        let mut menu = Menu::new();
        assert_eq!(click(&mut menu, &m, Id::Mute), Some(MenuAction::ToggleMute));
        assert_eq!(click(&mut menu, &m, Id::Deafen), Some(MenuAction::ToggleDeafen));
        assert_eq!(click(&mut menu, &m, Id::VoiceChat), Some(MenuAction::ToggleVoiceChat));
        m.muted = true;
        let d = menu.draw(&m, GUI, [0.0; 2], &width);
        assert!(texts(&d).contains(&"Muted".to_string()));
    }

    #[test]
    fn settings_buttons_and_sliders() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Settings));
        assert_eq!(click(&mut menu, &m, Id::Mode), Some(MenuAction::ToggleMode));
        assert_eq!(click(&mut menu, &m, Id::Noise), Some(MenuAction::ToggleNoiseSuppression));
        assert_eq!(click(&mut menu, &m, Id::Microphone), Some(MenuAction::NextMicrophone));
        assert_eq!(click(&mut menu, &m, Id::Speakers), Some(MenuAction::NextSpeakers));
        // The middle of a 0-200 % slider is 100 %; dragging past the end clamps.
        assert_eq!(click(&mut menu, &m, Id::MicVolume), Some(MenuAction::SetMicVolume(1.0)));
        assert_eq!(menu.drag(&m, GUI, [9999.0, 0.0]), Some(MenuAction::SetMicVolume(2.0)));
        assert!(menu.release());
        assert!(!menu.release());
        assert_eq!(click(&mut menu, &m, Id::Threshold), Some(MenuAction::SetThreshold(-30.0)));
        menu.release();
        let mut ptt = m.clone();
        ptt.push_to_talk = true;
        assert_eq!(click(&mut menu, &ptt, Id::Threshold), None, "no threshold in push to talk");
    }

    #[test]
    fn the_threshold_slider_shows_the_live_level() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Settings));
        let r = menu.layout(&m, GUI).widgets.into_iter().find(|w| w.id == Id::Threshold).unwrap().rect;
        let d = menu.draw(&m, GUI, [0.0; 2], &width);
        // -30 dB of -60..0 fills half the track.
        assert!(d.contains(&Draw::Fill { rect: [r[0], r[1], r[2] * 0.5, r[3]], argb: LEVEL }));
    }

    #[test]
    fn players_have_volume_sliders() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Players));
        let r = menu.layout(&m, GUI).widgets.into_iter().find(|w| w.id == Id::Player(Uuid([2; 16]))).unwrap().rect;
        assert_eq!(
            menu.press(&m, GUI, [r[0] + r[2] / 4.0, r[1] + 1.0]),
            Some(MenuAction::SetPlayerVolume(Uuid([2; 16]), 0.5))
        );
        assert!(texts(&menu.draw(&m, GUI, [0.0; 2], &width)).iter().any(|t| t.starts_with("> Alice")));
        let mut none = model();
        none.players.clear();
        assert!(texts(&menu.draw(&none, GUI, [0.0; 2], &width)).contains(&"No players nearby".to_string()));
    }

    #[test]
    fn an_open_group_joins_with_a_click() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Groups));
        assert_eq!(click(&mut menu, &m, Id::Group(Uuid([8; 16]))), Some(MenuAction::JoinGroup(Uuid([8; 16]), None)));
    }

    #[test]
    fn a_password_group_has_a_padlock_and_its_own_screen() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Groups));
        let l = menu.layout(&m, GUI);
        assert!(l.widgets.iter().any(|w| w.id == Id::Group(Uuid([9; 16])) && w.lock));
        assert!(l.widgets.iter().any(|w| w.id == Id::Group(Uuid([8; 16])) && !w.lock));

        assert_eq!(click(&mut menu, &m, Id::Group(Uuid([9; 16]))), None);
        assert_eq!(menu.page(&m), Page::Password(Uuid([9; 16])));
        assert!(menu.typing(), "the password box has focus");
        assert!(texts(&menu.draw(&m, GUI, [0.0; 2], &width)).contains(&"Join Secret".to_string()));
        assert_eq!(click(&mut menu, &m, Id::Join), None, "nothing typed yet");
        for c in "1221".chars() {
            menu.type_char(c);
        }
        assert_eq!(menu.key(MenuKey::Enter), Some(MenuAction::JoinGroup(Uuid([9; 16]), Some("1221".into()))));
        // A wrong password is shown on the same screen, to try again.
        let mut wrong = m.clone();
        wrong.group_problem = Some("wrong password".into());
        assert!(texts(&menu.draw(&wrong, GUI, [0.0; 2], &width)).iter().any(|t| t.contains("wrong password")));
        // Once in, the screen becomes the group's.
        assert_eq!(menu.page(&in_group()), Page::Groups);
    }

    #[test]
    fn create_a_group_by_typing() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Groups));
        click(&mut menu, &m, Id::Go(Page::CreateGroup));
        assert_eq!(menu.page(&m), Page::CreateGroup);
        assert!(menu.typing(), "the name box has focus");
        assert_eq!(click(&mut menu, &m, Id::Create), None, "no name yet");
        for c in "Raid team".chars() {
            menu.type_char(c);
        }
        menu.type_char('\u{8}'); // control characters are ignored
        menu.key(MenuKey::Tab);
        for c in "pw12".chars() {
            menu.type_char(c);
        }
        menu.key(MenuKey::Backspace);
        assert_eq!(menu.key(MenuKey::Enter), Some(MenuAction::CreateGroup { name: "Raid team".into(), password: Some("pw1".into()) }));
        assert_eq!(menu.page(&m), Page::Groups, "back to the groups screen");
        assert!(!menu.typing());
        // And with the button, without a password.
        click(&mut menu, &m, Id::Go(Page::CreateGroup));
        for c in "Open2".chars() {
            menu.type_char(c);
        }
        assert_eq!(click(&mut menu, &m, Id::Create), Some(MenuAction::CreateGroup { name: "Open2".into(), password: None }));
    }

    #[test]
    fn text_boxes_limit_length_mask_passwords_and_lose_focus() {
        let m = model();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Groups));
        click(&mut menu, &m, Id::Go(Page::CreateGroup));
        for _ in 0..100 {
            menu.type_char('a');
        }
        assert_eq!(menu.edit.name.chars().count(), MAX_NAME);
        menu.key(MenuKey::Tab);
        for c in "secret".chars() {
            menu.type_char(c);
        }
        let shown = texts(&menu.draw(&m, GUI, [0.0; 2], &width));
        assert!(shown.contains(&"******_".to_string()), "masked, with a caret: {shown:?}");
        assert!(!shown.iter().any(|t| t.contains("secret")));
        menu.key(MenuKey::Escape);
        assert!(!menu.typing());
        click(&mut menu, &m, Id::Text(Field::NewName));
        assert!(menu.typing());
        menu.press(&m, GUI, [1.0, 1.0]);
        assert!(!menu.typing(), "a click outside the boxes unfocuses");
    }

    #[test]
    fn in_a_group_shows_members_and_leave() {
        let m = in_group();
        let mut menu = Menu::new();
        assert!(texts(&menu.draw(&m, GUI, [0.0; 2], &width)).contains(&"Group: Open".to_string()), "named on the main screen");
        click(&mut menu, &m, Id::Go(Page::Groups));
        let d = menu.draw(&m, GUI, [0.0; 2], &width);
        let shown = texts(&d);
        assert!(shown.contains(&"Group: Open".to_string()));
        assert!(shown.iter().any(|t| t.starts_with("Alice") && t.contains("talking")));
        assert!(shown.contains(&"Carol".to_string()));
        assert!(shown.contains(&"C".to_string()), "Carol's initial");
        assert_eq!(click(&mut menu, &m, Id::Member(Uuid([3; 16]))), None, "members aren't buttons");
        assert_eq!(click(&mut menu, &m, Id::Leave), Some(MenuAction::LeaveGroup));
    }

    #[test]
    fn every_screen_fits_with_long_lists_and_small_windows() {
        let mut m = model();
        m.players = (0..30).map(|i| (Uuid([i; 16]), format!("Player{i}"), 1.0, false)).collect();
        m.groups = (0..30).map(|i| (Uuid([i; 16]), format!("Group{i}"), i % 2 == 0, 2, false)).collect();
        let mut g = in_group();
        g.group_members = (0..30).map(|i| (Uuid([i; 16]), format!("M{i}"), false)).collect();
        // 427 x 240 is the smallest GUI Minecraft gives (its minimum window at GUI scale 2).
        for gui in [GUI, [427.0, 240.0], [640.0, 360.0]] {
            for page in [Page::Main, Page::Settings, Page::Players, Page::Groups, Page::CreateGroup, Page::Password(Uuid([0; 16]))] {
                for model in [&m, &g] {
                    let mut menu = Menu::new();
                    menu.page = page;
                    let l = menu.layout(model, gui);
                    assert!(l.panel[0] >= 0.0 && l.panel[0] + l.panel[2] <= gui[0], "{page:?} {gui:?} too wide");
                    assert!(
                        l.panel[1] >= 0.0 && l.panel[1] + l.panel[3] <= gui[1] + 0.01,
                        "{page:?} {gui:?}: panel {:?} doesn't fit",
                        l.panel
                    );
                    for w in &l.widgets {
                        assert!(inside(l.panel, [w.rect[0], w.rect[1]]), "{page:?}: {:?} outside", w.id);
                    }
                }
            }
        }
    }

    #[test]
    fn long_names_are_shortened() {
        let mut m = model();
        m.microphone = "A very long ALSA device name that goes on and on and on".into();
        let mut menu = Menu::new();
        click(&mut menu, &m, Id::Go(Page::Settings));
        let r = menu.layout(&m, GUI).widgets.into_iter().find(|w| w.id == Id::Microphone).unwrap().rect;
        let label = texts(&menu.draw(&m, GUI, [0.0; 2], &width)).into_iter().find(|t| t.starts_with("Input:")).unwrap();
        assert!(width(&label) <= r[2] - 8.0, "{label}");
        assert!(label.ends_with('…'));
    }

    #[test]
    fn clicks_on_the_panel_are_contained() {
        let m = model();
        let menu = Menu::new();
        let p = menu.layout(&m, GUI).panel;
        assert!(menu.contains(&m, GUI, [p[0] + 1.0, p[1] + 1.0]));
        assert!(!menu.contains(&m, GUI, [1.0, 1.0]));
    }
}
