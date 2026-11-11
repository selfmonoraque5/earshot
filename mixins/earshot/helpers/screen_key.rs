// HEAD of native_screen_key_pressed_pre: keys for the voice menu's text boxes.
fn earshot_screen_key(game: &crate::game::Game<'_>, key: &str, canceled: &mut bool) {
    crate::game::earshot::screen_key(game, key, canceled);
}
