// HEAD of native_screen_mouse_button_pressed_pre: clicks in the voice menu.
fn earshot_screen_press(
    game: &crate::game::Game<'_>,
    screen: &horizon_client::mod_api::NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    crate::game::earshot::screen_press(game, screen, button, canceled);
}