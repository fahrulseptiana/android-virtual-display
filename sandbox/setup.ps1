<#
    Runs inside Windows Sandbox on every logon.

    On the first logon it installs the signed Virtual Display Driver and restarts
    the sandbox, because a display driver needs a boot before Windows enumerates
    the monitor it describes. State survives a restart begun inside the sandbox
    (Windows 11 22H2 and later), and the marker file is what stops the second
    logon from restarting again, which would otherwise loop forever. The marker
    lives on the sandbox's own disk, not in the mapped repository, so a fresh
    sandbox installs again rather than assuming someone else's work.

    Details that matter, each learned the hard way:

    * The driver reads its configuration from a fixed location,
      C:\VirtualDisplayDriver\vdd_settings.xml, not from beside the INF. Without
      it there the device installs and reports OK while creating no monitor at
      all, which is indistinguishable from a monitor DXGI cannot see.
    * pnputil only stages a package. The INF is root-enumerated (Root\MttVDD), so
      the device node has to be created, which devcon does and pnputil will not.
    * The signer certificates are imported into TrustedPublisher, as the project's
      own installer does, because the INF sets PnpLockdown=1.
    * A monitor Windows has not activated has no desktop on it, so extend onto
      everything before asking DXGI what it can see.

    No test signing is involved. VDD is signed with a certificate Windows
    accepts, which the sandbox could not have arranged for itself: it has no
    CmDiag in its image, and it cannot open its own boot configuration store.

    ASCII only: Windows PowerShell reads a .ps1 as ANSI unless it has a byte
    order mark, and a stray em dash becomes 0x94, which it reads as a quote.
#>

$ErrorActionPreference = 'Continue'

# Written into the mapped repo so it can be read from the host afterwards.
$logPath = 'C:\vd\sandbox\setup.log'
$dist = 'C:\vd\target\sandbox'
$driver = "$dist\vdd"
$marker = 'C:\vdd-installed'
# Where the driver looks for its settings, per the project README.
$configDir = 'C:\VirtualDisplayDriver'

function Note {
    param([string] $Message)
    $line = '[{0}] {1}' -f (Get-Date -Format 'HH:mm:ss'), $Message
    Write-Host $line
    Add-Content -Path $logPath -Value $line -ErrorAction SilentlyContinue
}

function Flat {
    param($Value)
    if ($null -eq $Value) { return '' }
    return (($Value | Out-String).Trim() -replace '\s+', ' ')
}

function Show-Displays {
    param([string] $When)
    $adapters = Get-CimInstance Win32_VideoController -ErrorAction SilentlyContinue |
        ForEach-Object { "$($_.Name) $($_.CurrentHorizontalResolution)x$($_.CurrentVerticalResolution)" }
    Note "adapters $When`: $(Flat ($adapters -join '; '))"

    # Monitors rather than adapters: an indirect display can be present as a
    # device and still have created no monitor, and MTT1337 is how VDD's own
    # scripts identify theirs.
    $monitors = Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue |
        ForEach-Object { "$($_.FriendlyName) [$($_.InstanceId)] $($_.Status)" }
    Note "monitors $When`: $(Flat ($monitors -join '; '))"
}

# Imports whatever the catalog was signed with, so that PnpLockdown=1 is happy.
function Trust-Catalog {
    param([string] $Folder)
    $cat = Get-ChildItem -Path $Folder -Filter '*.cat' -Recurse | Select-Object -First 1
    if (-not $cat) {
        Note "no .cat in $Folder"
        return
    }

    $certs = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2Collection
    $certs.Import([System.IO.File]::ReadAllBytes($cat.FullName))
    foreach ($cert in $certs) {
        $path = Join-Path $env:TEMP "$($cert.Thumbprint).cer"
        [System.IO.File]::WriteAllBytes(
            $path,
            $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert))
        Import-Certificate -FilePath $path -CertStoreLocation 'Cert:\LocalMachine\TrustedPublisher' | Out-Null
    }
    Note "trusted $($certs.Count) publisher certificate(s) from $($cat.Name)"
}

Note '--- sandbox logon ---'

