//! Earshot on Horizon's mod API: proximity voice chat compatible with Simple Voice Chat
//! servers, and an SVC-compatible voice server on a Horizon dedicated server. The hooks
//! here feed `client` (the voice client, HUD, name tag icons and the voice menu) and
//! `server`; everything that doesn't need the game is in `earshot_core`.
//!
//! Earshot is an independent project, not affiliated with Simple Voice Chat.
use mod_api::{
    self, ClientChatReceivedEvent, ClientHost, ClientPacketEvent, ClientPlayerLoggingOutEvent,
    ClientTickPostEvent, Flow, Hooks, KeyRegistration, Registrar, RegistrationContext,
    RenderGuiPostEvent, RenderPlayerPostEvent, ScreenCharTypedPreEvent, ScreenKeyPressedPreEvent,
    ScreenMouseButtonPressedPreEvent, ScreenMouseButtonReleasedPreEvent, ScreenMouseDraggedPreEvent,
    ScreenRenderPostEvent, ServerHost, ServerTickEvent, SettingChangedEvent, SettingSpec,
};

pub mod client;
/// The voice server, also reached by the game patch.
pub mod server;
mod text_json;

use client::ID;

/// One font pixel of a name tag, in blocks.
const TAG_UNIT: f32 = 0.025;
/// A white texture the icon's lines are drawn with, tinted.
const WHITE: &str = "minecraft:block/white_concrete";

#[derive(Default)]
struct Earshot;

#[mod_api::horizon_mod]
impl Hooks for Earshot {
    /// The keys, and the settings page's two switches (the voice menu holds the rest).
    fn register(&self, r: &mut dyn Registrar, _: &RegistrationContext) {
        for (id, _, key) in client::KEYS {
            r.register_key_mapping(KeyRegistration::new(id, key).label(&format!("{ID}.key.{id}")));
        }
        let _ = r.register_setting(SettingSpec::toggle(client::SETTING_VOICE, format!("{ID}.option.voice_chat"), true));
        let _ = r.register_setting(SettingSpec::toggle(client::SETTING_DEBUG, format!("{ID}.option.debug"), false));
    }

    fn setting_changed(&self, host: &mut dyn ClientHost, event: &mut SettingChangedEvent<'_>) {
        client::page_changed(&*host, event.key);
        // Turning voice off or on is a message to the server.
        client::flush(host);
    }

    /// Every play packet from the server: SVC's plugin messages go to the voice client.
    fn client_packet(&self, host: &mut dyn ClientHost, event: &mut ClientPacketEvent<'_>) {
        let Some(payload) = &event.packet.custom_payload else { return };
        let server = host.client_session().client_server().and_then(|c| c.address).unwrap_or_default();
        client::plugin_message(host, &payload.channel, &payload.data, &server);
    }

    /// A chat line from the server: voice chat lines in Earshot's own words.
    fn client_chat_received(&self, host: &mut dyn ClientHost, event: &mut ClientChatReceivedEvent<'_>) -> Flow {
        client::chat_line(&*host, event.message);
        Flow::Continue
    }

    fn client_tick_post(&self, host: &mut dyn ClientHost, _: &mut ClientTickPostEvent) {
        client::tick(host);
    }

    fn client_player_logging_out(&self, _: &mut dyn ClientHost, _: &mut ClientPlayerLoggingOutEvent) {
        client::disconnecting();
    }

    fn render_gui_post(&self, host: &dyn ClientHost, event: &mut RenderGuiPostEvent<'_>) {
        client::hud(host, &mut *event.draw, &mut *event.corners);
    }

    /// Beside each other player's name tag, the icon `client::tag_icon` gives, facing the camera.
    fn render_player_post(&self, host: &dyn ClientHost, event: &mut RenderPlayerPostEvent<'_>) {
        if event.who.local {
            return;
        }
        let Some((lines, argb)) = client::tag_icon(event.player.id) else { return };
        let Some(camera) = host.camera() else { return };
        let draw = &mut *event.draw;
        let name_w = host
            .text_width(&mod_api::Text::literal(event.who.name))
            .map_or(event.who.name.len() as f32 * 6.0, |w| w as f32);
        // The tag hangs half a block over the head; its text's middle 4 font pixels below its top.
        let height = if event.player.baby { 0.9 } else { 1.8 };
        draw.push();
        draw.translate([0.0, height + 0.5 - 4.0 * TAG_UNIT, 0.0]);
        // The pose turns with the body (+z the way it faces); face the camera instead.
        draw.rotate_about([0.0, 1.0, 0.0], event.player.body_yaw - camera.yaw - 180.0, [0.0; 3]);
        // Past the tag's right end: half its width, then the icon's half width and a gap.
        let offset = (name_w / 2.0 + 9.0) * TAG_UNIT;
        for (a, b) in lines {
            let p = |q: [f32; 2]| [-(offset + q[0] * TAG_UNIT), -q[1] * TAG_UNIT];
            let (pa, pb) = (p(a), p(b));
            // A line two font pixels wide: a thin quad across it.
            let (dx, dy) = (pb[0] - pa[0], pb[1] - pa[1]);
            let len = (dx * dx + dy * dy).sqrt().max(1e-6);
            let (nx, ny) = (-dy / len * TAG_UNIT, dx / len * TAG_UNIT);
            let corners = [
                [pa[0] + nx, pa[1] + ny, 0.0],
                [pb[0] + nx, pb[1] + ny, 0.0],
                [pb[0] - nx, pb[1] - ny, 0.0],
                [pa[0] - nx, pa[1] - ny, 0.0],
            ];
            draw.quad(WHITE, corners, [0.0, 0.0, 1.0, 1.0], argb);
        }
        draw.pop();
    }

    fn screen_render_post(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        if client::ours(host) {
            client::screen_render(host, &mut *event.draw, event.screen.mouse);
        }
    }

    fn screen_mouse_button_pressed_pre(&self, host: &dyn ClientHost, event: &mut ScreenMouseButtonPressedPreEvent<'_>) -> Flow {
        if !client::ours(host) || event.button != 0 {
            return Flow::Continue;
        }
        if client::screen_press(host, event.screen.rect, event.screen.mouse) { Flow::Cancel } else { Flow::Continue }
    }

    fn screen_mouse_dragged_pre(&self, host: &dyn ClientHost, event: &mut ScreenMouseDraggedPreEvent<'_>) -> Flow {
        if client::ours(host) && client::screen_drag(host, event.screen.rect, event.screen.mouse) {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    fn screen_mouse_button_released_pre(&self, host: &dyn ClientHost, _: &mut ScreenMouseButtonReleasedPreEvent<'_>) -> Flow {
        if client::ours(host) && client::screen_release(host) { Flow::Cancel } else { Flow::Continue }
    }

    fn screen_key_pressed_pre(&self, host: &dyn ClientHost, event: &mut ScreenKeyPressedPreEvent<'_>) -> Flow {
        if client::ours(host) && client::screen_key(host, event.key) { Flow::Cancel } else { Flow::Continue }
    }

    fn screen_char_typed_pre(&self, host: &dyn ClientHost, event: &mut ScreenCharTypedPreEvent<'_>) -> Flow {
        if client::ours(host) && client::screen_char(event.c) { Flow::Cancel } else { Flow::Continue }
    }

    /// The server's tick: where the players are, for the voice server.
    fn server_tick(&self, host: &mut dyn ServerHost, _: &mut ServerTickEvent<'_>) {
        server::tick(host);
    }
}
