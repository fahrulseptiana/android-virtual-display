# PLAN — Android Virtual Display

End-to-end plan for turning an Android tablet into a low-latency wireless
secondary display for a Windows PC.

This document is the source of truth for *what* we build, in *what order*, and
*why*. It is intentionally opinionated; each phase ends with something that can
be demonstrated on real hardware.

---

## 1. Goal

Mirror (and later extend) a Windows desktop onto an Android device over Wi-Fi,
with input flowing back to the PC, at a latency that feels native.

### Success criteria

| # | Criterion | Target |
|---|-----------|--------|
| 1 | Glass-to-glass latency on a clean 5 GHz LAN | < 50 ms |
| 2 | Sustained framerate at 1080p60 | 60 fps, no dropped-frame storms |
| 3 | Time from "open app" to "first frame" | < 3 s |
| 4 | Reconnect after Wi-Fi blip | < 5 s, automatic |
| 5 | Setup for a new device | discover → pair → stream, no config files |

### Non-goals (for now)

- Multi-client broadcast (one viewer per host session).
- macOS/Linux hosts (the transport is portable, the capture backend is not).
- Internet/WAN traversal (LAN only; remote access is a later, separate problem).
- Audio (planned, but after video + input are solid).

---

## 2. Architecture

```
┌──────────────────────────── Windows host (Rust) ────────────────────────────┐
│                                                                             │
│  vd-host                                                                    │
│   ├── discovery::Responder        UDP 47700  (probe / announce / bye)       │
│   ├── control server              TCP 47701  (handshake, caps, input)       │
│   ├── capture (DXGI / WGC)        GPU/CPU frames                            │
│   ├── encode (Media Foundation)   H.264/HEVC bitstream                      │
│   └── input injection (SendInput) mouse / keyboard / touch                  │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    ▲  Wi-Fi, same L2 network
                                    ▼
┌──────────────────────────── Android client (Rust) ──────────────────────────┐
│                                                                             │
│  vd-client                                                                  │
│   ├── discovery::Browser          UDP 47700                                 │
│   ├── control client              TCP 47701                                 │
│   ├── MediaCodec decode           H.264/HEVC → Surface                      │
│   ├── render (egui + wgpu)        video surface + on-screen UI              │
│   └── input capture               touch / keyboard → host                   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘

        shared by both:  vd-core  (protocol, discovery, session types)
```

Everything that is not OS-specific lives in `vd-core` and is compiled into both
apps. Capture/encode lives only in the host; decode/UI lives only in the client.

### GPU acceleration strategy (decision record)

The heavy lifting is done by hardware **video codecs**, not by Vulkan.
Vulkan is already present — the client composites with `wgpu`, whose Android
backend is Vulkan — but no amount of it helps the actual bottleneck, which is
the codec.

```
host:  DXGI (D3D11 texture)
         └─▶ D3D11 Video Processor      scale 4K→target, BGRA→NV12, on the GPU
               └─▶ Media Foundation MFT hardware H.264 (D3D11-aware, zero-copy)
                     └─▶ encrypt ─▶ TCP

client: TCP ─▶ decrypt ─▶ MediaCodec hardware H.264 ─▶ Surface
                             └─▶ composited by wgpu (Vulkan) under the egui UI
```

What this removes, measured on a 3840×2160 desktop streamed at 1280×720:

| Removed | Cost today | Replaced by |
|---------|-----------|-------------|
| GPU→CPU readback of the 4K frame | 33 MB/frame | stays on the GPU |
| CPU box downscale | 25 ms | D3D11 video processor (~1 ms) |
| CPU JPEG encode | 16 ms | hardware H.264 encode (~5 ms) |
| CPU JPEG decode on the tablet | ~400 ms | MediaCodec (~2 ms) |
| RGBA texture upload on the client | 3.7 MB/frame | not needed (decode to a Surface) |

Why not Vulkan Video (`VK_KHR_video_encode_queue` / `_decode_queue`)? It is a
real API, but hardware *encode* support on Windows drivers is immature, and on
Android the first-class, guaranteed path is MediaCodec. Capture is inherently
platform-specific anyway (DXGI here, PipeWire/DRM elsewhere), so a single
portable GPU API buys little. Revisit if the driver landscape changes.

One refinement is worth doing later: import MediaCodec's decoded
`AHardwareBuffer` into `wgpu` as an external texture
(`VK_ANDROID_external_memory_android_hardware_buffer`), so the video and the UI
share one renderer instead of being composited as two layers.

**Fallback:** the JPEG/raw path stays behind capability negotiation
(`VideoCodec`), so an old or exotic peer still works.

---

## 3. Phases

### Phase 0 — Foundation ✅ done

- [x] Cargo workspace with `vd-core`, `vd-host`, `vd-client`.
- [x] Android packaging with no Java/Kotlin (NativeActivity + `libvd_client.so`).
- [x] Desktop harness so the client UI can be developed without a device.
- [x] Unit tests with an in-memory transport.

### Phase 1 — Discovery over Wi-Fi ✅ done

