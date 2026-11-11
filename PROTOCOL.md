# SVC wire protocol: what we've observed

Target: Simple Voice Chat **2.6.27** (Fabric, Minecraft 26.3), compatibility version **20** (from the server log).
Every fact below comes from our own captures (`captures/*.jsonl`, recorded by `tools/capture-relay`),
SVC's server log, or its generated config. None of it comes from SVC's source.
**Status** is *confirmed* (checked on every packet in every capture) or *hypothesis* (fits, not yet proven).

## 1. Plugin channels (over the Minecraft connection)

Channel names seen (captures 01–04): `voicechat:` + `request_secret`, `secret`, `update_state`, `state`, `states`,
`remove_state`, `add_group`, `remove_group`, `joined_group`, `set_group`, `create_group`, `leave_group`,
`add_category`, `remove_category`. All of them appear at join, when the two sides announce which channels they support.

Integers are big-endian. Strings are a VarInt length followed by UTF-8.

### `voicechat:request_secret` (client -> server), *confirmed*
| Field | Type | Seen |
|---|---|---|
| compatibility version | i32 | 20 |

### `voicechat:secret` (server -> client), *confirmed (field meanings matched to config values)*
| Field | Type | Seen | Source of meaning |
|---|---|---|---|
| secret | 16 bytes | random per session | is the AES key (section 2) |
| server port | i32 | 24454 | config `port` |
| player UUID | 16 bytes | offline UUID of NullPort404 | calculated independently |
| codec | u8 | 0 | config `codec=VOIP` (hypothesis: 0 VOIP, 1 AUDIO, 2 RESTRICTED_LOWDELAY) |
| MTU | i32 | 1275 | config `mtu_size` |
| voice distance | f64 | 48.0 | config `max_voice_distance` |
| keep-alive ms | i32 | 1000 | config `keep_alive` |
| groups enabled | bool | true | config `enable_groups` |
| voice host | string | `<relay address>:24455` (the capture relay's address) | config `voice_host` |
| recording allowed | bool | true | config `allow_recording` |

The total is 92 bytes including the packet ID and channel name, which leaves no unaccounted bytes.

### Player state entry (used by `state` and `states`), *layout confirmed; "disabled" confirmed, "disconnected" hypothesis* (capture 04)
| Field | Type | Seen |
|---|---|---|
| disabled | bool | 1 after `mark disable` (476.29 s), 0 after `mark enable` (480.29 s) |
| disconnected? | bool | 1 at join (0.50 s), 0 once the UDP connection was validated (1.61 s) |
| player UUID | 16 bytes | |
| player name | string | `NullPort404` |
| group present? | bool | 0 (no group; the group fields that follow when it's 1 are TBD) |

- `voicechat:state` (server -> client): one entry. It's sent when a player's state changes, including your own.
- `voicechat:states` (server -> client): i32 count, then that many entries. Sent once after `secret`.
- `voicechat:update_state` (client -> server): one bool, **disabled**, *confirmed*. `00` is sent right after connecting.
  `01` was sent about 1 s after `mark disable` and `00` about 1 s after `mark enable`. Each time, the server answered with a `state` carrying the same flag.
  Keep-alives (UDP 0x08) carry on while disabled.

### Mute, *confirmed* (capture 04)
Muting sends **no** plugin message and no UDP message. Between the `mark mute down` (13.5 s) and `mark mute up` (20.7 s) markers nothing changes: the client simply stops sending mic packets. Keep-alives continue.

Still to do: the group messages.

## 2. UDP voice packets

### Outer layout, *confirmed* (all packets, captures 01–04)
- Client -> server: `0xff`, player UUID (16 B), VarInt n, n encrypted bytes
- Server -> client: `0xff`, VarInt n, n encrypted bytes

### Encryption, *confirmed* (1102/1102 packets decrypt across captures 01, 03 and 04)
AES-128-GCM. The key is the `secret` from section 1. The encrypted bytes are a 12-byte nonce, the ciphertext, then a 16-byte tag. No additional authenticated data.
How the nonce is chosen: TBD (it looks random).

### Message types (first plaintext byte)
| Type | Dir | Body | Meaning | Status |
|---|---|---|---|---|
| 0x01 | c2s | VarInt n, n bytes Opus, u64 sequence, u8 flag | mic audio | confirmed layout; flag was always 0 (whisper? TBD) |
| 0x02 | s2c | see "Audio from other players" below | another player's voice | confirmed (capture 05) |
| 0x05 | c2s | player UUID (16), secret (16) | authenticate | hypothesis (server log: "Successfully authenticated") |
| 0x06 | s2c | none | authenticate ack | hypothesis |
| 0x08 | both | none | keep-alive, about every 1000 ms | confirmed timing |
| 0x09 | c2s | none | connection check | hypothesis (log: "validated connection") |
| 0x0a | s2c | none | connection check ack | hypothesis |

Order at join: 05 -> 06 -> 09 -> 0a -> then 08 both ways every second.

### Audio, *confirmed*
Opus, 48 kHz mono, 20 ms frames (960 samples), application VOIP. The first observed TOC byte was `0x78`: hybrid, fullband, 20 ms.
The sequence increases by 1 per frame. Frames are only sent while push-to-talk is held.
Decoded with opus-rs, giving `captures/decoded/*.wav`. The decoded capture 03 was confirmed by ear: it's exactly what was said.

### Audio from other players (s2c 0x02), *confirmed* (capture 05: NullPort404 + AIOMarcus)
| Field | Type | Seen |
|---|---|---|
| type | u8 | 0x02 |
| channel id? | 16 bytes | the speaker's UUID |
| sender | 16 bytes | the speaker's UUID |
| audio | VarInt n, n bytes Opus | **byte-identical** to the speaker's c2s 0x01 data (the server does not re-encode) |
| sequence | u64 | the speaker's own sequence number |
| distance | f32 | 48.0 = config `max_voice_distance` |
| flag | u8 | 0 (whisper? TBD) |

Hypothesis: the first UUID is the audio "channel" and differs from the sender in groups. Needs a group capture to check.
The packet carries **no position**: the client has to use the speaker entity's position from the game world.

### Distance is filtered by the server, *confirmed* (capture 05)
- Near each other: NullPort404 sent 184 mic frames and AIOMarcus received all 184 as 0x02. AIOMarcus sent 377 and NullPort404 received all 377.
- After `mark far` (AIOMarcus walked about 60 blocks away): AIOMarcus sent 169 frames and **0** reached NullPort404.

So the server decides who hears whom; the client only applies volume falloff and panning.
Each 0x02 is encrypted with the **receiver's** key (each player had their own secret, and every packet decrypted with its receiver's key).

### Paper (SVC Bukkit plugin 2.6.27), *observed in game 2026-11-05*
Same compatibility version (20), and the plugin-channel handshake works unchanged.
With Earshot's original strict decoding, the server's replies to `Authenticate` (15 received, server log "Successfully authenticated") weren't accepted, so the login timed out.
Once 0x06/0x08/0x09/0x0a were made to ignore anything after the type byte, the connection completed.
Hypothesis: on Paper those replies carry extra bytes after the type. Their exact content wasn't recorded; a capture with Earshot's diagnostics would show it.

Two Earshot players on the same Paper server then connected but heard nothing, in either direction. Both were fully "connected to voice chat" per the server log, standing on the same block.
Once *every* UDP message was decoded leniently (known fields read, trailing bytes ignored), they heard each other.
Conclusion (*confirmed by the fix, not by a capture*): this server's voice packets (0x02) also carry bytes after the known fields.
Rule: **missing fields are errors; extra bytes are ignored.**

## 4. Groups, *confirmed* (capture 06, 2026-11-10: NullPort404 and AIOMarcus, both on Java SVC 2.6.27)

### Plugin messages
| Channel | Dir | Data | Status |
|---|---|---|---|
| `create_group` | c2s | name (string), has-password (bool), [password (string)], then `00 00` | confirmed (2 groups; hypothesis: the 2 bytes are the group type) |
| `set_group` | c2s | group id (16), has-password (bool), [password (string)] | **confirmed** at 518.57 s (it had been a hypothesis) |
| `leave_group` | c2s | nothing | confirmed |
| `add_group` | s2c, to everyone | group id (16), name, has-password (bool), 4 bytes, zero every time | confirmed |
| `remove_group` | s2c, to everyone | group id (16) | confirmed (when its last member left) |
| `joined_group` | s2c, to the joiner | in-a-group (bool), [group id], then a bool, 0 every time | confirmed layout; hypothesis: the bool is "wrong password" (no wrong attempt was captured) |
| `remove_state` | s2c | player UUID (16) | confirmed (a player left the server) |

The player state entry's last field is "in a group" (bool), **followed by the group's id when true** (the 47-byte states at 191.70 s).

### Group voice, UDP type 0x03 (s2c), *confirmed* on all 1061 packets
`0x03`, channel UUID (16), sender UUID (16), VarInt n + n bytes Opus, u64 sequence, u8 flag (0).
The channel was the sender in all 1061 packets. There's **no distance field**, unlike 0x02.

- Group members hear each other at **any distance**: after `mark far` (598.5 s, about 60 blocks) frames still went both ways (599–608 s).
- A nearby player who is **not** in the group hears **nothing** of group talk (193.5–197.4 s and 216.5–217.8 s: 0 frames to AIOMarcus while 4 blocks away).
- Sequence numbers keep counting across talk spurts (0…809 for one client).

### Group invite chat line, *confirmed* (Earshot's Debug info log, 2026-11-10, invited by muhnip to group "e")
A system chat line, `{"translate": "message.voicechat.invite", "with": [A, B, C]}` with no `fallback`:
- A: the inviter's name, `{"text": "muhnip"}`.
- B: the group name, gray, `{"color": "gray", "text": "e"}`.
- C: green, `chat.square_brackets` around `{"translate": "message.voicechat.accept_invite"}`, which carries
  `click_event` `run_command` `/voicechat join <group id>` and `hover_event` `show_text` `{"translate": "message.voicechat.accept_invite.hover"}`.

Only Simple Voice Chat's own client has text for the three `message.voicechat.` keys; Earshot words them itself (`core/src/chat.rs`).

## 5. Still unknown
- Group types (the 2 bytes in `create_group`, 4 in `add_group`), and wrong-password replies.
- The flag bytes at the end of c2s 0x01 and s2c 0x02 (whisper? both always 0 so far).
- Whether 0x05 must be repeated until a 0x06 arrives. AIOMarcus sent 0x05 twice in capture 05 and got 0x06 twice.
- How the nonce is chosen (it looks random; a receiver doesn't need to know).
