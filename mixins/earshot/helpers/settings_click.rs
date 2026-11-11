// HEAD of native_settings_click: a settings page button was clicked.
fn earshot_settings_click(game: &mut crate::game::Game<'_>, mod_id: &str, button: usize) {
    crate::game::earshot::settings_click(game, mod_id, button);
}