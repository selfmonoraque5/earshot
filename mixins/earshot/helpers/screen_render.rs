// HEAD of native_screen_render_post: draws the voice menu when it's the open screen.
fn earshot_screen_render(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
) {
    crate::game::earshot::screen_render(game, batch, screen);
}