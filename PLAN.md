# PLAN — Android Virtual Display

End-to-end plan for turning an Android tablet into a low-latency wireless
secondary display for a Windows PC.

This document is the source of truth for *what* we build, in *what order*, and
*why*. It is intentionally opinionated; each phase ends with something that can
be demonstrated on real hardware.

---

## 1. Goal

Mirror, and then genuinely *extend*, a Windows desktop onto an Android device
over Wi-Fi, with input flowing back to the PC, at a latency that feels native.

### Success criteria

| # | Criterion | Target |
|---|-----------|--------|
| 1 | Glass-to-glass latency on a clean 5 GHz LAN | < 50 ms |
| 2 | Sustained framerate at 1080p60 | 60 fps, no dropped-frame storms |
| 3 | Time from "open app" to "first frame" | < 3 s |
| 4 | Reconnect after Wi-Fi blip | < 5 s, automatic |
| 5 | Setup for a new device | discover → pair → stream, no config files |
| 6 | The tablet usable as an *extended* second monitor | Windows shows a second display; windows can be dragged onto it |

### Non-goals (for now)

- Multi-client broadcast (one viewer per host session; Phase 7 step 4 needs this
  revisited before two tablets can each have a screen).
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
│   ├── control server              TCP 47701  (handshake, caps, choice)      │
│   ├── displays (DXGI)             every output, and the one being watched   │
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
│   ├── overlay (egui)              mirror or extend, latency, tap to hide    │
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
- [ ] **Fallback:** Windows.Graphics.Capture — wanted for HDR/protected cases
      and per-window capture, and for anything duplication turns out not to
      reach. It was briefly promoted to the only way to capture a virtual display
      on the strength of a sandbox measurement that proved to be an artifact
      (Phase 7); on hardware, duplication captures one fine. It still hands over
      a D3D11 texture rather than a CPU buffer, which suits the zero-copy work
      better than duplication does.
- [ ] **Last resort:** GDI `BitBlt` — slow, but always available.
- [x] Pick a display at runtime rather than at startup: the client chooses and
      the host swaps capture sources live, without dropping the session
      (Phase 7).
- [ ] Notice the display set *changing* -- a monitor plugged in, a laptop docked,
      a virtual display removed -- and tell the client, rather than leaving it
      with a list that was true when the session began.

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
      Extend mode (Phase 7) removes the same 23 ms a different way, by never
      producing a 4K frame to scale, so this is only the answer while mirroring.
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
- [ ] Software keyboard for hosts that need it (Phase 8).

### Phase 7 — A real second monitor (extend)

Today the host duplicates a *physical* output, so the tablet can only ever show a
copy of a screen that already exists. Extending means Windows has to believe in a
monitor with no panel behind it, which is what the **Indirect Display Driver
(IddCx)** model is for: a user-mode driver that reports an adapter, an EDID and a
list of modes, after which Windows treats it as an ordinary display.

We do not write that driver. The **Virtual Display Driver** already is one: MIT
licensed, signed with a timestamped certificate that Windows accepts, IddCx 1.10,
UMDF, and used by Sunshine, OBS and VR setups. Sunshine takes the same route,
telling users to install a virtual display rather than shipping one. The
alternative was an EV certificate, Microsoft attestation signing and quite
possibly WHQL, to arrive back at something that already works.

Three findings shape the phase, and the first two are measured on this machine
rather than reasoned about:

1. **Desktop Duplication captures a virtual display like any other.** The VDD
   monitor is output 1 of the AMD adapter -- `\\.\DISPLAY6`, 1920x1200, exactly
   the mode it was configured with -- and duplication on it succeeds, which means
   the host captures and encodes frames from it. So the capture path does not
   change at all: a virtual monitor is another output to enumerate and choose.

   This was checked in a sandbox first, and the sandbox said the opposite. That
   was an artifact: its own display stack is virtualised, it presents a "Microsoft
   Remote Display Adapter", and it never exposed the indirect output to DXGI. A
   nested display stack is the wrong instrument for a question about display
   topology -- the right one is the machine that has to do the work.

2. **Extend removes the largest cost in the project.** Per-monitor duplication
   hands us the whole 3840x2160 output and we scale it down on the CPU. Streaming
   the virtual monitor at its own 1920x1200 measures **scale 0 ms**; asking for
   1280x800 from the same monitor measures **17 ms**. Creating the monitor at the
   client's size deletes that work rather than optimising it.

