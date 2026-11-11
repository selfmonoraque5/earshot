<p align="center"><img src="icon.png" width="128" alt="Earshot icon"></p>

# Earshot (Horizon native mod)

Proximity voice chat for [Horizon](https://playhorizon.dev/), the Rust rewrite of Minecraft: Java Edition 26.3.
Talk to the players near you on servers that run **Simple Voice Chat**: with Java players using Simple Voice Chat,
and with other Horizon players using Earshot.

> Earshot is an independent project, **not affiliated with or endorsed by Simple Voice Chat** or its author.
> It contains no Simple Voice Chat code or assets. Its protocol support was worked out only from our own
> network captures; see [PROTOCOL.md](PROTOCOL.md), which records where each fact came from.

## Features

- **Proximity voice** with 3D audio: voices come from where players stand and fade with distance.
- **Push to talk** (Mouse 4) or **voice activation** with an adjustable threshold and a live mic level.
- **Voice groups**: create one (with an optional password), join one with a click, leave it. Group members hear each
  other at any distance, and a panel in the top-left corner lists them, lighting up whoever is talking.
- **Group invites** show in chat in Earshot's own words, with the server's join link still clickable.
- **Noise reduction**, **per-player volume**, **mute** and **deafen**.
- A **mic icon** on the HUD (idle, talking, muted, voice off) and a **speaker icon** beside talking players' name tags.
- Opus audio and AES-128-GCM encryption, as the server expects.

## Keys

| Key | Does |
|---|---|
| **V** | Voice menu: Main, Settings, Players and Groups screens |
| **Mouse 4** (side button) | Push to talk |
| **M** | Mute your microphone |
| **N** | Deafen |

Rebind them in Options > Mods > Earshot, which also has a **Debug info** switch for connection details.

## Works with

- Servers running **Simple Voice Chat 2.6.27** (compatibility version 20): Fabric and Paper (the Bukkit plugin).
- Tested in game on Windows and Linux (PipeWire): Horizon with Java players on Simple Voice Chat, Horizon with
  Horizon, and voice groups.
- A server without Simple Voice Chat: Earshot stays quiet and sends nothing.

## Network use

When you join a server that runs Simple Voice Chat, Earshot asks it for a voice key over the game connection.
It then sends and receives encrypted (AES-128-GCM) voice packets over **UDP** to the address the server names (default
port 24454). It contacts nothing else.

## Install

In the Horizon launcher: Mods, find **Earshot**, Install, then **Build**. To try a local copy, put the `.hnmod` in your
instance's `horizon-native-mods/` folder and Build. The first native build takes a while (the launcher downloads Rust).

## Compatibility

Earshot needs hook API **409** or later (`compatibility.json`). Last tested on Horizon Nightly `fdc87ae` with
horizon-mixins `5a64af9`.

Not yet supported: whispering, group types other than the default, and a reply to a wrong group password. Earshot can
also run a Simple Voice Chat-compatible voice server on a Horizon dedicated server (UDP port 24454); that part is tested
against Earshot's own client only.

## How it works

| Path | What |
|---|---|
| `mixins/earshot/horizon-mixin.toml` | The bundle: the game glue module, the core crate and the hooks |
| `mixins/earshot/runtime.rs` | Game glue, added to the game as `crate::game::earshot` |
| `mixins/earshot/helpers/` | One small function per hook |
| `mixins/earshot/core` | `earshot-core`, everything that doesn't need the game: the wire format, AES-128-GCM, Opus, jitter buffer, mixing, positional audio, the menu and the icons. All unit tests are here. |
| `mixins/earshot/core/src/opus` | opus-rs 0.1.41, vendored (see below) |
| `PROTOCOL.md` | What Earshot knows of the voice protocol, and the capture each fact came from |

```
cargo test -p earshot-core
```

`runtime.rs` and the helpers only compile inside the game, so `cargo test` checks the core crate; the game build checks
the rest.

## Dependencies and licenses

Shipped in the mod (Horizon only allows crates already in its own `Cargo.lock`):

| Dependency | Version | License | How it's used |
|---|---|---|---|
| [opus-rs](https://github.com/restsend/opus-rs) | 0.1.41 | BSD-3-Clause (© restsend.com; © Xiph.Org, Skype Limited, Octasic, Jean-Marc Valin, Timothy B. Terriberry, CSIRO, Gregory Maxwell, Mark Borgerding, Erik de Castro Lopo, Mozilla, Amazon) | Vendored source in `mixins/earshot/core/src/opus/`, license in `mixins/earshot/core/COPYING-opus-rs`; changed only its `crate::` paths |
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
| bitflags | 1.3.2 | MIT/Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| bumpalo | 3.20.3 | MIT OR Apache-2.0 |
| bytes | 1.12.1 | MIT |
| cesu8 | 1.1.0 | Apache-2.0/MIT |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| cipher | 0.4.4 | MIT OR Apache-2.0 |
| combine | 4.6.9 | MIT |
| coreaudio-rs | 0.13.0 | MIT/Apache-2.0 |
| cpal | 0.16.0 | Apache-2.0 |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 |
| dasp_sample | 0.11.0 | MIT OR Apache-2.0 |
| equivalent | 1.0.3 | Apache-2.0 OR MIT |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| generic-array | 0.14.7 | MIT |
| getrandom | 0.3.4 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| inout | 0.1.4 | MIT OR Apache-2.0 |
| itoa | 1.0.19 | MIT OR Apache-2.0 |
| jni | 0.21.1 | MIT/Apache-2.0 |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 |
| js-sys | 0.3.107 | MIT OR Apache-2.0 |
| libc | 0.2.191 | MIT OR Apache-2.0 |
| log | 0.4.35 | MIT OR Apache-2.0 |
| mach2 | 0.4.3 | BSD-2-Clause OR MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| ndk | 0.9.0 | MIT OR Apache-2.0 |
| ndk-context | 0.1.1 | MIT OR Apache-2.0 |
| ndk-sys | 0.6.0+11769913 | MIT OR Apache-2.0 |
| num_enum | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| num_enum_derive | 0.7.6 | BSD-3-Clause OR MIT OR Apache-2.0 |
| num-derive | 0.4.2 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| objc2 | 0.6.5 | MIT |
| objc2-audio-toolbox | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-audio-types | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-core-foundation | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-encode | 4.1.0 | MIT |
| objc2-foundation | 0.3.2 | MIT |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| proc-macro-crate | 3.5.0 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| r-efi | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 |
| slab | 0.4.12 | MIT |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.9 | MIT OR Apache-2.0 |
| thiserror | 1.0.69 | MIT OR Apache-2.0 |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 |
| tokio | 1.54.1 | MIT |
| toml_datetime | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_edit | 0.25.18+spec-1.1.0 | MIT OR Apache-2.0 |
| toml_parser | 1.1.5+spec-1.1.0 | MIT OR Apache-2.0 |
| typenum | 1.20.1 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| wasip2 | 1.0.1+wasi-0.2.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| wasm-bindgen | 0.2.130 | MIT OR Apache-2.0 |
| wasm-bindgen-futures | 0.4.80 | MIT OR Apache-2.0 |
| wasm-bindgen-macro | 0.2.130 | MIT OR Apache-2.0 |
| wasm-bindgen-macro-support | 0.2.130 | MIT OR Apache-2.0 |
| wasm-bindgen-shared | 0.2.130 | MIT OR Apache-2.0 |
| web-sys | 0.3.107 | MIT OR Apache-2.0 |
| windows | 0.54.0 | MIT OR Apache-2.0 |
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
| windows-core | 0.54.0 | MIT OR Apache-2.0 |
| windows-result | 0.1.2 | MIT OR Apache-2.0 |
| windows-sys | 0.45.0 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| windows-targets | 0.42.2 | MIT OR Apache-2.0 |
| windows-targets | 0.52.6 | MIT OR Apache-2.0 |
| winnow | 1.0.5 | MIT |
| wit-bindgen | 0.46.0 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| zmij | 1.0.23 | MIT |
</details>

## License

Earshot's own code is MIT ([LICENSE](LICENSE)). The vendored opus-rs keeps its BSD-3-Clause license.
