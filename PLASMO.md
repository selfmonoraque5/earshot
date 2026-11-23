# Plasmo Voice protocol: what Earshot's support is written from

Facts read from Plasmo Voice's source (plasmoapp/plasmo-voice, LGPL-3.0), commit 0d81b06 (2.2.0-beta.4); wire format checked unchanged against tag 2.1.17 (only refactors in `PacketUdpCodec`/`PacketUdp`; `PlayerIconConfig` changed). Earshot's code (`src/core/src/plasmo.rs`) is written from these notes, not translated from Plasmo Voice's Java,
and contains none of it.

Thanks to the Plasmo Voice authors (plasmoapp) for publishing their source under the LGPL-3.0. Earshot is not
affiliated with or endorsed by Plasmo Voice.

## Encoding (`proto/packets/PacketUtil.java`)
- Java DataOutput, big-endian. int = 4 bytes, long = 8, short = 2, bool = 1 byte.
- String = `writeUTF`: u16 byte length + Java modified UTF-8.
- UUID = most significant long, then least significant long (16 bytes).
- bytes = int length + bytes. UUID/int lists and generic lists: int count + items.
- nullable = bool present + value.

## Channels (`server/common/.../BaseVoiceServer.java`)
- `plasmo:voice/v2` — TCP-side packets over the game's plugin messages.
- `plasmo:voice/v2/installed` — flag channel. `plasmo:voice/v2/service` — proxy/service.

## TCP packets (`proto/packets/tcp/PacketTcpCodec.java`)
Payload = 1 byte type + fields. Types in order (C = clientbound, S = serverbound):
1 C Connection, 2 C PlayerInfoRequest, 3 C Config, 4 C ConfigPlayerInfo, 5 S LanguageRequest, 6 C Language,
7 C PlayerList, 8 C PlayerInfoUpdate, 9 C PlayerDisconnect, 10 S PlayerInfo, 11 S PlayerState,
12 S PlayerAudioEnd, 13 S PlayerActivationDistances, 14 C DistanceVisualize, 15 S SourceInfoRequest,
16 C SourceInfo, 17 C SelfSourceInfo, 18 C SourceAudioEnd, 19 C ActivationRegister, 20 C ActivationUnregister,
21 C SourceLineRegister, 22 C SourceLineUnregister, 23 C SourceLinePlayerAdd, 24 C SourceLinePlayerRemove,
25 C SourceLinePlayersList, 26 C AnimatedActionBar.

