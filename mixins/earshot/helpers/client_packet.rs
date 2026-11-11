// HEAD of native_client_packet: every play packet from the server, SVC's plugin messages among them.
fn earshot_client_packet(
    game: &mut crate::game::Game<'_>,
    packet: &horizon_client::mod_api::ClientboundPlayPacket,
) {
    crate::game::earshot::client_packet(game, packet);
}