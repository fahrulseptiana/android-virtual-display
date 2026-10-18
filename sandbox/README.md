# Windows Sandbox

Why this exists: extend mode needs a virtual display, and testing one on the
machine in use is a bad idea. VDD's own notes warn that Windows may promote a
monitor with nothing attached, which scrambles display priority and can leave the
real screen black after a GPU driver update. A sandbox is disposable, so that
experiment costs nothing but the window.

Nothing here runs on the host except the builds. No driver is installed on the
host, and no test signing is needed anywhere.

## Using it

```bash
scripts/build-sandbox.sh     # host + client, static CRT, into target/sandbox/
scripts/fetch-vdd.sh         # signed VDD driver package, signature checked
sandbox/vd-sandbox.wsb       # double-click, or WindowsSandbox.exe it
```

On logon the sandbox copies the driver package somewhere local, verifies its
signature, installs it with `pnputil`, and reports the displays before and after.
`sandbox/setup.log` lands in the mapped repo, so it can be read from the host
after the sandbox is gone.

Then, inside the sandbox, the host picks the virtual monitor:

```powershell
C:\vd\target\sandbox\vd-host.exe          # output 0, the usual display
$env:VD_OUTPUT=1; C:\vd\target\sandbox\vd-host.exe   # the virtual one
```

The host logs which output it duplicated and at what size, so "Desktop Duplication
works on an indirect display" is answered by reading its log rather than by
reasoning about it.

## What the run has to say

The host prints every adapter and every output, and whether duplication works on
each. What the sandbox is good at is finding out whether a driver *installs*:
VDD did, with no test signing, and three things had to be right before the monitor
appeared at all. Each cost a run to find:

| Step | Why |
|---|---|
| `vdd_settings.xml` at `C:\VirtualDisplayDriver\` | The driver reads its configuration from there, not from beside the INF. Without it the device installs, reports OK, and creates no monitor -- which looks exactly like a monitor DXGI cannot see. |
| Create the device node after `pnputil` | The INF is root-enumerated (`Root\MttVDD`). `pnputil /add-driver` only stages the package; `devcon install` creates the node. |
| Trust the signing certificates | The INF sets `PnpLockdown=1`, and the project's own installer imports the catalog's certificates into `TrustedPublisher`. |

A reboot is needed after installing, and sandbox state survives one.

### What it got wrong

It reported that **no DXGI adapter has an output for the virtual monitor**, and
that was taken to mean Desktop Duplication cannot capture one. On real hardware
the same driver's monitor is output 1 of the AMD adapter at 1920x1200 and
duplication on it succeeds.

The sandbox misled because its own display stack is virtualised: it presents a
"Microsoft Remote Display Adapter" of its own, and the indirect output never
reached DXGI. A nested display stack is the wrong instrument for a question about
display topology, however convenient it is for questions about installation.

## Housekeeping: killing a stale sandbox
## Housekeeping: killing a stale sandbox

`Stop-Process` does not stop a sandbox. The VM outlives its window, keeps its
memory, and the next launch adds another one — two left running was enough to
have the host killed for memory. The VM is owned by the Hyper-V host service, so
it has to be asked properly, and only the sandbox ones:

```powershell
hcsdiag list                          # find compute systems by name
hcsdiag kill <id>                     # only where the name is WindowsSandbox
```

The `wslc` container on this machine (SearXNG) appears in the same list and must
be left alone.

## Why VDD rather than our own driver

The Virtual Display Driver is MIT licensed, IddCx 1.10, UMDF, and signed with a
timestamped certificate Windows accepts. That last part is the expensive one:
writing a driver is not hard, but loading an unsigned one needs either test
signing or, for distribution, an EV certificate and Microsoft attestation
signing, quite possibly WHQL. Sunshine takes the same route, telling users to
install a virtual display instead of shipping one.

The package it installs is the *driver only* release - an INF, a UMDF DLL and a
signed catalog - under `target/sandbox/vdd/`. `scripts/fetch-vdd.sh` checks the
signature rather than trusting the download, since being signed is the entire
reason for using it.

## What the sandbox cannot do

It cannot enable test signing, which is why that route was abandoned. There is no
`CmDiag` in the sandbox image (it is a host tool, and it configures every
container on the host), and a sandbox cannot open its own boot configuration
store: `bcdedit` answers "the requested system device cannot be found". Both were
tried rather than assumed.

It is also behind NAT, so the tablet cannot reach in and UDP discovery will not
cross. The useful test is a loopback one, host and client on the same machine.

## Caveats worth knowing

- VDD can become the primary display after a GPU driver update. Its README
  recommends uninstalling before major driver updates.
- The sandbox has no hardware video encoder, so the host falls back to software
  H.264 there. Fine for "does it capture", not for performance.
- `MemoryInMB` is 4 GB; raise it if the sandbox is killed while streaming.
