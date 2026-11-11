// HEAD of native_key: push-to-talk, mute and deafen keys.
fn earshot_key(game: &mut crate::game::Game<'_>, key: &str, pressed: bool) {
    crate::game::earshot::key(game, key, pressed);
}