// HEAD of native_screen_char_typed_pre: typing into the voice menu's text boxes.
fn earshot_screen_char(game: &crate::game::Game<'_>, c: char, canceled: &mut bool) {
    crate::game::earshot::screen_char(game, c, canceled);
}
