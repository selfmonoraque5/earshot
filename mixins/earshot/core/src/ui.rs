//! Earshot's drawing primitive and icons, independent of the game. The voice menu built from
//! them is [`crate::menu`].
//!
//! Everything here produces plain [`Draw`] commands in GUI pixels (or icon units); the game glue
//! only paints them with Horizon's helpers. All icons are Earshot's own, drawn from rectangles
//! and lines.

/// One thing to paint. Colours are ARGB for shapes, RGB for text.
#[derive(Debug, Clone, PartialEq)]
pub enum Draw {
    Fill { rect: [f32; 4], argb: u32 },
    Quad { points: [[f32; 2]; 4], argb: u32 },
    Text { text: String, at: [f32; 2], rgb: u32 },
}

/// The state the HUD microphone icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicState {
    /// Connected, not sending.
    Idle,
    /// Sending right now.
    Talking,
    Muted,
    /// Voice chat switched off.
    Off,
}

/// A thick line from `a` to `b` as a quad.
pub fn line_quad(a: [f32; 2], b: [f32; 2], width: f32) -> [[f32; 2]; 4] {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    let (nx, ny) = (-dy / len * width / 2.0, dx / len * width / 2.0);
    [[a[0] + nx, a[1] + ny], [b[0] + nx, b[1] + ny], [b[0] - nx, b[1] - ny], [a[0] - nx, a[1] - ny]]
}

/// The microphone icon, 16 x 16 GUI pixels with its top-left at `at`.
pub fn mic_icon(at: [f32; 2], state: MicState) -> Vec<Draw> {
    let argb = match state {
        MicState::Idle => 0xFFDDDDDD,
        MicState::Talking => 0xFF55FF55,
        MicState::Muted => 0xFFFF5555,
        MicState::Off => 0xFF888888,
    };
    let r = |x: f32, y: f32, w: f32, h: f32| Draw::Fill { rect: [at[0] + x, at[1] + y, w, h], argb };
    let mut out = vec![
        Draw::Fill { rect: [at[0] - 2.0, at[1] - 2.0, 20.0, 20.0], argb: 0x7800_0000 },
        // Capsule, with rounded ends.
        r(6.0, 1.0, 4.0, 1.0),
        r(5.0, 2.0, 6.0, 7.0),
        r(6.0, 9.0, 4.0, 1.0),
        // Holder: a U around the capsule.
        r(3.0, 6.0, 1.0, 4.0),
        r(12.0, 6.0, 1.0, 4.0),
        r(4.0, 10.0, 1.0, 1.0),
        r(11.0, 10.0, 1.0, 1.0),
        r(5.0, 11.0, 6.0, 1.0),
        // Stem and base.
        r(7.5, 12.0, 1.0, 2.0),
        r(5.0, 14.0, 6.0, 1.0),
    ];
    if matches!(state, MicState::Muted | MicState::Off) {
        out.push(Draw::Quad { points: line_quad([at[0] + 2.0, at[1] + 1.0], [at[0] + 14.0, at[1] + 15.0], 1.6), argb: 0xFFFF3030 });
    }
    out
}

/// The speaker icon (deafened indicator on the HUD), 16 x 16 at `at`, crossed out.
pub fn deafened_icon(at: [f32; 2]) -> Vec<Draw> {
    let argb = 0xFFFF5555;
    let p = |x: f32, y: f32| [at[0] + x, at[1] + y];
    let mut out = vec![Draw::Fill { rect: [at[0] - 2.0, at[1] - 2.0, 20.0, 20.0], argb: 0x7800_0000 }];
    out.push(Draw::Fill { rect: [at[0] + 2.0, at[1] + 6.0, 3.0, 4.0], argb });
    out.push(Draw::Quad { points: [p(5.0, 6.0), p(9.0, 2.0), p(9.0, 14.0), p(5.0, 10.0)], argb });
    for (a, b) in speaker_waves() {
        out.push(Draw::Quad { points: line_quad(p(a[0] + 8.0, 8.0 - a[1]), p(b[0] + 8.0, 8.0 - b[1]), 1.0), argb });
    }
    out.push(Draw::Quad { points: line_quad(p(1.0, 1.0), p(15.0, 15.0), 1.6), argb: 0xFFFF3030 });
    out
}