3. **The dependency is an ordinary one.** VDD publishes a driver-only package --
   an INF, a UMDF DLL and a signed catalog -- configured by an XML file listing
   the monitors, GPU and modes to offer. Two details are load-bearing and each
   cost a debugging session: the settings file must be at
   `C:\VirtualDisplayDriver\vdd_settings.xml`, not beside the INF, or the device
   installs and reports OK while creating no monitor at all; and the INF is
   root-enumerated, so `pnputil` stages the package but `devcon` has to create the
   device node. A reboot finishes it.

- [x] **Step 1 — stream any display, virtual ones included.** The host lists
      every output of every adapter, and the capture thread swaps sources at
      runtime, so a virtual monitor is just another output to pick.
      `VD_LIST_OUTPUTS` prints the list by hand.
- [x] **Step 2 — choose it from the tablet.** Two buttons over the stream: mirror
      the screen the host is using, or extend onto the monitor its virtual
      display driver added. The host answers a selection with a fresh list, so the
      highlight follows what is being captured rather than what was asked for.
      Remembering the choice per host, and offering it before the stream starts
      rather than over it, are the obvious refinements.
- [x] **Step 3 — fit the tablet, as far as the monitor goes.**
      `scripts/install-vdd.ps1` writes `vdd_settings.xml` with 1920x1200 first,
      the tablet's panel in landscape, so the monitor is created at the size the
      client has rather than at the host's.
- [ ] **Step 4 — make the stream size follow the display.** `VD_VIDEO_SIZE` is
      still a fixed 1280x800, so extending to a 1920x1200 monitor scales it down
      anyway, at about 18 ms a frame, and the win Step 3 sets up is not actually
      taken. Measured with the stream matching the monitor: 0 ms of scaling,
      against 17 ms to scale that same monitor down. The size should come from the
      display being captured, or from what the client asks for.
- [ ] **Step 5 — a monitor per client**, so two tablets can have a screen each.
      Beyond the current one-viewer-per-host non-goal, so it waits on revisiting
      that rather than on anything here.

Input forwarding (Phase 8) gets easier once this exists: a virtual monitor has a
known rectangle in the virtual desktop, so an absolute pointer maps exactly onto
it rather than being guessed from the tablet's aspect ratio.

#### Trying it without risking the desktop

VDD's own notes warn about display-priority changes and black screens after GPU
driver updates, because Windows may promote a monitor with nothing attached. That
is a real cost to experimenting on the machine in use, so the first attempt was a
disposable Windows Sandbox VM: install the driver, create a monitor, run capture
against it, clean up nothing but the window. VDD being signed is what makes that
possible at all, since the sandbox cannot enable test signing -- it has no
`CmDiag`, and it cannot open its own boot configuration store, both of which were
checked rather than assumed. The tooling lives on the `windows-sandbox` branch.

It was worth doing, but it answered the wrong question. The sandbox is a good
place to find out whether a driver *installs* -- it did, and three details had to
be right for the monitor to appear at all. It is a bad place to ask whether DXGI
can *see* the result, because its own display stack is virtualised and it presents
a display adapter of its own. That is what produced the wrong answer above; the
question needed the machine that has to do the work.

#### Why not our own driver

Recorded because it was the obvious plan first, and the obstacle is not writing a
driver but loading one:

| | Development | Distribution |
|---|---|---|
| Signing | a test certificate | EV certificate, Microsoft attestation signing |
| Also needs | test signing on, which is a boot option | reportedly WHQL for display drivers on 24H2+ and ARM64 |
| Can be enabled where | not from inside a sandbox | not without a certificate and a Partner Center process |

#### Known risks

- **The virtual display's mode list is a configuration file, not an API.** VDD
  reads `vdd_settings.xml` at start-up, so changing what the tablet is offered
  means rewriting that file and restarting the device. Fine for a first version,
  and awkward if the client is ever meant to pick a size while the host runs.
- **We would depend on a third-party driver** for the feature to exist at all.
  Acceptable because the licence is permissive, the project is active and the
  alternative is a signing process; it should still be a documented dependency
  rather than a silent one, and the host should say plainly when no virtual
  display is available.

### Phase 8 — Input forwarding

- [ ] Touch → absolute pointer on the host's chosen monitor.
- [ ] Mouse (trackpad mode) and scroll.
- [ ] Keyboard (hardware + soft keyboard) → `SendInput` on Windows.
- [ ] Back-pressure and ordering guarantees over the control channel.

> A tap on the picture already means "show or hide the overlay" (Phase 7), and
> this phase wants the same tap to reach the host. Decide which before writing
> either: the usual answer is that the first tap wakes the controls and later
> ones are forwarded, but a tap that sometimes clicks and sometimes does not is
> worth being deliberate about. See open question 8.

### Phase 9 — Polish

