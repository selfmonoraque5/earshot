// HEAD of native_mods: list Earshot and its keys under Options > Mods.
fn earshot_mods(mods: &mut Vec<horizon_client::mod_api::NativeMod>) {
    crate::game::earshot::mods(mods);
}