/// A small padlock (password groups), 7 x 9 GUI pixels with its top-left at `at`.
pub fn lock_icon(at: [f32; 2], argb: u32) -> Vec<Draw> {
    let r = |x: f32, y: f32, w: f32, h: f32| Draw::Fill { rect: [at[0] + x, at[1] + y, w, h], argb };
    vec![
        // Shackle.
        r(2.0, 0.0, 3.0, 1.0),
        r(1.0, 1.0, 1.0, 3.0),
        r(5.0, 1.0, 1.0, 3.0),
        // Body, with a keyhole left open.
        r(0.0, 4.0, 7.0, 2.0),
        r(0.0, 6.0, 3.0, 1.0),
        r(4.0, 6.0, 3.0, 1.0),
        r(0.0, 7.0, 7.0, 2.0),
    ]
}

/// One member line of the group panel.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelMember {
    pub name: String,
    /// Talking right now: heard in the last moment, or our own microphone sending.
    pub talking: bool,
    /// The player themselves.
    pub me: bool,
}

/// The most members the group panel lists; the rest are counted on a last line.
pub const PANEL_MAX_MEMBERS: usize = 8;
/// A line's height and the panel's padding, in GUI pixels.
const PANEL_ROW: f32 = 11.0;
const PANEL_PAD: f32 = 3.0;
/// Room for the speaker before each name.
const PANEL_ICON: f32 = 10.0;

/// The panel shown in the top-left corner while in a voice group: the group's name, then its
/// members, those talking now in white with a green speaker beside them, the others grey.
/// `width_of` measures text in GUI pixels. Returns the panel's size and its draws, with its
/// top-left at (0, 0): [`translate`] moves them where the panel goes.
pub fn group_panel(group: &str, members: &[PanelMember], width_of: &dyn Fn(&str) -> f32) -> ([f32; 2], Vec<Draw>) {
    let title = format!("Group: {group}");
    let shown = &members[..members.len().min(PANEL_MAX_MEMBERS)];
    let more = (members.len() > PANEL_MAX_MEMBERS).then(|| format!("+{} more", members.len() - PANEL_MAX_MEMBERS));
    let label = |m: &PanelMember| if m.me { format!("{} (you)", m.name) } else { m.name.clone() };

    let mut width = width_of(&title);
    for m in shown {
        width = width.max(PANEL_ICON + width_of(&label(m)));
    }
    if let Some(t) = &more {
        width = width.max(PANEL_ICON + width_of(t));
    }
    let lines = 1 + shown.len() + usize::from(more.is_some());
    let size = [width + 2.0 * PANEL_PAD, lines as f32 * PANEL_ROW + 2.0 * PANEL_PAD - 2.0];

    let mut out = vec![
        Draw::Fill { rect: [0.0, 0.0, size[0], size[1]], argb: 0x7800_0000 },
        Draw::Text { text: title, at: [PANEL_PAD, PANEL_PAD], rgb: 0x7CE38B },
    ];
    let line_y = |i: usize| PANEL_PAD + (i + 1) as f32 * PANEL_ROW;
    for (i, m) in shown.iter().enumerate() {
        let y = line_y(i);
        if m.talking {
            out.extend(small_speaker([PANEL_PAD, y], 0xFF55FF55));
        }
        let rgb = if m.talking { 0xFFFFFF } else { 0xAAAAAA };
        out.push(Draw::Text { text: label(m), at: [PANEL_PAD + PANEL_ICON, y], rgb });
    }
    if let Some(t) = more {
        out.push(Draw::Text { text: t, at: [PANEL_PAD + PANEL_ICON, line_y(shown.len())], rgb: 0x888888 });
    }
    (size, out)
}

