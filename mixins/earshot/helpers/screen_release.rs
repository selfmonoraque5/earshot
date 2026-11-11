// HEAD of native_screen_mouse_button_released_pre: a slider in the voice menu was let go.
fn earshot_screen_release(game: &crate::game::Game<'_>, canceled: &mut bool) {
    crate::game::earshot::screen_release(game, canceled);
}