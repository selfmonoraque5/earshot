// HEAD of remote.rs's chat_received: a chat line from the server, before it shows. Voice chat
// lines the game has no words for (a group invite) get Earshot's own wording.
fn earshot_chat_received(game: &crate::game::Game<'_>, message: &mut horizon_client::text::Text) {
    crate::game::earshot::chat_received(game, message);
}
