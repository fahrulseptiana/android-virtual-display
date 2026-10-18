# Contributing

Thanks for taking the time to contribute! This document explains how the project
is laid out, how to build and test it, and the conventions we follow.

By participating you agree to keep the discussion respectful and technical. All
contributions are accepted under the project's [MIT license](LICENSE).

---

## Getting started

### Prerequisites

| Tool | Why |
|------|-----|
| Rust stable | everything |
| `cargo-ndk` | cross-compiling the Android client |
| Android SDK + NDK | packaging the APK |
| Android Studio (JBR) or any JDK 17+ | Gradle |
| `adb` | installing/running on a device |

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

### Build and run

```bash
# Host (Windows) — release: a debug build is about ten times slower in the
# pixel paths, which turns 30 fps into about 3.
cargo run -p vd-host --release

# Client — desktop harness (fastest way to iterate on the UI)
cargo run -p vd-client --release
cargo run -p vd-client --example discover   # headless, prints hosts

# Client — Android
scripts/build-android.sh
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n id.fahrul.vd/android.app.NativeActivity
adb logcat -s vd-client
```

### Testing without a device

The host and the desktop client run happily against each other on one machine,
which is far quicker than deploying to a tablet. The desktop client cannot decode
H.264 (that is Android-only), but it exercises discovery, the handshake and the
control channel.

Two host diagnostics save time:

| Variable | What it does |
|---|---|
| `VD_LIST_OUTPUTS=1` | lists every display output and whether it can be duplicated, then exits |
| `VD_ENCODER_CHECK=1` | encodes 30 synthetic frames and reports, so the hardware encoder can be checked without a client |

Before opening a pull request, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

---

## Project layout

```
crates/vd-core     shared, platform-agnostic: protocol, discovery, session,
                   latency, display descriptions
crates/vd-host     Windows host binary
crates/vd-client   Android cdylib + desktop harness
android/           Gradle project that packages id.fahrul.vd
scripts/           developer scripts
```

See [PLAN.md](PLAN.md) for the roadmap and [README.md](README.md) for how
discovery works today.

---

## Architecture rules

These exist to keep the shared core genuinely shared. Please respect them:

1. **`vd-core` must stay platform-agnostic and dependency-light.** No Windows
   APIs, no Android APIs, no JNI, no threads, no filesystem. It is compiled for
   both targets and for tests.
2. **Talk to abstractions, not sockets.** Discovery talks to the `PacketIo`
   trait, and the session's framing works on plain byte slices, so it can be
   driven by any stream. Algorithms should not reach for
   `UdpSocket`/`TcpStream` themselves.
3. **Keep it testable.** New logic in `vd-core` should be unit-testable with
   `MockIo` and no network. A test that needs a real socket belongs in an
   integration test.
4. **YAGNI.** Don't build the next phase early. No config file until someone
   needs one, and no abstraction until there is a second implementation of it —
   the transport was deliberately left as TCP rather than put behind a trait
   because there was only ever one thing to put there.
5. **One responsibility per type.** `Envelope` encodes, `Browser` tracks the
   client's view, `Responder` announces. Don't merge them.
6. **No panics in library code.** Return `Result`; the app may log and recover.
   `unwrap()` in `vd-core` is a review blocker (tests excepted).
7. **The platform seam is narrow.** Windows-specific capture lives in `vd-host`,
   behind the `FrameSource` trait; nothing downstream knows what a display is.
   Keep new platform code on that side of the seam, and keep the seam honest —
   `vd-client` has exactly one Android-only module for the same reason.

---

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/). **Every
commit must follow this format:**

```
<type>(<optional scope>): <description>

[optional body]

[optional footer(s)]
```

Allowed types:

| Type | Use for |
|------|---------|
| `feat` | a new feature |
| `fix` | a bug fix |
| `docs` | documentation only |
| `refactor` | code change that is neither a feature nor a fix |
| `perf` | performance improvement |
| `test` | tests only |
| `build` | build system, dependencies, packaging |
| `ci` | CI configuration |
| `chore` | maintenance that doesn't fit elsewhere |
| `revert` | reverting a previous commit |

Scopes (optional, but preferred): `core`, `host`, `client`, `android`, `deps`,
`docs`, `plan`, `sandbox`. A scope names the area rather than the file:
`docs(plan)` for the roadmap, `feat(sandbox)` for the test tooling.

Rules:

- Imperative, lower-case description, no trailing period:
  `feat(core): add session handshake state machine`.
- One logical change per commit. Keep commits reviewable.
- A breaking change gets a `!` (`feat(core)!: ...`) and/or a
  `BREAKING CHANGE:` footer.
- Reference issues in the footer: `Closes #12`.

Examples:

```
feat(core): add session handshake state machine
fix(client): drop stale hosts after the TTL expires
docs: document the discovery protocol
build(android): exclude unused kotlin-stdlib from the APK
refactor(core): move ports into the protocol module
```

---

## Branches and pull requests

- Branch from `main`. Use a descriptive, kebab-case name with a type prefix:
  `feat/session-handshake`, `fix/host-ttl`.
- Keep pull requests focused. If you find an unrelated bug, open a separate PR.
- A PR should:
  - explain **what** and **why** (link the PLAN milestone/phase),
  - note how it was tested,
  - keep CI green (`fmt`, `clippy -D warnings`, `test`).
- Prefer several small, well-described commits over one giant commit.

### Definition of done

- [ ] `cargo fmt --all` applied.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes; new logic has tests.
- [ ] Documentation updated (README / PLAN / inline rustdoc) when behaviour
      changes.
- [ ] Commits follow the convention above.

---

## Code style

- Rust 2021, `rustfmt` defaults, `clippy` clean at `-D warnings`.
- Public items get rustdoc; comment *why*, not *what*.
- Keep functions small and name things after the domain (`Announce`, `Responder`,
  not `Msg1`, `Manager`).
- Errors: `thiserror` in libraries, `anyhow`-style context only in binaries.

---

## Reporting bugs / proposing features

- Bugs: include the OS, Android version/device, host + client versions, and
  relevant `adb logcat -s vd-client` / host logs.
- Features: describe the use case first; check [PLAN.md](PLAN.md) to see whether
  it is already scheduled.
- Security issues: please **do not** open a public issue — contact the
  maintainer directly.
