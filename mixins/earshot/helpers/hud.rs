// HEAD of native_hud: our microphone's state.
fn earshot_hud(
    game: &mut crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    corners: &mut horizon_client::mod_api::Corners,
) {
    crate::game::earshot::hud(game, batch, corners);
}