- [x] Versioned wire format (`VDD1` + JSON envelope).
- [x] `Probe` / `Announce` / `Bye` messages.
- [x] Poll-driven `Browser` and `Responder`.
- [x] Host TTL, beacon interval, loopback probe for single-machine dev.
- [x] Verified on a real Android device ↔ Windows host.

**Exit criteria met:** the client lists every host on the LAN within ~2 s, and
drops one within ~10 s of it going away.

### Phase 2 — Control channel ✅ done

A reliable, encrypted TCP session on `HostInfo::control_port` (47701).

- [x] Session handshake: protocol negotiation and capability exchange
      (codecs, max resolution/fps, input modes).
- [x] Pairing: ephemeral X25519 exchange derives a session key and a six-digit
      PIN. The **client** shows the PIN and asks its user to confirm; the
      **host auto-approves** and establishes immediately.
- [x] Key confirmation: the host's `Welcome` carries an HMAC tag over the
      handshake transcript, so the client can detect a broken exchange.
- [x] Encryption: ChaCha20-Poly1305 with per-direction keys and monotonic
      nonces; the `Welcome` is the last plaintext message.
- [x] Heartbeats (`Ping`/`Pong`) and automatic reconnect with exponential
      backoff (1 s … 30 s).
- [x] `vd-core::session` — handshake, pairing, framing and cipher, all
      transport-agnostic and unit tested.

