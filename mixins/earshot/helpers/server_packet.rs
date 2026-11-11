// HEAD of lan_guest_packet: every play packet a player sends a Horizon server. SVC's plugin
// messages go to Earshot's voice server (Horizon itself drops channels it doesn't know).
fn earshot_server_packet(
    game: &mut crate::game::Game<'_>,
    host: &mut crate::game::lan::LanHost,
    conn: u32,
    packet: &horizon_protocol::packets::play::Serverbound,
) {
    crate::game::earshot::server_packet(game, host, conn, packet);
}