$admin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
Note "administrator=$admin  test signing not needed, VDD is signed"

if (-not (Test-Path $marker)) {
    Show-Displays 'before'

    if (-not (Test-Path $driver)) {
        Note "no driver package in $driver; run scripts/fetch-vdd.sh on the host"
        Note '--- nothing to install ---'
        exit
    }

    # A local copy: installing a driver from a mapped drive is asking for
    # trouble, and the mapped folder is writable by anything in here.
    $local = 'C:\vdd'
    New-Item -Path $local -ItemType Directory -Force | Out-Null
    Copy-Item -Path "$driver\*" -Destination $local -Recurse -Force

    # Recursive: the release extracts into a folder of its own.
    $inf = Get-ChildItem -Path $local -Filter '*.inf' -Recurse | Select-Object -First 1
    if (-not $inf) {
        Note "no .inf under $local"
        Note '--- nothing to install ---'
        exit
    }

    # Before the driver loads, because it reads this at start-up.
    $settings = Get-ChildItem -Path $local -Filter 'vdd_settings.xml' -Recurse | Select-Object -First 1
    New-Item -Path $configDir -ItemType Directory -Force | Out-Null
    if ($settings) {
        Copy-Item $settings.FullName (Join-Path $configDir 'vdd_settings.xml') -Force
        Note "settings: $configDir\vdd_settings.xml"
    } else {
        Note "no vdd_settings.xml in the package; the driver will use its defaults"
    }

    Trust-Catalog -Folder $local

    Note "staging $($inf.Name)"
    $staged = pnputil.exe /add-driver $inf.FullName /install 2>&1
    Note "pnputil: code=$LASTEXITCODE $(Flat $staged)"

    $devcon = "$dist\devcon.exe"
    if (Test-Path $devcon) {
        $created = & $devcon install $inf.FullName 'Root\MttVDD' 2>&1
        Note "devcon install: code=$LASTEXITCODE $(Flat $created)"
    } else {
        Note "no devcon.exe in $dist; the driver is staged but no display will appear"
    }

    New-Item -Path $marker -ItemType File -Force | Out-Null
    Note 'restarting so Windows enumerates the new display'
    Start-Sleep -Seconds 3
    shutdown.exe /r /t 0
    exit
}

Note 'Driver was installed on an earlier logon; reporting the result.'
Show-Displays 'after reboot'

if (-not (Test-Path "$dist\vd-host.exe")) {
    Note "no host build in $dist; run scripts/build-sandbox.sh on the host"
    Note '--- ready ---'
    exit
}

# A monitor Windows has not activated has no desktop on it and would be absent
# for that reason rather than an interesting one, so extend onto everything
# first. What is being tested is whether DXGI can *reach* an indirect display,
# not whether it happens to be switched on.
$switch = "$env:WINDIR\System32\DisplaySwitch.exe"
if (Test-Path $switch) {
    Start-Process -FilePath $switch -ArgumentList '/extend' -Wait -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 3
}
Show-Displays 'after extend'

# One short run rather than starting a session: the host lists every adapter and
# every output, and says whether duplication works on each. DXGI enumerates
# outputs per adapter, so a monitor added by an indirect display driver can be
# invisible to anything that only looks at the GPU, and this is how that is seen
# rather than assumed.
$env:VD_LIST_OUTPUTS = '1'
$outputs = "$env:TEMP\vd-outputs.out"
$errors = "$env:TEMP\vd-outputs.err"
Remove-Item $outputs, $errors -ErrorAction SilentlyContinue

$process = Start-Process -FilePath "$dist\vd-host.exe" -PassThru -NoNewWindow `
    -RedirectStandardOutput $outputs -RedirectStandardError $errors
if (-not $process.WaitForExit(30000)) { $process.Kill() }

$seen = Get-Content $outputs -ErrorAction SilentlyContinue
if ($seen) {
    Note 'what DXGI can see:'
    foreach ($line in $seen) { Note "  $line" }
} else {
    Note 'the host printed no outputs'
}
$problem = Flat (Get-Content $errors -ErrorAction SilentlyContinue)
if ($problem) { Note "  stderr: $problem" }

Note '--- ready ---'
