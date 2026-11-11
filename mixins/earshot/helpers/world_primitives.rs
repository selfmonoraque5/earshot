// HEAD of native_world_primitives: the speaker icon beside talking players' name tags.
fn earshot_world_primitives(
    game: &crate::game::Game<'_>,
    partial: f32,
    primitives: &mut horizon_client::world_primitives::WorldPrimitives,
) {
    crate::game::earshot::world_primitives(game, partial, primitives);
}