/// A small speaker for the group panel, 7 x 8 GUI pixels with its top-left at `at`.
fn small_speaker(at: [f32; 2], argb: u32) -> Vec<Draw> {
    let r = |x: f32, y: f32, w: f32, h: f32| Draw::Fill { rect: [at[0] + x, at[1] + y, w, h], argb };
    vec![
        // Box and cone.
        r(0.0, 3.0, 2.0, 2.0),
        r(2.0, 2.0, 1.0, 4.0),
        r(3.0, 1.0, 1.0, 6.0),
        // One wave.
        r(5.0, 2.0, 1.0, 1.0),
        r(6.0, 3.0, 1.0, 2.0),
        r(5.0, 5.0, 1.0, 1.0),
    ]
}

/// `draws` moved by `by` GUI pixels.
pub fn translate(draws: Vec<Draw>, by: [f32; 2]) -> Vec<Draw> {
    let p = |q: [f32; 2]| [q[0] + by[0], q[1] + by[1]];
    draws
        .into_iter()
        .map(|d| match d {
            Draw::Fill { rect, argb } => Draw::Fill { rect: [rect[0] + by[0], rect[1] + by[1], rect[2], rect[3]], argb },
            Draw::Quad { points, argb } => Draw::Quad { points: points.map(p), argb },
            Draw::Text { text, at, rgb } => Draw::Text { text, at: p(at), rgb },
        })
        .collect()
}

/// The sound waves of the speaker icon: line segments in icon units, around (0, 0), y up.
fn speaker_waves() -> Vec<([f32; 2], [f32; 2])> {
    let mut segs = Vec::new();
    for radius in [2.6f32, 4.8] {
        let steps = 4;
        for i in 0..steps {
            let a0 = (-50.0 + 100.0 * i as f32 / steps as f32).to_radians();
            let a1 = (-50.0 + 100.0 * (i + 1) as f32 / steps as f32).to_radians();
            segs.push(([radius * a0.cos(), radius * a0.sin()], [radius * a1.cos(), radius * a1.sin()]));
        }
    }
    segs
}

