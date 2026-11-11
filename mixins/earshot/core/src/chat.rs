//! Chat lines from a voice chat server that the game has no words for.
//!
//! Simple Voice Chat servers send some chat lines as bare translation keys under
//! `message.voicechat.` (a group invite is `message.voicechat.invite`), with the details as
//! the key's arguments. Only SVC's own client has text for those keys, so Horizon showed the
//! key itself. [`rewrite`] replaces each such key with Earshot's own lead-in and keeps the
//! server's arguments exactly as they came: names, group names and any clickable part
//! (a join link, say) still work.
//!
//! The wording here is Earshot's. No text from Simple Voice Chat is used, and the arguments'
//! meaning isn't assumed beyond their order: they're shown one after another.

use serde_json::{Map, Value};

/// The translation keys this rewrites.
pub const PREFIX: &str = "message.voicechat.";

/// Keys of a component that carry its content rather than its style.
const CONTENT_KEYS: [&str; 6] = ["translate", "with", "fallback", "text", "extra", "type"];

/// Earshot's lead-in for a `message.voicechat.` key: worded for the keys we know by name,
/// otherwise the key's own last part made readable ("voice chat: group full").
pub fn lead_in(key: &str) -> String {
    let name = key.strip_prefix(PREFIX).unwrap_or(key);
    match name {
        "invite" => "You were invited to a voice group".to_owned(),
        "accept_invite" => "Join".to_owned(),
        "accept_invite.hover" => "Click to join this voice group".to_owned(),
        other => format!("Voice chat: {}", other.replace(['_', '.'], " ")),
    }
}

/// Where a component keeps its hover text: `hover_event` (26.x) or `hoverEvent` (older), its
/// text under `value` or `contents`.
const HOVER_KEYS: [&str; 2] = ["hover_event", "hoverEvent"];
const HOVER_TEXT_KEYS: [&str; 2] = ["value", "contents"];

/// `o`'s hover text rewritten where needed: the event with its text replaced, `None` if unchanged.
fn rewrite_hover(o: &Map<String, Value>, translated: &dyn Fn(&str) -> bool) -> Option<(String, Value)> {
    for key in HOVER_KEYS {
        let Some(Value::Object(event)) = o.get(key) else { continue };
        for text_key in HOVER_TEXT_KEYS {
            if let Some(new) = event.get(text_key).and_then(|t| rewrite(t, translated)) {
                let mut event = event.clone();
                event.insert(text_key.into(), new);
                return Some((key.into(), Value::Object(event)));
            }
        }
    }
    None
}

/// `component` (a chat line as the server's JSON) with every `message.voicechat.` key the
/// game can't translate replaced by Earshot's wording; `None` when nothing needed it.
/// `translated` tells whether the game's language (or a resource pack) has text for a key:
/// such keys are left alone, as are keys that carry their own `fallback` text.
pub fn rewrite(component: &Value, translated: &dyn Fn(&str) -> bool) -> Option<Value> {
    match component {
        Value::Array(list) => {
            let mut changed = false;
            let out: Vec<Value> = list
                .iter()
                .map(|v| match rewrite(v, translated) {
                    Some(new) => {
                        changed = true;
                        new
                    }
                    None => v.clone(),
                })
                .collect();
            changed.then_some(Value::Array(out))
        }
        Value::Object(o) => rewrite_object(o, translated),
        _ => None,
    }
}

