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

### Phase 2 — Control channel

Establish a reliable TCP session on `HostInfo::control_port` (47701) before any
video is sent.

- [ ] Session handshake: protocol negotiation, capability exchange
      (codecs, max resolution, HDR, input modes).
- [ ] Pairing: 6-digit PIN shown on the host, entered on the client; derive a
      shared secret and encrypt the session (Noise `NNpsk0` or TLS 1.3).
- [ ] Keep-alive + heartbeats; clean teardown; reconnect with exponential
      backoff.
- [ ] `vd-core::session` — handshake state machine, transport-agnostic and unit
      tested with `MockIo`.

**Why first:** it is small, testable, and everything later hangs off it.

### Phase 3 — Desktop capture (Windows)

- [ ] **Primary:** DXGI Desktop Duplication — lowest latency, GPU-resident
      textures, dirty-rect + move-rect metadata.
- [ ] **Fallback:** Windows.Graphics.Capture — needed for some HDR/protected
      cases and per-window capture.
- [ ] **Last resort:** GDI `BitBlt` — slow, but always available.
- [ ] Abstraction: `trait FrameSource` returning a frame + dirty regions, so the
      encoder is capture-agnostic.
- [ ] Handle display changes (resolution, HDR toggle, monitor add/remove).

### Phase 4 — Video encode

- [ ] **Primary:** hardware H.264 via Media Foundation MFTs (AMD AMF / NVENC /
      QuickSync), zero-copy from the capture texture.
- [ ] **Fallback:** software x264 at reduced quality.
- [ ] Rate control for Wi-Fi: variable bitrate with a capped bitrate, keyframe
      on demand (first frame, loss recovery, resolution change).
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

- [ ] `MediaCodec` in async mode, decode to an `AImageReader`/`Surface`.
- [ ] Render the video surface directly (SurfaceView/texture) under a thin egui
      overlay for connection status.
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

A remote display is remote control. From Phase 2 onward:

- Never stream to an unpaired client.
- Encrypt the session; authenticate the host to the client **and** the client to
  the host.
- The PIN is a one-time pairing secret, not the long-term key.
- Document the threat model in `SECURITY.md` (LAN attacker, rogue host).

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
| **M2 — Connected** | Encrypted, negotiated session; no video yet | 2 |
| **M3 — First frame** | Capture → encode → TCP → decode → screen | 3–6 (minimal) |
| **M4 — Usable** | 1080p30+, < 100 ms, stable | 5–6 |
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
