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
│   ├── capture (DXGI)              desktop frame + cursor, downscaled        │
│   ├── encode (Media Foundation)   hardware H.264 bitstream                  │
│   └── input injection (SendInput) mouse / keyboard          ← not yet       │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    ▲  Wi-Fi, same L2 network
                                    ▼
┌──────────────────────────── Android client (Rust) ──────────────────────────┐
│                                                                             │
│  vd-client                                                                  │
│   ├── discovery::Browser          UDP 47700                                 │
│   ├── control client              TCP 47701                                 │
│   ├── MediaCodec decode           H.264 → NV12 → RGBA                       │
│   ├── render (egui + wgpu)        searching / host list / full-screen video │
│   └── input capture               touch / keyboard → host    ← not yet      │
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

**Where we are:** hardware encode and hardware decode both work, driven from
system memory, and the display sustains the host's 30 fps. The zero-copy
transfers below are the remaining part of this strategy; they are now an
efficiency question (CPU, power, headroom) rather than a speed one.

```
host:   DXGI (D3D11 texture)
          └─▶ D3D11 Video Processor    scale 4K→target, BGRA→NV12   ← not yet
                └─▶ Media Foundation hardware H.264 MFT
                      └─▶ encrypt ─▶ TCP

client: TCP ─▶ decrypt ─▶ MediaCodec hardware H.264 ─▶ Surface      ← not yet
                             └─▶ composited by wgpu (Vulkan)
```

What the hardware codecs removed, measured on a 3840×2160 desktop streamed at
1280×720:

| Removed | Was | Now |
|---------|-----|-----|
| CPU JPEG encode (host) | 16 ms | hardware H.264 encode, ~3 ms |
| CPU JPEG decode (tablet) | ~400 ms | MediaCodec, ~2 ms |
| Software H.264 encode | ~15 fps ceiling | ~30 fps, matching the host target |

Still on the CPU, and worth removing later:

| Still on the CPU | Cost | Would become |
|------------------|------|--------------|
| GPU→CPU readback of the 4K frame | 33 MB/frame | stays on the GPU |
| Box downscale 4K→720p (host) | ~23 ms | D3D11 video processor (~1 ms) |
| YUV→RGBA conversion (tablet) | ~13 ms | decoded straight to a `Surface` |
| RGBA texture upload (tablet) | ~9 ms | not needed with a `Surface` |

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
- [x] Heartbeats (`Ping`/`Pong`) and automatic reconnect: an immediate retry
      after a drop, then a short backoff (0.5 s … 5 s).
- [x] `vd-core::session` — handshake, pairing, framing and cipher, all
      transport-agnostic and unit tested.

