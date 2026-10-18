#!/usr/bin/env bash
#
# Builds the host and the desktop client for use inside Windows Sandbox.
#
# The sandbox has no Visual C++ runtime, so these link against the static CRT —
# without it they fail to start with a missing VCRUNTIME140.dll. The build
# happens here on the host, which is not the same as *running* it there: nothing
# in this project loads a driver outside the sandbox.
#
# Output lands in target/sandbox/, which the sandbox sees as C:\vd\target\sandbox.
set -euo pipefail
cd "$(dirname "$0")/.."

# A target directory of its own: switching RUSTFLAGS invalidates fingerprints,
# so sharing one with normal builds would rebuild everything on every switch.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target/sandbox-build}"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"

# Two builds, not one. `--bin` applies to every package on the command line, so
# `-p vd-host -p vd-client --bin vd-client-desktop` quietly builds only that one
# binary and leaves vd-host's alone -- and a stale host then gets copied into the
# bundle, which is a confusing way to spend an afternoon.
cargo build --release -p vd-host
cargo build --release -p vd-client --bin vd-client-desktop

out=target/sandbox
mkdir -p "$out"
cp "$CARGO_TARGET_DIR/release/vd-host.exe" "$out/"
cp "$CARGO_TARGET_DIR/release/vd-client-desktop.exe" "$out/"

# devcon creates the root-enumerated device node a virtual display driver needs;
# pnputil only stages the package. It ships with the WDK and links no C runtime,
# so it runs inside the sandbox as-is.
devcon=$(find "/c/Program Files (x86)/Windows Kits/10/Tools" -name devcon.exe -path "*x64*" 2>/dev/null | sort | tail -1)
if [ -n "$devcon" ]; then
    cp "$devcon" "$out/"
    echo "devcon.exe copied from $(dirname "$devcon")"
else
    echo "warning: devcon.exe not found; the virtual display cannot be created"
    echo "         (install the WDK, or the sandbox run will only report the error)"
fi

echo "sandbox bundle written to $out"
echo "run sandbox/vd-sandbox.wsb to try it"