**Known limitation:** the host does not display the PIN, so the PIN is a
client-side approval gate rather than an out-of-band comparison. It protects
against passive eavesdroppers (the channel is encrypted) but not against an
active man-in-the-middle who re-runs the exchange on both sides. See
[Security](#security) for the planned fix (trust-on-first-use host key).

### Phase 3 — Desktop capture (Windows)

- [x] Abstraction: `trait FrameSource` returning a frame, so the encoder is
      capture-agnostic, plus a synthetic `TestPattern` source.
- [x] **Primary:** DXGI Desktop Duplication (`vd-host::dxgi`), with
      `DXGI_ERROR_WAIT_TIMEOUT` and `DXGI_ERROR_ACCESS_LOST` handled (the
      duplication is rebuilt after mode changes, UAC prompts and locks).
      Selects the monitor with `VD_OUTPUT`.
- [x] Stream size is bounded: a 4K frame is downscaled to the target size with
      a box filter before encoding (the `image` crate's resampler cost 1.7 s per
      4K frame; the box filter costs 25 ms).
- [ ] **Fallback:** Windows.Graphics.Capture — needed for some HDR/protected
      cases and per-window capture.
- [ ] **Last resort:** GDI `BitBlt` — slow, but always available.
- [ ] Pick a monitor at runtime rather than by index; handle monitors appearing
      and disappearing.

### Phase 4 — Video encode

- [x] Interim codec: JPEG (`vd-core::codec`), enough to prove the pipeline.
      Measured from a 3840×2160 desktop streamed at 1280×720 in release:
      **8.9–13 fps** (box downscale 25 ms, JPEG encode 16 ms per frame). On the
      tablet the pure-Rust JPEG *decoder* is the client-side limit (~2 fps).
- [ ] **Primary:** hardware H.264 through Media Foundation. The image is scaled
      and converted to NV12 by the **D3D11 Video Processor**, then fed to a
      hardware encoder MFT as a D3D11 texture (zero-copy). See the
      [GPU acceleration strategy](#gpu-acceleration-strategy-decision-record).
- [ ] Rate control for Wi-Fi: variable bitrate with a capped bitrate, keyframe
      on demand (first frame, loss recovery, resolution change).
- [ ] Keep the JPEG/raw path as the capability-negotiated fallback.
- [ ] Optional, later: HEVC and AV1 (AV1 only where hardware decode exists).

**Decision:** start with H.264 — it has the widest hardware decode support on
Android and is the safest baseline. Add codecs behind the capability
negotiation from Phase 2.

### Phase 5 — Transport

- [ ] **Start:** TCP with a length-prefixed frame header (id, timestamp,
      keyframe flag, codec, size). Simple and reliable; good enough to measure.
- [ ] **Then:** evaluate an unreliable, ordered datagram transport (QUIC or
      custom UDP + FEC) once real latency data exists. Wi-Fi does not need TCP's
      head-of-line blocking for video.
- [ ] Frame pacing / send scheduling so a slow link degrades gracefully
      (drop frames, never queue unboundedly).

> Keep the transport behind a trait so Phase 5 can replace TCP without touching
> capture, encode, decode or UI.

### Phase 6 — Android decode + render

- [x] JPEG decode on the client and display through an egui texture, off the UI
      thread, with a one-slot mailbox so the display never lags the stream.
- [x] H.264 through MediaCodec (`vd-client::media_codec`), the NDK C API via a
      small FFI layer. Output is requested as NV12 and converted to RGBA on the
      CPU. Measured on the tablet: **7.8 fps**, against ~2 fps for JPEG.
- [ ] Zero-copy output: decode to a `Surface` so frames never touch the CPU.
      Measured on the tablet at 720p: decode **plus** the YUV→RGBA conversion
      costs 18.5 ms, the egui texture upload 10.2 ms, but the whole UI repaint
      takes ~105 ms — and halving the resolution to 960x540 cuts the repaint to
      ~71 ms and raises the rate from 9.5 to 14 fps. The bottleneck is therefore
      **filling the screen with the video texture through egui/wgpu**, not the
      codec and not the host. The fix is a `Surface`: `MediaCodec` can decode
      straight to the `NativeActivity`'s `ANativeWindow`, which the display
      hardware composites for free. That means the app is either showing its
      egui UI or showing video full-screen, not both at once.
- [ ] Adaptive: drop late frames rather than queueing; track decode/render
      timings for the latency budget.
- [ ] Handle rotation, notch/cutout, and backgrounding (pause capture when the
      app is not visible).

### Phase 7 — Input forwarding

- [ ] Touch → absolute pointer on the host's chosen monitor.
- [ ] Mouse (trackpad mode) and scroll.
- [ ] Keyboard (hardware + soft keyboard) → `SendInput` on Windows.
- [ ] Back-pressure and ordering guarantees over the control channel.

### Phase 8 — Polish

- [ ] Audio capture + `AudioTrack` playback.
- [ ] Multi-monitor selection and per-monitor capture.
- [ ] Quality/latency presets.
- [ ] Clipboard sync (optional).
- [ ] Wake-on-LAN / keep-host-awake.
- [ ] Packaging: signed release APK, Windows installer + auto-start.

---

## 4. Cross-cutting concerns

### Security

A remote display is remote control. The control channel is encrypted, but the
current pairing is **not yet authenticated end-to-end** — see the known
limitation in Phase 2. Plan:

- [x] Encrypt the session (ChaCha20-Poly1305, per-direction keys).
- [x] Host proves key knowledge to the client (key-confirmation tag).
- [ ] Trust on first use: the host gets a persistent identity key; the client
      stores it after the first pairing and refuses to talk to a different one
      without warning. This is what turns the PIN into a real check.
- [ ] Bind the session key to that identity so a MITM cannot re-key silently.

Document the threat model (LAN attacker, rogue host) in `SECURITY.md`.

### Performance budgets (1080p60)

| Stage | Budget |
|-------|--------|
| Capture | < 5 ms |
| Encode | < 10 ms |
| Network (LAN) | < 5 ms |
| Decode | < 10 ms |
| Render | < 5 ms |
| **Total** | **< 50 ms** |

Instrumentation ships from Phase 3 onward so regressions are visible.

### Testing

- **Unit** (`vd-core`): protocol round-trips, discovery, handshake — all with
  `MockIo`, no sockets, deterministic.
- **Integration**: host + client on loopback, asserting a real session and a
  known frame reaches the decoder.
- **Device smoke tests**: scripted `adb` install + launch + screenshot.
- **Latency harness**: flash a pattern, measure round-trip via the control
  channel.

### CI (GitHub Actions)

- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- Build the Android `cdylib` for `aarch64-linux-android` (compile check).
- Optional: assemble the debug APK on tags.

---

## 5. Milestones

| Milestone | Contents | Phases |
|-----------|----------|--------|
| **M1 — Discoverable** ✅ | Hosts show up on the device | 0–1 |
| **M2 — Connected** ✅ | Encrypted, paired session; no video yet | 2 |
| **M3 — First frame** ✅ | Synthetic capture → JPEG → encrypted TCP → decode → screen | 3–6 (minimal) |
| **M4 — Usable** | Real DXGI capture ✅, H.264 end to end ✅ (7.8 fps), hardware encode ⚠️, 1080p30+ ✗ | 3–6 |
| **M5 — Interactive** | Input forwarding | 7 |
| **M6 — Native feel** | 1080p60, < 50 ms, audio, reconnect | 5–8 |

---

## 6. Risks & mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Wi-Fi jitter / packet loss | Stutter, latency spikes | FEC + frame dropping, VBR cap, prefer 5 GHz; measure early |
| Hardware encoder fragmentation | Device-specific bugs | Capability negotiation + software fallback |
| DRM/protected content | Black frames | Document as unsupported; WGC is not a DRM bypass |
| Android backgrounding kills the stream | Broken session | Foreground service + explicit lifecycle handling |
| Latency creep from copies | Too slow | Zero-copy paths, budget per stage, instrumentation |
| Scope creep (audio, multi-monitor) | Never ships | Strict phase order; polish only after M4 |

---

## 7. Open questions

1. eframe/egui overlay vs. a dedicated `SurfaceView`/`TextureView` for video?
2. QUIC vs. custom UDP+FEC for Phase 5 — decide with real measurements.
3. Is per-window capture (WGC) worth it, or is per-monitor enough?
4. Minimum supported Android version — currently 31 (Android 12); MediaCodec
   async API and Vulkan make this comfortable. Revisit if we need older devices.
5. Do we want a headless host service (Windows service) in addition to the CLI?