/// The speaker icon drawn beside a talking player's name tag: line segments in font pixels,
/// centred on (0, 0), x to the right and y up, about 12 wide and 9 tall.
pub fn speaker_lines() -> Vec<([f32; 2], [f32; 2])> {
    let mut segs = vec![
        // Box.
        ([-6.0, -1.5], [-4.0, -1.5]),
        ([-4.0, -1.5], [-4.0, 1.5]),
        ([-4.0, 1.5], [-6.0, 1.5]),
        ([-6.0, 1.5], [-6.0, -1.5]),
        // Cone.
        ([-4.0, -1.5], [-1.0, -4.0]),
        ([-1.0, -4.0], [-1.0, 4.0]),
        ([-1.0, 4.0], [-4.0, 1.5]),
    ];
    segs.extend(speaker_waves().into_iter().map(|(a, b)| ([a[0] + 0.5, a[1]], [b[0] + 0.5, b[1]])));
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(d: &Draw) -> Vec<[f32; 2]> {
        match d {
            Draw::Fill { rect, .. } => vec![[rect[0], rect[1]], [rect[0] + rect[2], rect[1] + rect[3]]],
            Draw::Quad { points, .. } => points.to_vec(),
            Draw::Text { .. } => vec![],
        }
    }

    #[test]
    fn icons_stay_in_their_box() {
        for state in [MicState::Idle, MicState::Talking, MicState::Muted, MicState::Off] {
            for d in mic_icon([100.0, 100.0], state) {
                for p in points(&d) {
                    assert!((97.0..=119.0).contains(&p[0]) && (97.0..=119.0).contains(&p[1]), "{state:?}: {p:?}");
                }
            }
        }
        assert!(mic_icon([0.0, 0.0], MicState::Muted).iter().any(|d| matches!(d, Draw::Quad { .. })), "muted is crossed out");
        assert!(!mic_icon([0.0, 0.0], MicState::Talking).iter().any(|d| matches!(d, Draw::Quad { .. })));
        for d in lock_icon([10.0, 10.0], 0xFFFFFFFF) {
            for p in points(&d) {
                assert!((10.0..=17.0).contains(&p[0]) && (10.0..=19.0).contains(&p[1]), "lock: {p:?}");
            }
        }
        for (a, b) in speaker_lines() {
            for p in [a, b] {
                assert!(p[0].abs() <= 6.5 && p[1].abs() <= 5.0, "{p:?}");
            }
        }
    }

    #[test]
    fn line_quads_have_the_width_asked_for() {
        let q = line_quad([0.0, 0.0], [10.0, 0.0], 2.0);
        assert_eq!(q, [[0.0, 1.0], [10.0, 1.0], [10.0, -1.0], [0.0, -1.0]]);
    }

    fn member(name: &str, talking: bool, me: bool) -> PanelMember {
        PanelMember { name: name.into(), talking, me }
    }

    /// Text widths as 6 GUI pixels a character, like the game's font roughly is.
    fn width(t: &str) -> f32 {
        t.chars().count() as f32 * 6.0
    }

    fn texts(draws: &[Draw]) -> Vec<(String, u32)> {
        draws.iter().filter_map(|d| if let Draw::Text { text, rgb, .. } = d { Some((text.clone(), *rgb)) } else { None }).collect()
    }

    #[test]
    fn the_group_panel_lists_members_and_lights_up_who_talks() {
        let members = [member("AIOMarcus", true, false), member("NullPort404", false, true)];
        let (size, draws) = group_panel("Party", &members, &width);
        assert_eq!(
            texts(&draws),
            [
                ("Group: Party".to_string(), 0x7CE38B),
                ("AIOMarcus".to_string(), 0xFFFFFF),
                ("NullPort404 (you)".to_string(), 0xAAAAAA),
            ]
        );
        // As wide as the longest line plus padding; a title and two members tall.
        assert_eq!(size, [PANEL_ICON + width("NullPort404 (you)") + 2.0 * PANEL_PAD, 3.0 * PANEL_ROW + 2.0 * PANEL_PAD - 2.0]);
        // One green speaker, beside the talking member's line only.
        let green: Vec<&Draw> = draws.iter().filter(|d| matches!(d, Draw::Fill { argb: 0xFF55FF55, .. })).collect();
        assert!(!green.is_empty());
        let first_line = PANEL_PAD + PANEL_ROW;
        for d in green {
            for p in points(d) {
                assert!(p[1] >= first_line && p[1] <= first_line + 8.0 && p[0] <= PANEL_PAD + PANEL_ICON, "{p:?}");
            }
        }
        // Everything inside the panel's background.
        for d in &draws {
            for p in points(d) {
                assert!(p[0] >= 0.0 && p[1] >= 0.0 && p[0] <= size[0] && p[1] <= size[1], "{p:?}");
            }
        }
    }

    #[test]
    fn a_big_group_is_cut_short_and_counted() {
        let members: Vec<PanelMember> = (0..11).map(|i| member(&format!("P{i}"), false, false)).collect();
        let (size, draws) = group_panel("Big", &members, &width);
        let t = texts(&draws);
        assert_eq!(t.len(), 1 + PANEL_MAX_MEMBERS + 1);
        assert_eq!(t.last().unwrap().0, "+3 more");
        assert_eq!(size[1], (2 + PANEL_MAX_MEMBERS) as f32 * PANEL_ROW + 2.0 * PANEL_PAD - 2.0);
    }

    #[test]
    fn translate_moves_every_kind_of_draw() {
        let moved = translate(
            vec![
                Draw::Fill { rect: [1.0, 2.0, 3.0, 4.0], argb: 1 },
                Draw::Quad { points: [[0.0, 0.0]; 4], argb: 2 },
                Draw::Text { text: "a".into(), at: [5.0, 6.0], rgb: 3 },
            ],
            [10.0, 20.0],
        );
        assert_eq!(moved[0], Draw::Fill { rect: [11.0, 22.0, 3.0, 4.0], argb: 1 });
        assert_eq!(moved[1], Draw::Quad { points: [[10.0, 20.0]; 4], argb: 2 });
        assert_eq!(moved[2], Draw::Text { text: "a".into(), at: [15.0, 26.0], rgb: 3 });
    }
}
