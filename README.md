<p align="center"><img src="icon.png" width="128" alt="Earshot icon"></p>

# Earshot (Horizon mod)

Proximity voice chat for [Horizon](https://playhorizon.dev/), the Rust rewrite of Minecraft: Java Edition 26.3.
Talk to the players near you on servers that run **Simple Voice Chat** or **Plasmo Voice**: with Java players using
those mods, and with other Horizon players using Earshot.

> Earshot is an independent project, **not affiliated with or endorsed by Simple Voice Chat** or its author.
> It contains no Simple Voice Chat code or assets. Its protocol support was worked out only from our own
> network captures; see [PROTOCOL.md](PROTOCOL.md), which records where each fact came from.
>
> Earshot is also **not affiliated with or endorsed by Plasmo Voice** or its authors. Its Plasmo Voice support is
> Earshot's own code, written from protocol facts read in Plasmo Voice's source (LGPL-3.0); see [PLASMO.md](PLASMO.md),
> which credits it and names the file each fact came from.

## Screenshots

| Voice menu (V) | Settings | Voice groups |
|---|---|---|
| ![The voice menu: settings, groups, players, and mic, sound and voice switches](screenshots/voice-menu.png) | ![Settings: volumes, push to talk, mic test and level, noise reduction, devices](screenshots/settings.png) | ![Creating a voice group with a name, an optional password and a type](screenshots/create-group.png) |

## Features

- **Proximity voice** with 3D audio: voices come from where players stand and fade with distance.
- **Push to talk** (Mouse 4), held or pressed once to switch on and off, or **voice activation** with an adjustable
  threshold and a live mic level.
- A **mic test** in the settings: hear yourself as the others would.
- **Whisper** (Mouse 5, held): talk so only players close by hear you (the server sets how close).
- **Voice groups**: create one (normal, open or isolated, with an optional password), join one with a click, leave it. Group members hear each
  other at any distance, and a panel in the top-left corner lists them, lighting up whoever is talking. A **group volume**,
  and a short **chime** when someone joins or leaves your group (Earshot's own sound; it can be turned off).
- **Group invites** show in chat in Earshot's own words, with the server's join link still clickable.
- **Noise reduction**, **per-player volume**, **mute** and **deafen**.
- A **mic icon** on the HUD (idle, talking, muted, voice off), and beside other players' name tags a **speaker icon**:
  green while they talk, red if they turned voice chat off, grey and crossed out if they aren't connected to voice
  (no voice chat mod) on a server that runs it.
- Opus audio, encrypted as the server expects: AES-128-GCM for Simple Voice Chat, AES-128-CBC (with an RSA key exchange)
  for Plasmo Voice.
- On a **Plasmo Voice** server: proximity voice, push to talk, voice activation, mute, deafen, per-player volume and the
  icons. Plasmo Voice's own groups and addons aren't supported, and the server must use 48 kHz audio (its default).

## Keys

| Key | Does |
|---|---|
| **V** | Voice menu: Main, Settings, Players and Groups screens |
| **Mouse 4** (side button) | Push to talk (hold, or press to switch: Settings in the voice menu) |
| **Mouse 5** (other side button) | Whisper, while held |
| **M** | Mute your microphone |
| **N** | Deafen |

Rebind them in Options > Controls. Options > Mods > Earshot > **Config** has a **Voice chat** switch and a **Debug info**
switch for connection details.

## Works with

- Servers running **Simple Voice Chat 2.6.27** (compatibility version 20): Fabric and Paper (the Bukkit plugin).
- 2.0 tested in game on Windows with Simple Voice Chat servers. Earshot 1.x was also tested on Linux (PipeWire),
  but 2.0 doesn't build on Linux yet (see Compatibility).
- Servers running **Plasmo Voice 2.x** (tested against its protocol as of 2.1.17 and 2.2.0 beta; not yet in game).
- A server without either: Earshot stays quiet and sends nothing.

## Network use

When you join a server that runs Simple Voice Chat, Earshot asks it for a voice key over the game connection.
It uses **UDP** for encrypted (AES-128-GCM) voice packets, to the address the server names (default port 24454). It
contacts nothing else.

On a server that runs Plasmo Voice, Earshot registers Plasmo Voice's channel, sends the server an RSA public key (made
on your computer each time the game starts) and gets back the voice address and an AES audio key sealed to that key.
Voice packets then go over **UDP**, AES-128-CBC encrypted, to the address the server names (default port 60606).

Earshot opens the microphone when voice chat is enabled, the mic is unmuted, and either the game server supplies
voice connection details or you start the mic test. Muting closes the microphone while keeping playback available
in a voice session. Switching **Voice chat** off closes both audio streams. The mic test is unavailable while muted
or switched off.

Earshot sends microphone audio only over a connected voice session when voice chat is enabled, the mic is unmuted,
and push-to-talk, whisper, or voice activation is active. The mic test plays back locally and does not start
transmission on its own.

**This version does not host voice on Horizon dedicated servers.** It can still join servers running Simple Voice Chat.
Earshot 1.x could host voice on a dedicated server and listened on UDP port 24454 on all interfaces by default.

## Install

Made for Horizon **Nightly 2026-11-22 · f1bb66f**.
It uses only the mod API, so the launcher also builds it on later Horizon builds where nothing it uses
changed.

From GitHub:

1. Download the `earshot-….hmod` file from the [latest release](https://github.com/selfmonoraque5/earshot/releases/latest).
2. Put it in the `horizon-native-mods` folder of your game folder, not in `mods`. On Windows that is
   `%APPDATA%\horizon\horizon-native-mods`.
3. In the launcher, open the Mods page and click **Build** (the first build downloads the mod SDK and takes a few
   minutes). After a build the launcher can look stuck for a while as it cleans its cache: let it finish.
4. When it says **Up to date**, play.

From the Horizon registry, open **Mods**, find **Earshot**, and choose **Install**.

To build a local copy: `horizon-launcher native-build <this folder> --instance <instance> --trust-native-code`.

Earshot 2.0 is made for Horizon's mod API. Its menu settings and player volumes are kept in `client.txt` in the
game's `horizon-mod-data/earshot/` folder, so they start from the defaults again once: the 1.x settings files aren't
read.

## Compatibility

A native mod on Horizon's mod API (`horizon-mod.toml`, `[native]`). Everything on the client goes through the mod
API's hooks: the server's packets (`client_packet`), chat lines (`client_chat_received`), the HUD
(`render_gui_post`), the name tag icons (`render_player_post`), the voice menu (a mod screen and the screen hooks),
the keys and the settings. It patches no game function, so the launcher also builds it on later Horizon builds
where nothing it uses changed.

**Tested on Windows.** macOS should work but is untested. On Linux the launcher refuses it: Earshot's audio library (cpal) uses ALSA there,
which the game links too, and a mod may not link a second copy. A Linux version is being worked on.

The voice server for Horizon dedicated servers needs a hook the mod API doesn't have yet (a player's custom payloads
on the server); it used a game patch that stopped compiling after Horizon's gameplay split
([horizon-rust/horizon#18462](https://github.com/horizon-rust/horizon/issues/18462), since fixed in the launcher), so it
is off in this version and comes back once that patch is tested again.

## How it works

| Path | What |
|---|---|
| `horizon-mod.toml` | Mod manifest and Horizon build requirements |
| `src/lib.rs` | The hooks |
| `src/client.rs` | The voice client between hooks: keys, positions, the HUD, the name tag icons, the voice menu and its settings |
| `src/server.rs` | Dedicated-server voice code and settings. Hosting is disabled in this release. |
| `src/text_json.rs` | Chat lines between the mod API's text and the JSON `earshot_core::chat` rewrites |
| `src/core` | `earshot-core`, everything that doesn't need the game: the wire format, AES-128-GCM, Opus, jitter buffer, mixing, positional audio, the voice server, the menu and the icons |
| `src/core/src/opus` | opus-rs 0.1.41, vendored (see below) |
| `PROTOCOL.md` | What Earshot knows of the voice protocol, and the capture each fact came from |

```
cargo test --workspace
```

## Dependencies and licenses

Shipped in the mod (Horizon only allows crates already in its own `Cargo.lock`):

| Dependency | Version | License | How it's used |
|---|---|---|---|
| [opus-rs](https://github.com/restsend/opus-rs) | 0.1.41 | BSD-3-Clause (© restsend.com; © Xiph.Org, Skype Limited, Octasic, Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark Borgerding, Erik de Castro Lopo, Mozilla, Amazon) | Vendored source in `src/core/src/opus/`, license in `COPYING-opus-rs`; adapted module paths and attributes, enabled the upstream default `std` and `heap` code, and removed the packet-loss audio trace |
| [aes](https://crates.io/crates/aes) | 0.8.4 | MIT OR Apache-2.0 | AES block cipher; GCM is our own code on top of it |
| [getrandom](https://crates.io/crates/getrandom) | 0.3.4 | MIT OR Apache-2.0 | Random nonces |
| [cpal](https://crates.io/crates/cpal) | 0.16.0 | Apache-2.0 | Microphone and speakers |
| [serde_json](https://crates.io/crates/serde_json) | 1.0 | MIT OR Apache-2.0 | Reading chat lines' JSON (group invites) |
| cpal's platform backends (windows, alsa, coreaudio/objc2, jni/ndk, wasm-bindgen…) | | MIT, Apache-2.0, BSD, Zlib, Unlicense, Unicode-3.0 | Transitive; every crate listed below |
| cipher 0.4.4, crypto-common 0.1.7, inout 0.1.4, cpufeatures 0.2.17, cfg-if 1.0.5, typenum 1.20.1 | | MIT OR Apache-2.0 | Transitive, via aes / getrandom |
| serde, serde_core, itoa, zmij | | MIT OR Apache-2.0 (zmij: MIT) | Transitive, via serde_json |
| generic-array 0.14.7 | | MIT | Transitive, via aes |
| version_check 0.9.5 | | MIT OR Apache-2.0 | Build-time only (generic-array) |
| libc, r-efi, wasip2, wit-bindgen | | MIT OR Apache-2.0 (each also offers others) | Transitive via getrandom, other platforms only (Unix, UEFI, WASI) |

<details>
<summary>Every crate the mod can compile with, on any platform</summary>

From `cargo tree -p earshot-core -e normal --target all` and `cargo metadata`, in this repository's own build. All are
permissive (MIT, Apache-2.0, BSD-2/3-Clause, Zlib, Unlicense or Unicode-3.0); where a crate offers a choice, Earshot uses
it under MIT or Apache-2.0. When Horizon builds the mod, it uses the versions in Horizon's own `Cargo.lock`.

| Crate | Version | License |
|---|---|---|
| aes | 0.8.4 | MIT OR Apache-2.0 |
| alsa | 0.9.1 | Apache-2.0/MIT |
| alsa-sys | 0.3.1 | MIT |
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| bitflags | 1.3.2 | MIT/Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 |
| bytes | 1.12.1 | MIT |
| cesu8 | 1.1.0 | Apache-2.0/MIT |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| cipher | 0.4.4 | MIT OR Apache-2.0 |
| combine | 4.6.8 | MIT |
| coreaudio-rs | 0.13.0 | MIT/Apache-2.0 |
| cpal | 0.16.0 | Apache-2.0 |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 |
| dasp_sample | 0.11.0 | MIT OR Apache-2.0 |
| dispatch2 | 0.3.1 | Zlib OR Apache-2.0 OR MIT |
| equivalent | 1.0.2 | Apache-2.0 OR MIT |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| generic-array | 0.14.7 | MIT |
| getrandom | 0.3.4 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| inout | 0.1.4 | MIT OR Apache-2.0 |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| jni | 0.21.1 | MIT/Apache-2.0 |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 |
| js-sys | 0.3.106 | MIT OR Apache-2.0 |
| libc | 0.2.190 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| mach2 | 0.4.3 | BSD-2-Clause OR MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| ndk | 0.9.0 | MIT OR Apache-2.0 |
| ndk-context | 0.1.1 | MIT OR Apache-2.0 |
| ndk-sys | 0.6.0+11769913 | MIT OR Apache-2.0 |
| num-derive | 0.4.2 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| num_enum | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| num_enum_derive | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| objc2 | 0.6.5 | MIT |
| objc2-audio-toolbox | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio-types | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-encode | 4.1.0 | MIT |
| objc2-foundation | 0.3.2 | MIT |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| pkg-config | 0.3.34 | MIT OR Apache-2.0 |
| proc-macro-crate | 3.5.0 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| same-file | 1.0.6 | Unlicense/MIT |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| slab | 0.4.12 | MIT |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.6 | MIT OR Apache-2.0 |
| thiserror | 1.0.69 | MIT OR Apache-2.0 |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 |
| tokio | 1.53.3 | MIT |
| toml_datetime | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_edit | 0.25.15+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_parser | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 |
| typenum | 1.20.1 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| version_check | 0.9.5 | MIT/Apache-2.0 |
| walkdir | 2.5.0 | Unlicense/MIT |
| wasip2 | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasm-bindgen | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-futures | 0.4.79 | MIT OR Apache-2.0 |
| wasm-bindgen-macro | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-macro-support | 0.2.129 | MIT OR Apache-2.0 |
| wasm-bindgen-shared | 0.2.129 | MIT OR Apache-2.0 |
| web-sys | 0.3.106 | MIT OR Apache-2.0 |
| winapi-util | 0.1.11 | Unlicense OR MIT |
| windows | 0.54.0 | MIT OR Apache-2.0 |
| windows-core | 0.54.0 | MIT OR Apache-2.0 |
| windows-result | 0.1.2 | MIT OR Apache-2.0 |
| windows-sys | 0.45.0 | MIT OR Apache-2.0 |
| windows-sys | 0.59.0 | MIT OR Apache-2.0 |
| windows-targets | 0.42.2 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.42.2 | MIT OR Apache-2.0 |
| windows_aarch64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.42.2 | MIT OR Apache-2.0 |
| windows_aarch64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.42.2 | MIT OR Apache-2.0 |
| windows_i686_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.42.2 | MIT OR Apache-2.0 |
| windows_i686_msvc | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.42.2 | MIT OR Apache-2.0 |
| windows_x86_64_gnu | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.42.2 | MIT OR Apache-2.0 |
| windows_x86_64_gnullvm | 0.52.6 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.42.2 | MIT OR Apache-2.0 |
| windows_x86_64_msvc | 0.52.6 | MIT OR Apache-2.0 |
| winnow | 1.0.4 | MIT |
| wit-bindgen | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| zmij | 1.0.23 | MIT |

</details>

## License

Earshot's own code is MIT ([LICENSE](LICENSE)). The vendored opus-rs keeps its BSD-3-Clause license.
