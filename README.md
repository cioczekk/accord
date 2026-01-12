# Accord Voice

Accord Voice is a peer-to-peer desktop voice calling app built in Rust. It uses:

- iroh for discovery and encrypted QUIC connections between peers.
- iroh-roq for RTP/Opus real-time audio transport over QUIC.
- cpal for cross-platform audio capture and playback.
- Iced for the GUI.

The app is designed to work without a central signaling server. Each instance can generate a shareable ticket that includes the information needed to connect to it.

## Workspace layout

- `voice-core`: networking, tickets, call state, and audio pipeline
- `voice-gui`: Iced application and user interface

## Build prerequisites

### Windows

- Install the Rust toolchain (https://rustup.rs).
- Install libopus development files. Recommended using vcpkg:
  - `vcpkg install opus:x64-windows`
  - Set `VCPKG_ROOT` and ensure `VCPKG_DEFAULT_TRIPLET=x64-windows` if needed.

### Linux (Debian/Ubuntu)

```
sudo apt update
sudo apt install -y pkg-config libasound2-dev libopus-dev
```

For other distributions, install the equivalent ALSA and libopus development packages.

## Build and run

From the workspace root:

```
cargo build -p voice-gui
cargo run -p voice-gui
```

For a release build:

```
cargo build -p voice-gui --release
```

## Usage

1. Launch the app on both machines.
2. On the first machine, copy the local ticket (or scan the QR code).
3. On the second machine, paste the ticket into the remote ticket field and click **Call**.
4. On the first machine, click **Accept** to start the call or **Reject** to dismiss it.
5. Use **Hang up** to end the call.
6. Use **Mute/Unmute** to stop sending microphone audio while staying connected.

## Configuration and identity

On first launch, the app generates an iroh identity key and stores it under a platform config directory:

- Windows: `%APPDATA%\Accord\` (example)
- Linux: `~/.config/Accord/`

This identity is reused on subsequent runs to keep the same node ID.

## Known limitations

- No acoustic echo cancellation or noise suppression yet.
- Audio device selection is automatic; there is no device picker UI.
- One active call at a time.
- No contact list or call history.
- No adaptive jitter buffer or congestion control tuning.

## Future improvements

- Echo cancellation and noise suppression.
- UI for choosing input/output devices.
- Multiple calls and call waiting.
- Contact management and favorites.
- Optional video support.
- More detailed network diagnostics.