fn rewrite_object(o: &Map<String, Value>, translated: &dyn Fn(&str) -> bool) -> Option<Value> {
    let mut changed = false;
    // The arguments and siblings may hold keys of their own.
    let args: Vec<Value> = list_of(o.get("with"), translated, &mut changed);
    let extra: Vec<Value> = list_of(o.get("extra"), translated, &mut changed);
    // So does the hover text (the invite's join button explains itself there).
    let hover = rewrite_hover(o, translated);
    let mut o = o.clone();
    if let Some((k, event)) = hover {
        o.insert(k, event);
        changed = true;
    }
    let o = &o;

    let key = o.get("translate").and_then(Value::as_str);
    let ours = key.filter(|k| k.starts_with(PREFIX) && !o.contains_key("fallback") && !translated(k));
    let Some(key) = ours else {
        if !changed {
            return None;
        }
        let mut out = o.clone();
        if o.contains_key("with") {
            out.insert("with".into(), Value::Array(args));
        }
        if o.contains_key("extra") {
            out.insert("extra".into(), Value::Array(extra));
        }
        return Some(Value::Object(out));
    };

    // Our wording with the arguments as they came (spaced), then the original siblings. The
    // component's own style stays on the root, so the arguments inherit it as before.
    let mut args = args.into_iter();
    let mut parts = Vec::new();
    match (key.strip_prefix(PREFIX), args.next()) {
        // An invite's arguments are the inviter, the group's name and the join button
        // (PROTOCOL.md): "muhnip invited you to the voice group e [Join]".
        (Some("invite"), Some(inviter)) => {
            parts.push(inviter);
            match args.next() {
                Some(group) => {
                    parts.push(Value::String(" invited you to the voice group ".into()));
                    parts.push(group);
                }
                None => parts.push(Value::String(" invited you to a voice group".into())),
            }
        }
        (_, first) => {
            parts.push(Value::String(lead_in(key)));
            parts.extend(first.into_iter().flat_map(|a| [Value::String(" ".into()), a]));
        }
    }
    for arg in args {
        parts.push(Value::String(" ".into()));
        parts.push(arg);
    }
    parts.extend(extra);
    let mut out: Map<String, Value> =
        o.iter().filter(|(k, _)| !CONTENT_KEYS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    out.insert("text".into(), Value::String(String::new()));
    out.insert("extra".into(), Value::Array(parts));
    Some(Value::Object(out))
}

/// The components of a `with` or `extra` list, each rewritten where needed.
fn list_of(list: Option<&Value>, translated: &dyn Fn(&str) -> bool, changed: &mut bool) -> Vec<Value> {
    let Some(Value::Array(items)) = list else {
        return Vec::new();
    };
    items
        .iter()
        .map(|v| match rewrite(v, translated) {
            Some(new) => {
                *changed = true;
                new
            }
            None => v.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn none(_: &str) -> bool {
        false
    }

    /// The plain text of a component, as a reader would see it (keys shown bare).
    fn plain(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Array(list) => list.iter().map(plain).collect(),
            Value::Object(o) => {
                let mut s = String::new();
                if let Some(t) = o.get("text").and_then(Value::as_str) {
                    s.push_str(t);
                }
                if let Some(k) = o.get("translate").and_then(Value::as_str) {
                    s.push_str(k);
                }
                if let Some(Value::Array(with)) = o.get("with") {
                    s.extend(with.iter().map(|a| format!("({})", plain(a))));
                }
                if let Some(Value::Array(extra)) = o.get("extra") {
                    s.extend(extra.iter().map(plain));
                }
                s
            }
            other => other.to_string(),
        }
    }

    /// An invite as a Simple Voice Chat 2.6.27 server sent it (Earshot's Debug info log, PROTOCOL.md).
    fn captured_invite() -> Value {
        json!({"translate": "message.voicechat.invite", "with": [
            {"text": "muhnip"},
            {"color": "gray", "text": "e"},
            {"color": "green", "translate": "chat.square_brackets", "with": [{"extra": [{
                "click_event": {"action": "run_command", "command": "/voicechat join 7cc08ec3-3480-4739-8c01-b76bf237f0c6"},
                "hover_event": {"action": "show_text", "value": {"translate": "message.voicechat.accept_invite.hover"}},
                "translate": "message.voicechat.accept_invite"}], "text": ""}]}
        ]})
    }

    #[test]
    fn an_invite_gets_our_wording_and_keeps_its_arguments() {
        // chat.square_brackets is the game's own key; the test shows it bare.
        let out = rewrite(&captured_invite(), &|k: &str| k == "chat.square_brackets").expect("rewritten");
        assert_eq!(plain(&out), "muhnip invited you to the voice group e chat.square_brackets(Join)");
        assert!(out.get("translate").is_none() && out.get("with").is_none());
        let button = &out["extra"][4]["with"][0]["extra"][0];
        assert_eq!(button["click_event"]["command"], "/voicechat join 7cc08ec3-3480-4739-8c01-b76bf237f0c6");
        assert_eq!(plain(&button["hover_event"]["value"]), "Click to join this voice group");
        assert_eq!(out["extra"][4]["color"], "green", "the button keeps its style");
    }

    #[test]
    fn an_invite_without_a_group_name() {
        let line = json!({"translate": "message.voicechat.invite", "color": "gray", "with": ["AIOMarcus"]});
        let out = rewrite(&line, &none).expect("rewritten");
        assert_eq!(plain(&out), "AIOMarcus invited you to a voice group");
        assert_eq!(out["color"], "gray", "the line keeps its style");
    }

    #[test]
    fn keys_the_game_can_translate_or_with_a_fallback_stay() {
        let line = json!({"translate": "message.voicechat.invite", "with": ["A"]});
        assert!(rewrite(&line, &|k: &str| k == "message.voicechat.invite").is_none());
        let fallback = json!({"translate": "message.voicechat.invite", "fallback": "%s invites you"});
        assert!(rewrite(&fallback, &none).is_none());
        let other = json!({"translate": "chat.type.text", "with": ["A", "hi"]});
        assert!(rewrite(&other, &none).is_none());
        assert!(rewrite(&json!("plain text"), &none).is_none());
    }

    #[test]
    fn keys_inside_siblings_and_arguments_are_found() {
        let line = json!({"text": "[Server] ", "extra": [
            {"translate": "message.voicechat.group_full", "with": [{"translate": "message.voicechat.x"}]}
        ]});
        let out = rewrite(&line, &none).expect("rewritten");
        assert_eq!(plain(&out), "[Server] Voice chat: group full Voice chat: x");
        let list = json!([{"text": "a "}, {"translate": "message.voicechat.invite"}]);
        assert_eq!(plain(&rewrite(&list, &none).unwrap()), "a You were invited to a voice group");
    }

    #[test]
    fn lead_ins() {
        assert_eq!(lead_in("message.voicechat.invite"), "You were invited to a voice group");
        assert_eq!(lead_in("message.voicechat.some_new.thing"), "Voice chat: some new thing");
    }
}