- [ ] Audio capture + `AudioTrack` playback.
- [ ] Multi-monitor selection and per-monitor capture.
- [ ] Quality/latency presets.
- [ ] Clipboard sync (optional).
- [ ] Wake-on-LAN / keep-host-awake.
- [ ] Packaging: signed release APK, Windows installer + auto-start. Auto-start
      means a scheduled task in the logged-on session rather than a Windows
      service: DXGI duplication needs the interactive session, and session 0 has
      no desktop to duplicate. See open question 5.

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

| Stage | Budget | Measured, mirroring 4K (720p30) |
|-------|--------|---------------------------------|
| Capture + scale | < 5 ms | ~28 ms, of which ~23 ms is the 4K downscale |
| Encode | < 10 ms | 4 ms (hardware) |
| Network (LAN) | < 5 ms | ~8 ms |
| Decode | < 10 ms | 19-25 ms |
| Render | < 5 ms | 7-9 ms |
| **Total** | **< 50 ms** | 41 ms to arrive, 90-135 ms to display |

Instrumentation ships from Phase 3 onward so regressions are visible. The client
measures the whole path up to the frame being drawn, which is why the total has
two numbers: the first is the host and the network, the second what the client
adds. Panel scan-out and the host's own capture latency sit outside both, so true
glass-to-glass is higher again.

Most of the budget is spent on avoidable work rather than on anything inherent:
the downscale goes away by streaming at the display's own size (Phase 7 step 4),
and the decode-and-upload cost by decoding to a `Surface` (open question 1).

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
| **M5 — Extended** ✅ | Windows treats the tablet as a second monitor: the client picks mirror or extend, and the host switches live | 7 |
| **M6 — Interactive** | Input forwarding | 8 |
| **M7 — Native feel** | 1080p60, < 50 ms, audio, packaging | 5–9 |

---

## 6. Risks & mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| Wi-Fi jitter / packet loss | Stutter, latency spikes | FEC + frame dropping, VBR cap, prefer 5 GHz; measure early |
| Hardware encoder fragmentation | Device-specific bugs | Capability negotiation + software fallback |
| DRM/protected content | Black frames | Document as unsupported; WGC is not a DRM bypass |
| Android backgrounding kills the stream | Broken session | Done in Phase 6: the UI going quiet is taken as invisible and the host pauses |
| Driver signing for a virtual monitor | Extend mode would be unreachable | Not needed: a signed, MIT-licensed driver already exists, so nothing here is signed or redistributed. See Phase 7 |
| The virtual display is someone else's driver | Extend depends on a third-party project | Permissively licensed and actively used; the host says plainly when no virtual display is available |
| VDD can be promoted to primary after a GPU driver update | The real screen goes black on boot | Its own notes say to uninstall before updating GPU drivers; `install-vdd.ps1 -Uninstall` is kept for that |
| Latency creep from copies | Too slow | Zero-copy paths, budget per stage, instrumentation |
| Scope creep (audio, per-client monitors) | Never ships | Strict phase order; polish only after M4 |

---

## 7. Open questions

1. **egui texture vs. decoding to a `Surface`** — reopened by the Phase 6
   timings: 19-25 ms decoding and 7-9 ms of texture work per frame puts the
   client over the whole budget on its own.
2. ~~QUIC vs. custom UDP+FEC~~ — neither; see the transport decision in Phase 5.
3. **Per-window capture (WGC) vs. per-monitor** — much less pressing than it
   looked: extending creates a monitor at the client's size, which removes the
   downscale outright (0 ms against 17 ms) rather than making it cheaper. What is
   left of the argument is HDR and protected content.
4. **Minimum supported Android version** — 31, and nothing in Phases 6-8 moves it.
5. **Headless host service** — not as a Windows service; DXGI duplication needs
   the interactive session. Auto-start at logon instead.
6. **The cursor is unowned** — blocks zero-copy on the host, and input forwarding
   needs it either way.
7. **`LATE_AFTER_US` is tuned for 30 fps** — must follow the frame interval if the
   stream is ever run much slower.
8. **The tap gesture is claimed twice.** A tap on the picture toggles the overlay,
   and input forwarding will want that tap to reach the host instead. Settle it
   before Phase 8 rather than after: a tap that sometimes clicks and sometimes
   does not is worth being deliberate about.
9. **Mirror or extend is chosen after connecting**, because the host's display
   list only arrives with the session. Offering it while picking a host means the
   host advertising its display kinds in the discovery beacon, and remembering the
   choice per host.
10. **Whether an output is virtual is a heuristic** — matched on the monitor's
    hardware id, because that is the only place an indirect display names itself.
    `DISPLAYCONFIG`'s output technology would answer it properly.