## Handshake (`server/common/.../connection/VoiceTcpServerConnectionManager.java`)
- Server -> Connection(secret UUID, ip String, port int). ip/port: public host if configured, else host, else the UDP server's port.
- Server -> PlayerInfoRequest (empty).
- Config: serverId UUID, CaptureInfo(sampleRate, mtuSize, CodecInfo("opus", {mode, bitrate})), EncryptionInfo("AES/CBC/PKCS5Padding", AES key RSA-encrypted with the client's public key, Java `Cipher.getInstance("RSA")` = RSA/ECB/PKCS1Padding), source lines, activations (permission-filtered), permissions, PlayerIconConfig.
- => Earshot needs an RSA key pair (send public key in PlayerInfo) and RSA PKCS#1 v1.5 decryption of the AES key.

## When the handshake starts (`server/common/.../connection/PlayerInfoRequestScheduler.kt`, `socket/NettyPacketHandler.java`)
- On join the server sends PlayerInfoRequest, and resends it on a backoff until it has the player's public key.
- PlayerInfo (S, type 10; `tcp/serverbound/PlayerInfoPacket.java`, extends PlayerState): voiceDisabled bool, microphoneMuted bool, minecraftVersion UTF (<=64), version UTF (<=64), publicKey int len (1..2048) + bytes. Public key = X.509 SubjectPublicKeyInfo DER of a 2048-bit RSA key (client: `KeyPairGenerator.getInstance("RSA").initialize(2048)` in `client/.../connection/ModServerConnection.java`).
- Server checks `version`: same major as the server's own version, and >= config `clientModMinVersion` (default "2.0.0"); else it tells the player to update and stops. => Earshot must send a 2.x version string.
- Then server -> Connection(secret, ip, port).
- Client -> UDP Ping carrying the secret. The first UDP packet with a known secret creates the UDP connection; the server then sends Config and PlayerList over TCP and broadcasts PlayerInfoUpdate.
- Server keepalive (`socket/NettyUdpKeepAlive.java`): every 100 ms checks connections; drops one silent longer than `keepAliveTimeoutMs` (then re-requests player info); sends a Ping every 1.5–3 s.
- PlayerState (S, type 11): voiceDisabled bool, microphoneMuted bool.

## UDP (`proto/packets/udp/PacketUdpCodec.java`)
- Datagram = int magic 0x4e9004e9, byte type, secret UUID, long timestamp (ms), packet fields.
- Types: 1 Ping (any), 2 PlayerAudio (S), 3 SourceAudio (C), 4 SelfAudioInfo (C), 0x100 Custom (any).
- Ping: long time, then optional UTF serverIp (<=255) + unsigned short port (only written when known).
- BaseAudio: long sequenceNumber, int len (1..2048) + data.
- PlayerAudio (S) = BaseAudio + activationId UUID + short distance + bool stereo.
- SourceAudio (C) = BaseAudio + sourceId UUID + byte sourceState + short distance.
- SelfAudioInfo (C): sourceId UUID, long sequenceNumber, bool hasData [+ int len + data], short distance.

## Encryption (`common/.../encryption/aes/AesEncryption.java`, `client/.../capture/VoiceAudioCapture.java`)
- Audio data = Opus frame, then AES/CBC/PKCS5 (= PKCS#7) with a random 16-byte IV: wire data = IV (16) + ciphertext. Decrypt: first 16 bytes IV, rest ciphertext.
- Applied to every audio packet's `data` (sending: encode, then encrypt; when there's no encryption info, plain Opus).
- Frames are 20 ms: `sampleRate / 1000 * 20` samples (`client/.../connection/VoiceServerInfo.java`).
- Sequence numbers count per activation, from 1 (`VoiceAudioCapture.getSequenceNumber`).
- When talking stops: TCP PlayerAudioEnd (S, type 12) with sequenceNumber long, activationId UUID, distance short.

## Config (C, type 3; `tcp/clientbound/ConfigPacket.java`)
serverId UUID; CaptureInfo; bool hasEncryption [EncryptionInfo]; int n (0..127) VoiceSourceLine; int n (0..127) VoiceActivation; then ConfigPlayerInfo's permissions (int n (0..127) of (UTF key, bool)); then PlayerIconConfig (since 2.1.7; older servers omit it, so read it only if bytes remain).
- CaptureInfo: int sampleRate, int mtuSize, bool [CodecInfo].
- CodecInfo: UTF name, int n (0..128) of (UTF key, UTF value). Opus params seen: "mode", "bitrate".
- EncryptionInfo: UTF algorithm, int len (1..2048) + bytes (RSA-encrypted AES key).
- VoiceActivation: UTF name, UTF translation, UTF icon, int-list distances (<=64), int defaultDistance, bool proximity, bool transitive, bool stereoSupported, bool [CodecInfo], int weight. id = UUID.nameUUIDFromBytes((name + "_activation") UTF-8), i.e. UUID v3 (MD5). Proximity's name is "proximity".
- VoiceSourceLine: UTF name, UTF translation, UTF icon, double defaultVolume, int weight, bool [int n (0..32767) GameProfile]. id = UUID v3 of name + "_line".
- GameProfile: UUID, UTF name, int n (0..100) of (UTF name, UTF value, UTF signature).
- PlayerIconConfig: int n of UTF visibility names, Pos3d offset.
- ConfigPlayerInfo (C, type 4): the permissions map alone (later updates).

## Sources (other players' voices)
- SourceInfo (`data/audio/source/SourceInfo.java`): UTF type name (PLAYER | ENTITY | STATIC | DIRECT), then: UTF addonId, UUID id, nullable UTF name, byte state, bool [CodecInfo decoder], bool stereo, UUID lineId, bool iconVisible, int angle; then per type:
  - PLAYER: VoicePlayerInfo (UUID playerId, UTF nick, bool muted, bool voiceDisabled, bool microphoneMuted).
  - ENTITY: int entityId. STATIC: Pos3d position, Pos3d lookAngle (Pos3d = 3 doubles).
  - DIRECT: bool [GameProfile sender], bool [Pos3d relativePosition], bool cameraRelative.
- SourceInfo (C, 16) carries one SourceInfo. SourceInfoRequest (S, 15): UUID sourceId. SourceAudioEnd (C, 18): UUID sourceId, long sequenceNumber.
- Client (`client/.../socket/NettyUdpClientHandler.java`, `client/.../source/VoiceClientSourceManager.kt`): SourceAudio for an unknown source is buffered and a SourceInfoRequest sent; for a known one whose `state` byte differs, the info is requested again.
- PlayerList (C, 7): int n (0..32767) VoicePlayerInfo. PlayerInfoUpdate (C, 8): one VoicePlayerInfo (presumably). PlayerDisconnect (C, 9): UUID playerId.

## Keepalive / timeouts
- Client answers every server Ping with a Ping. Until connected it sends Ping(serverIp, serverPort) every tick of its UDP handler.
- Client timeouts: soft 7 s, hard 30 s (`NettyUdpClientHandler`). Server default keepAliveTimeoutMs 15 000.
- Connection ip "0.0.0.0" = use the game server's address (`ModServerConnection.handle(ConnectionPacket)`).

## Server defaults (`server/common/.../config/VoiceServerConfig.java`, `BaseVoiceServer.java`)
- sampleRate 48 000 (allowed 8 000, 12 000, 24 000, 48 000); mtuSize 1024 (128..5000); AES key = the 16 bytes of a random UUID (AES-128).
