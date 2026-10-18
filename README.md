# Android Virtual Display

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org)
[![Host: Windows](https://img.shields.io/badge/host-Windows-0078D6.svg)](#building-and-running)
[![Client: Android 12+](https://img.shields.io/badge/client-Android%2012%2B-3DDC84.svg)](#building-and-running)
[![Last commit](https://img.shields.io/github/last-commit/fahrulseptiana/android-virtual-display.svg)](https://github.com/fahrulseptiana/android-virtual-display/commits/main)

Use an Android tablet as a wireless secondary display for a Windows PC.

Two apps, one shared codebase:

| App | Platform | Crate | What it does |
|-----|----------|-------|--------------|
| **Host** | Windows | `crates/vd-host` | Runs on the PC: announces itself on the LAN, captures a display, encodes and streams it, and switches display when asked. |
| **Client** | Android | `crates/vd-client` | Runs on the tablet: discovers hosts, decodes and renders the stream, and chooses which display to watch. |

Both depend on `crates/vd-core`, which holds everything that is not
platform specific: the wire protocol and the discovery state machines.

> **Status: mirror or extend, 720p30, hardware on both ends.** The client
> discovers a host, pairs, opens an encrypted session and displays the live
> Windows desktop — DXGI capture (with the mouse cursor) → AMD hardware H.264
> encode → MediaCodec hardware decode — sustained at the host's 30 fps target.
>
> With a virtual display installed the tablet can be a **second monitor** rather
> than a copy of an existing one: the client offers *mirror* or *extend*, and the
> host switches between them without dropping the session. Extending onto a
> monitor created at the tablet's size is also what makes the 4K downscale
> unnecessary — once the stream size follows it (Phase 7 step 4), scaling costs
> 0 ms instead of 17 ms.
>
> The UI is a searching ripple, a tappable host list and a full-screen desktop
> with an overlay that a tap hides. Remaining work is input forwarding and
> authenticated first pairing. See PLAN.md.

Package id: `id.fahrul.vd` · min SDK 31 (Android 12) · target SDK 36.

---

## Repository layout

```
.
├── Cargo.toml                     # cargo workspace
├── crates
│   ├── vd-core                    # shared logic, platform agnostic
│   │   └── src
│   │       ├── protocol.rs        # HostInfo, DiscoveryMessage, Envelope (JSON over UDP)
│   │       ├── net.rs             # PacketIo trait + UdpPacketIo implementation
│   │       ├── clock.rs           # one monotonic clock, shared for timestamps
│   │       ├── latency.rs         # clock offset (NTP-style) + measurement stats
│   │       ├── display.rs         # DisplayInfo/DisplayKind: what a host could show
│   │       ├── discovery          # browser.rs (client) + responder.rs (host)
│   │       ├── session            # control channel
│   │       │   ├── mod.rs         # SessionMessage + capability types
│   │       │   ├── handshake.rs   # Client/HostHandshake state machines
│   │       │   ├── pairing.rs     # X25519 exchange, PIN, key confirmation
│   │       │   ├── cipher.rs      # ChaCha20-Poly1305 session cipher
│   │       │   ├── channel.rs     # plaintext/encrypted message transport
│   │       │   ├── frame.rs       # length-prefixed framing
│   │       │   └── heartbeat.rs   # liveness tracking
│   │       ├── video              # FrameHeader / VideoFrame wire format
│   │       ├── capture            # FrameSource trait + synthetic test pattern
│   │       └── codec.rs           # JPEG, downscaling, RGBA→NV12
│   ├── vd-host                    # Windows host
│   │   └── src
│   │       ├── main.rs            # wiring, settings, diagnostics
│   │       ├── gpu.rs             # the shared D3D11 device
│   │       ├── dxgi.rs            # output enumeration, Desktop Duplication, cursor
│   │       ├── displays.rs        # outputs described for a client, and classified
│   │       ├── encoder            # Media Foundation h264 (hardware), controls, selection
│   │       ├── video.rs           # capture → encode → broadcast, and switching display
│   │       ├── control.rs         # TCP control server (per-connection thread)
│   │       ├── identity.rs        # stable host id + long-lived identity key
│   │       ├── tray.rs            # notification-area icon
│   │       └── single_instance.rs # one host per machine
│   └── vd-client                  # Android client + desktop harness
│       └── src
│           ├── app.rs             # UI: searching / host list / full-screen + overlay
│           ├── connection.rs      # session worker, decode thread, reconnects
│           ├── frames.rs          # bounded decoded-frame queue
│           ├── latency.rs         # per-stage timings, for the overlay and logs
│           ├── trust.rs           # remembered host identities
│           ├── visibility.rs      # whether the UI is drawing, for pausing video
│           └── media_codec.rs     # Android MediaCodec FFI (NDK)
└── android                        # Gradle project that packages the APK
    └── app/src/main
        ├── AndroidManifest.xml    # NativeActivity -> libvd_client.so
        └── jniLibs/               # filled in by `cargo ndk` (git-ignored)
```

The Android APK contains **no Java or Kotlin code**. The manifest points at the
platform's `android.app.NativeActivity`, which loads `libvd_client.so` and calls
the exported Rust `android_main` function through the `android-activity` glue.

---

## How discovery works

Discovery is a small, symmetric UDP protocol on port **47700**:

```
  client                                          host
    │  ── Probe (broadcast to 255.255.255.255) ────▶│
    │  ◀────────────────── Announce (unicast) ──────│
    │  ◀────────────────── Announce (beacon) ───────│  every 2 s
    │  ◀──────────────────────────── Bye ───────────│  on shutdown
```

* The client broadcasts a `Probe` every 1.5 s and also probes `127.0.0.1` so a
  host and client on the *same* machine can find each other during development.
* A host answers each probe with a unicast `Announce` and otherwise emits a
  beacon every 2 s.
* The client keeps a host for 10 s after the last announce, then drops it.
* Every datagram is `b"VDD1"` followed by a JSON `Envelope`; the envelope carries
  a protocol version so old and new peers can recognise each other.

Both sides are **poll driven** (`poll(now)`), which lets the same state machines
run in a CLI loop on Windows and in an egui repaint loop on Android.

Reserved for later: TCP port **47701** (`HostInfo::control_port`) carries the
control session and the video stream.

---

## How the control channel works

Once a host is discovered the client opens a TCP connection on its
`control_port` and runs a short, encrypted handshake:

```
  client                                          host
    │  ── Hello { protocol, client, caps,          │
    │            client_public } ─────────────────▶│  derives keys, auto-approves
    │  ◀── Welcome { protocol, host, caps,          │
    │               host_public, salt, tag } ───────│  (last plaintext message)
    │  derives the same key + PIN, verifies tag     │
    │  user confirms the PIN  ──▶  Pair confirmed   │
    │  ◀──────── encrypted Ping / Pong ────────────▶│
```

* Both sides perform an **ephemeral X25519** exchange and derive a session key
  and a six-digit PIN with HKDF-SHA256.
* The **client** shows the PIN and waits for the user; the **host
auto-approves**. The host's `Welcome` carries an HMAC key-confirmation tag.
* After the `Welcome` every message is sealed with **ChaCha20-Poly1305**, using
  a separate key and nonce counter per direction.
* `Ping`/`Pong` keep the session alive; a silent peer is dropped and the client
  reconnects with exponential backoff.

The messages live in [`vd_core::session`](crates/vd-core/src/session) and the
state machines are transport-agnostic (and unit tested without sockets).

### Video frames

Video rides the same encrypted connection. Each payload starts with a one-byte
kind *inside* the sealed plaintext, so a frame is either a control message or a
binary video frame (`vd_core::video`): a 26-byte header (version, format,
dimensions, sequence, timestamp) followed by the encoded image.

The host captures through a `FrameSource` — DXGI Desktop Duplication on
Windows, or a synthetic test pattern as a fallback — encodes and fans the
result out to every client through a `Broadcaster`. Desktop Duplication leaves
the mouse cursor out of the desktop image (it is a separate overlay), so the
pointer position and bitmap are fetched too and blended into the frame before
scaling. Frames larger than the configured size are downscaled with a fast box
filter first, so a 4K desktop becomes a streamable 1280×720. Each client gets a
single-slot channel, so a slow client misses frames instead of queueing them.
Capture pauses entirely when nobody is watching.

The client decodes on its own thread and queues the result for the UI, which
draws one frame per repaint. Frames are drawn in order until one has fallen well
behind the freshest waiting — a hardware decoder returns two pictures at once, so
keeping only the newest would halve the apparent frame rate and save nothing —
and the queues are bounded, so a slow moment costs frames instead of latency.

Changing display is a live operation. The capture thread swaps its source, asks
for a keyframe so that no decoder is left predicting into a picture it has never
seen, and rebuilds the encoder when the new display fits to a different stream
size; the client is told through the config generation and reconfigures itself.

> H.264 is the default (Media Foundation on the host, MediaCodec on the
> tablet); `VD_CODEC=jpeg` forces the older JPEG path, which also serves as the
> automatic fallback when no encoder is available.

### Security note

The channel is encrypted, and the host has a **long-lived identity key** that the
session key is bound to: the client remembers it the first time the user approves
a PIN and challenges anything different afterwards. That both closes the silent
man-in-the-middle hole and removes the PIN prompt on reconnects.

The *first* pairing is still unauthenticated in the strict sense — the host does
not display the PIN, so there is nothing to compare out of band. See
[PLAN.md](PLAN.md#security).

---

## Using the tablet as a second monitor

Mirroring needs nothing beyond the host and the app. Extending needs Windows to
believe in a monitor with no panel behind it, which is what an indirect display
driver is for — and there is already a good one.

The **[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)**
is MIT licensed, signed with a certificate Windows accepts, and used by Sunshine,
OBS and VR setups. This project neither ships nor requires its own driver; it
captures a virtual monitor that exists.

```powershell
# Elevated, once. Downloads the driver, trusts its certificate, installs it and
# writes its mode list with the tablet's 1920x1200 first.
scripts\install-vdd.ps1
```

Reboot afterwards, since the monitor is enumerated at boot. The client's
**Extend** button then becomes available, and the host switches between mirroring
and extending without dropping the session.

Three things worth knowing:

* VDD's own notes warn that Windows may promote a monitor with nothing attached,
  scrambling display priority and leaving the real screen black after a GPU
  driver update. Uninstall before updating GPU drivers, and keep
  `scripts\install-vdd.ps1 -Uninstall` to hand.
* Extend mode is also where the pipeline gets faster, but it is only half the
  change: the monitor has to exist at the tablet's size *and* the stream has to be
  that size. `VD_VIDEO_SIZE` defaults to 1280x800, so extending still downscales
  unless it is raised to match — at 1920x1200 the scaling costs 0 ms instead of
  17 ms. Making the stream size follow the display is step 4 of Phase 7.
* Which displays are offered comes from the host, and whether one is *virtual* is
  a heuristic on the monitor's hardware id -- the honest answer needs
  `DISPLAYCONFIG`'s output technology. A monitor not recognised as virtual simply
  does not offer Extend.

---

## Building and running

### Prerequisites

* Rust (stable) with the `aarch64-linux-android` target
* `cargo-ndk` (`cargo install cargo-ndk`)
* Android SDK + NDK (`ANDROID_HOME` / `ANDROID_NDK_HOME`)
* A JDK for Gradle (Android Studio's bundled JBR works)

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

### Host (Windows)

```bash
cargo run -p vd-host --release
```

Environment variables:

| Variable | Meaning | Default |
|----------|---------|---------|
| `RUST_LOG` | log level, e.g. `debug` | `info` |
| `VD_HOST_NAME` | overrides the advertised machine name | machine name |
| `VD_OUTPUT` | which display to start on; the client changes it afterwards | `0` |
| `VD_VIDEO_SIZE` | stream size, `WxH` — the stream is downscaled to fit | `1280x800` |
| `VD_VIDEO_FPS` | encoder target frame rate | `30` |
| `VD_VIDEO_BITRATE` | encoder target bitrate, bits per second | `8000000` |
| `VD_CODEC` | `jpeg` forces the fallback codec | H.264 |
| `VD_ENCODER` | `software` forces the software encoder, or a name substring such as `dx12` | hardware first |
| `VD_ENCODER_CHECK` | encode 30 synthetic frames, print the result and exit (a diagnostic) | off |
| `VD_LIST_OUTPUTS` | list every display output and whether it can be duplicated, then exit | off |

The host id is generated once and stored in
`%LOCALAPPDATA%\id.fahrul.vd\host-id`, so clients recognise the machine across
restarts.

While it runs the host shows a notification-area icon — a visible "still here"
marker — with **Exit** on its right-click menu. Only one instance can run at a
time: a second copy exits immediately rather than fighting the first for the
discovery and control ports and announcing the machine twice.

### Client — desktop harness

The Android UI is plain `egui`, so the exactly same code runs on the desktop.
This is the fastest way to iterate:

```bash
cargo run -p vd-client --release      # GUI
cargo run -p vd-client --example discover   # headless, prints hosts for 5 s
```

`VD_CLIENT_NAME` overrides the name this client advertises.

### Client — Android

```bash
scripts/build-android.sh                    # debug APK, arm64-v8a
scripts/build-android.sh --release          # release APK
scripts/build-android.sh --all-abis         # + armeabi-v7a + x86_64 (emulator)

adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n id.fahrul.vd/android.app.NativeActivity
adb logcat -s vd-client                       # app logs
```

The script compiles the Rust crate with `cargo ndk` (writing
`libvd_client.so` into `android/app/src/main/jniLibs/<abi>/`) and then runs
Gradle. You can also open the `android/` folder in Android Studio — just build
the native library first, since the `.so` files are not checked in.

> **Use `--release` when measuring.** A debug native library is roughly ten
> times slower in the pixel paths (decoding, YUV conversion, texture upload),
> which turns 30 fps into about 3. The release APK is signed with the debug key
> so it can be installed locally; replace that with a real key before
> distributing.

---

## Design notes

The project deliberately leans on a few principles:

* **DRY** — the protocol and both discovery state machines live in `vd-core` and
  are compiled into both apps. There is no duplicated wire logic.
* **Single responsibility** — `Envelope` owns encoding, `Browser` owns the
  client view of the world, `Responder` owns the host announcements. None of
  them knows about sockets.
* **Dependency inversion** — `Browser` and `Responder` talk to a `PacketIo`
  trait, not to `UdpSocket`. Production uses `UdpPacketIo`; the unit tests use an
  in-memory `MockIo`, so the algorithms are tested deterministically without a
  network (`cargo test -p vd-core`).
* **YAGNI** — JSON instead of a hand-rolled binary format, no mDNS, no config
  file, and **no video transport until discovery is solid**. `HostInfo` carries a
  `control_port` and an extensible `Platform` enum, but nothing more.

## Roadmap

- [x] Host discovery over Wi-Fi (probe / announce / bye)
- [x] Windows host + Android client from one Rust codebase
- [x] Control channel on TCP 47701: handshake, capability exchange, PIN pairing,
      encryption, heartbeats and automatic reconnect
- [x] Video pipeline end to end: frame protocol, capture abstraction, encoded
      frames over the encrypted channel, client render
- [x] Real desktop capture (DXGI Desktop Duplication) with stream downscaling
- [x] H.264 end to end: Media Foundation encode on the host, MediaCodec decode on
      the tablet ([strategy](PLAN.md#gpu-acceleration-strategy-decision-record))
- [x] Hardware (GPU) H.264 encode (AMD hardware MFT)
- [x] Mouse cursor mirrored from the host
- [x] Client UI: searching ripple → host list → full-screen, back to return
- [x] Extend as well as mirror: the tablet chooses the display, and the host
      switches live ([why a virtual display](PLAN.md#phase-7--a-real-second-monitor-extend))
- [x] The tablet as a real second monitor, via a signed
      [virtual display driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
- [x] Overlay on the stream — mirror/extend and latency — hidden by a tap
- [ ] Touch / keyboard input forwarding
- [x] Trust-on-first-use host identity (authenticated pairing, silent reconnects)
- [ ] Zero-copy: D3D11 video processor on the host, Surface on the client
      (efficiency, not speed — see PLAN.md)

## Testing

```bash
cargo test --workspace     # protocol + discovery unit tests
cargo clippy --workspace --all-targets
```

---

## Documentation

| Document | Contents |
|----------|----------|
| [PLAN.md](PLAN.md) | End-to-end roadmap: capture, encode, transport, decode, input |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Dev setup, architecture rules, commit conventions |
| [LICENSE](LICENSE) | MIT |

## License

MIT — see [LICENSE](LICENSE).

Dependencies keep their own licenses; this project's license does not change
that.