The host publishes a **long-lived identity key**, and the session key is derived
from both the ephemeral exchange and a Diffie-Hellman against that identity, so
only a host holding the private half can complete the handshake. The client
records the identity the first time the user approves a PIN and accepts that key
silently afterwards — see [Security](#security).

**Remaining limitation:** the host still does not display the PIN, so the *first*
pairing remains open to an active man-in-the-middle — an inherent property of
trust on first use without an out-of-band comparison. Every later connection is
protected, because a different key is challenged.

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
- [x] **Cursor:** Desktop Duplication leaves the pointer *out* of the desktop
      image — it is a separate overlay — so the duplication also fetches the
      pointer position and bitmap and blends it into the frame before scaling.
      Handles all three shapes Windows emits (colour, masked-colour,
      monochrome AND/XOR).
- [ ] **Fallback:** Windows.Graphics.Capture — needed for some HDR/protected
      cases and per-window capture.
- [ ] **Last resort:** GDI `BitBlt` — slow, but always available.
- [ ] Pick a monitor at runtime rather than by index; handle monitors appearing
      and disappearing.

### Phase 4 — Video encode

- [x] Interim codec: JPEG (`vd-core::codec`), enough to prove the pipeline.
      Kept as the capability-negotiated fallback.
- [x] **Hardware H.264** through a Media Foundation MFT (`AMDh264Encoder`), fed
      NV12 from system memory. Getting there needed four fixes that all reported
      the wrong error: the D3D11 device lacked `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`;
      the async unlock has to go on the transform (via
      `IMFTransform::GetAttributes`, since casting to `IMFAttributes` fails);
      `MFT_MESSAGE_SET_D3D_MANAGER` wants the device-manager *pointer*, not the
      reset token; and async transforms must be driven through their event
      queue. Falls back to the software encoder automatically.
- [ ] **Zero-copy input.** Scale and convert to NV12 with the **D3D11 Video
      Processor** and hand the encoder a D3D11 texture, removing the 33 MB
      readback and the CPU downscale. Measurement in Phase 5 put that downscale
      at **23 ms of the 32 ms** the host spends per frame, which makes this the
      single largest win left in the pipeline — bigger than the whole network.
      ToDo: Two things have to be settled
      first, which is why it is not simply a matter of swapping the scaler:
      * the **cursor** is currently blended into the CPU BGRA buffer, and a
        zero-copy path never has that buffer — so it must either be composited
        on the GPU, or moved to the client as a position plus bitmap that the
        client draws over the video (the more natural shape for a remote
        display, and it also removes the cursor from the video for free);
      * the duplicated texture is only valid between `AcquireNextFrame` and
        `ReleaseFrame`, so the whole GPU conversion has to run inside that
        window rather than being handed off.
- [ ] Rate control is done (see below). The remaining headroom would mostly
      fund a higher stream resolution, so this is worth doing alongside that,
      not before it.
- [x] Rate control for Wi-Fi: capped variable bitrate, a configurable mean
      bitrate (`VD_VIDEO_BITRATE`) and **keyframes on demand**, all through
      `ICodecAPI`. Verified supported by the AMD encoder. Forcing a keyframe
      fixed a real bug: a reconnecting client used to sit on a blank screen for
      up to the GOP length (~6 s here) because the encoder keeps emitting
      predicted frames.
- [ ] Optional, later: HEVC and AV1 (AV1 only where hardware decode exists).

**Decision:** start with H.264 — it has the widest hardware decode support on
Android and is the safest baseline. Add codecs behind the capability
negotiation from Phase 2.

### Phase 5 — Transport

- [x] **Start:** TCP with a length-prefixed frame header (kind byte, then either
      a JSON control message or a binary video frame). Simple and reliable, and
      it carries the encrypted session.
- [x] Frame dropping rather than queueing: the host's fan-out gives each client
      a single slot, and the client's decode queue and frame queue are bounded,
      so a slow link degrades by dropping frames.
- [x] Latency is measured rather than assumed. The host stamps every frame from
      one clock, `Ping`/`Pong` carry clock readings, and the client derives the
      offset between the two clocks the way NTP does. It subtracts the host's
      own share to leave the network's. Reported every 5 s as mean/p95/worst.
- [x] **Evaluated, and decided against.** A datagram transport would address a
      smaller share of the latency than the host's own scaling does. See below.

> The original note here read "keep the transport behind a trait so Phase 5 can
> replace TCP without touching capture, encode, decode or UI". The measurement
> above answered the question the trait existed to defer, so the trait was never
> written: an abstraction with one implementation and no concrete second one is
> just a place for the wiring to hide. The seam that does exist is the framing
> layer (`session::frame`), which is pure and works on byte slices, so it can
> already be driven by any stream — including a datagram-shaped one.

#### Transport decision: TCP stays

Measured at 720p30, capture 3840x2160, Ryzen AI MAX+ 395 → iPlay60 mini Pro over
Wi-Fi. "Client" is the age of a frame when it *arrives*, so it covers capture,
encode, transport and delivery — not decode, render or panel scan-out.

| Stage | Loopback | Wi-Fi |
|-------|----------|-------|
| Host: scale 4K→720p | 23 ms | 23 ms |
| Host: encode | 4 ms | 4 ms |
| Host: capture → socket write | 32.3 ms | 32.7 ms |
| Host: the write itself | 0.2 ms | **0.1 ms** |
| Client: frame age on arrival | 32.7 ms | 40.7 ms |
| — of which network | 0.4 ms | **~8 ms** |

Mean 40.7 ms, p95 48 ms, worst 73-86 ms, 28-29 fps, no duplicated or reordered
frames observed.

The transport is not where the time goes. A socket write of 0.1 ms means the
window never fills, so there is no head-of-line blocking to remove; the queue
ahead of the write does not grow either. Meanwhile 23 ms of every 32 ms the host
spends is CPU scaling a 33 MB BGRA 4K frame down to 720p. Replacing TCP would
chase the 8 ms while the 23 ms sits untouched — and QUIC or FEC adds its own
overhead, plus a congestion-control implementation to get wrong.

Revisit if the host-side scaling is ever moved to the GPU: at that point the
network would be the largest single term, and the 73-86 ms tail would be worth
attacking directly.

### Phase 6 — Android decode + render

- [x] H.264 through MediaCodec (`vd-client::media_codec`), the NDK C API via a
      small FFI layer behind `#[cfg(target_os = "android")]`. Output is
      requested as NV12 and converted to RGBA on the CPU for the egui texture.
- [x] Decode runs on its own thread. Inline decoding stopped the reader for
      ~19 ms per frame, the TCP window filled, the host's write blocked, and its
      broadcaster dropped frames — which capped the client at ~12 fps.
- [x] A bounded frame **queue**, not a single slot. A hardware decoder does not
      emit evenly: it stalls and returns two pictures at once, and a single slot
      let the second overwrite the first before the UI saw it, discarding half of
      every pair.
- [x] Sustained 720p30 end to end (29.0 fps measured), matching the host's
      target. Those two bugs were the entire gap.
- [x] Client UI (`vd-client::app`): three states — a "No Host Found" ripple
      while searching, a tappable host list (name, platform, address, online
      dot), and the desktop full-screen with Android's back button returning to
      the list and disconnecting.
- [ ] Zero-copy output. **This is no longer merely an efficiency matter.** The
      measurements added in point 7 put the client at 19-25 ms decoding and 7-9
      ms building and uploading each texture, against 41 ms to arrive — so the
      client's own work is roughly what the whole 50 ms budget allows, and the
      display lands at 96-136 ms. Decoding to a `Surface` would remove the
      YUV→RGBA conversion and the texture upload together. It costs the egui
      overlay, so it needs the overlay drawn over the surface rather than the
      texture drawn by egui.
- [x] Adaptive: a frame is drawn unless it has fallen more than 70 ms behind
      the freshest one waiting, in which case it is dropped and counted. It is
      *not* simply newest-wins: that was tried first, and halved the drawn rate
      to 14 fps of 28, because the decoder returns two pictures at once and
      there are 53 repaint slots per second for 28 frames. Dropping only what is
      genuinely late keeps motion smooth — 26-28 fps drawn, ~0 dropped — while
      still refusing to draw the past after a stall.
- [x] Decode and render timings are tracked against the budget alongside the
      arrival age, the display age, the drawn rate and the drop count, in one
      line every five seconds. They say the client is over budget: see below.
- [x] Handle rotation, notch/cutout, and backgrounding. Rotation needs no code
      (`configChanges` keeps the activity alive and the view scales to fit).
      The cutout is handled by a theme with `windowLayoutInDisplayCutoutMode`
      set to `shortEdges`, so the screen is used rather than letterboxed away
      from a notch — unverifiable here, as this tablet has no cutout.
      Backgrounding is detected by the UI going quiet, and the client asks the
      host to stop sending; the host drops its subscription and the capture loop
      idles. Resuming re-subscribes, which asks for a keyframe — without one the
      decoder could not pick up after the gap.
- [ ] Software keyboard for hosts that need it (Phase 7).

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
- [x] Trust on first use: the host has a persistent identity key, and the client
      stores it after the first pairing. A different key is challenged and
      clearly labelled, which is what separates a re-installed host from an
      impostor.
- [x] Bind the session key to that identity, so a man-in-the-middle cannot
      re-key silently — it cannot derive the session key at all without the
      host's private key.
- [ ] Document the threat model (LAN attacker, rogue host) in `SECURITY.md`.
      That document does not exist yet; the summary above is all there is.

### Performance budgets (1080p60)

| Stage | Budget | Measured (720p30, Wi-Fi) |
|-------|--------|--------------------------|
| Capture + scale | < 5 ms | ~28 ms (23 ms of it the 4K→720p downscale) |
| Encode | < 10 ms | 4 ms (hardware) |
| Network (LAN) | < 5 ms | ~8 ms |
| Decode | < 10 ms | not yet measured |
| Render | < 5 ms | not yet measured |
| **Total** | **< 50 ms** | 40.7 ms mean, 48 ms p95 on arrival |

Instrumentation ships from Phase 3 onward so regressions are visible. The
measured column covers only up to the frame *arriving*; decode, render and panel
scan-out are still unmeasured, so the true glass-to-glass figure is higher than
the one above.

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
| **M4 — Usable** ✅ | Real DXGI capture, hardware H.264 both ends, 720p30, cursor mirrored, three-state UI | 3–6 |
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

1. ~~eframe/egui overlay vs. a dedicated `SurfaceView`/`TextureView` for video?~~
   Settled for now: egui holds 30 fps, so the overlay stays. A `Surface` remains
   the option if CPU or power becomes the limit.
2. QUIC vs. custom UDP+FEC for Phase 5 — decide with real measurements.
3. Is per-window capture (WGC) worth it, or is per-monitor enough?
4. Minimum supported Android version — currently 31 (Android 12); MediaCodec
   async API and Vulkan make this comfortable. Revisit if we need older devices.
5. Do we want a headless host service (Windows service) in addition to the CLI?
