# Android Virtual Display

Use an Android tablet as a wireless secondary display for a Windows PC.

Two apps, one shared codebase:

| App | Platform | Crate | What it does |
|-----|----------|-------|--------------|
| **Host** | Windows | `crates/vd-host` | Runs on the PC, announces itself on the LAN. Later: captures the desktop and encodes a video stream. |
| **Client** | Android | `crates/vd-client` | Runs on the tablet/phone, discovers hosts. Later: decodes and renders the stream. |

Both depend on `crates/vd-core`, which holds everything that is not
platform specific: the wire protocol and the discovery state machines.

> **Status: discovery only.** The virtual display / video transport is *not*
> implemented yet. Today the client binds a UDP socket, finds hosts on the local
> network and shows them in a list. That is the whole feature set — and it works
> on a real Android device talking to a real Windows host over Wi-Fi.

Package id: `id.fahrul.vd` · min SDK 31 (Android 12) · target SDK 36.

---

## Repository layout

```
.
├── Cargo.toml                     # cargo workspace
├── crates
│   ├── vd-core                    # shared: protocol + discovery (no I/O deps)
│   │   └── src
│   │       ├── protocol.rs        # HostInfo, DiscoveryMessage, Envelope (JSON over UDP)
│   │       ├── net.rs             # PacketIo trait + UdpPacketIo implementation
│   │       └── discovery
│   │           ├── browser.rs     # client side: probe + collect announces
│   │           └── responder.rs   # host side: answer probes + beacon
│   ├── vd-host                    # Windows host binary
│   └── vd-client                  # Android cdylib + desktop harness
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

Reserved for later: TCP port **47701** (`HostInfo::control_port`) will carry the
display session handshake and stream.

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

| Variable | Meaning |
|----------|---------|
| `RUST_LOG` | log level, e.g. `debug` (default `info`) |
| `VD_HOST_NAME` | overrides the advertised machine name |

The host id is generated once and stored in
`%LOCALAPPDATA%\id.fahrul.vd\host-id`, so clients recognise the machine across
restarts.

### Client — desktop harness

The Android UI is plain `egui`, so the exactly same code runs on the desktop.
This is the fastest way to iterate:

```bash
cargo run -p vd-client            # GUI
cargo run -p vd-client --example discover   # headless, prints hosts for 5 s
```

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
- [ ] Control connection on TCP 47701 (session handshake, capability exchange)
- [ ] Desktop capture on Windows (DXGI Desktop Duplication)
- [ ] Hardware video encode (Media Foundation / NVENC)
- [ ] Android decode (MediaCodec) and render
- [ ] Touch / keyboard input forwarding

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
