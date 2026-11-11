// HEAD of native_screen_mouse_dragged_pre: dragging a slider in the voice menu.
fn earshot_screen_drag(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    canceled: &mut bool,
) {
    crate::game::earshot::screen_drag(game, screen, canceled);
}