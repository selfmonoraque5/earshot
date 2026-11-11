// HEAD of native_tick: positions, push-to-talk and outgoing messages, once a tick.
fn earshot_tick(game: &mut crate::game::Game<'_>) {
    crate::game::earshot::tick(game);
}