// HEAD of native_settings: Earshot's settings page buttons.
fn earshot_settings(game: &mut crate::game::Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    crate::game::earshot::settings(game, mod_id, buttons);
}