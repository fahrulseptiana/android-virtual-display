<#
    Installs the signed Virtual Display Driver on this machine and configures the
    monitor it should create.

    Run elevated. Phase 7 uses VDD rather than writing a driver of our own: it is
    MIT licensed, IddCx 1.10, UMDF, and signed with a certificate Windows accepts,
    so none of this needs test signing.

    The order matters, and each step is here because leaving it out produced a
    different failure:

      * vdd_settings.xml goes to C:\VirtualDisplayDriver\ *first*. The driver
        reads its configuration from there at start-up; beside the INF is not
        looked at. Without it the device installs, reports OK, and creates no
        monitor at all -- which looks exactly like a monitor that cannot be
        captured.
      * The catalog's certificates are trusted, because the INF sets
        PnpLockdown=1.
      * pnputil stages the package. It does not create the device node, because
        the INF is root-enumerated (Root\MttVDD).
      * devcon creates the node.

    Then reboot: a display driver's monitor is enumerated at boot, and this one
    does not appear without it.

    -Uninstall reverses it. Worth reading before installing: VDD's own notes warn
    that Windows may promote a monitor with nothing attached, which scrambles
    display priority and can leave the real screen black after a GPU driver
    update. Safe Mode and Device Manager recover from that.

    ASCII only: Windows PowerShell reads a .ps1 as ANSI unless it has a byte order
    mark, and a stray em dash becomes 0x94, which it reads as a quote.
#>
param(
    # Where the extracted driver package lives. scripts/fetch-vdd.sh fills this
    # in on the sandbox branch; this script downloads it if it is not there.
    [string] $PackagePath = 'target\sandbox\vdd\VirtualDisplayDriver',

    # Removes the device and the driver package.
    [switch] $Uninstall,

    # The release to fetch, pinned rather than "latest" so a run is repeatable.
    [string] $Release = '25.7.23'
)

$ErrorActionPreference = 'Stop'

# Where the driver looks for its settings, per the project README.
# Resolved from the script's own location rather than the working directory.
# Elevated, the working directory is System32, so a relative path quietly
# downloads the driver package into System32 -- which is litter, in a system
# directory, and was cleaned up by hand once.
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $PackagePath) {
    $PackagePath = Join-Path $repoRoot 'target\sandbox\vdd\VirtualDisplayDriver'
}
$dist = Join-Path $repoRoot 'target\sandbox'

$configDir = 'C:\VirtualDisplayDriver'
$hardwareId = 'Root\MttVDD'

function Require-Elevation {
    $elevated = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $elevated) {
        throw 'This needs to run elevated: it installs a driver.'
    }
}

function Get-Devcon {
    $local = Join-Path $dist 'devcon.exe'
    if (Test-Path $local) { return (Resolve-Path $local).Path }

    # Ship-with-the-WDK tool; it links no C runtime, so it runs as it is.
    $found = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Tools' -Recurse -Filter 'devcon.exe' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Select-Object -First 1
    if ($found) { return $found.FullName }

    throw 'devcon.exe not found. Install the Windows Driver Kit, or copy it to target\sandbox\.'
}

function Get-Package {
    param([string] $Path)

    if (Test-Path (Join-Path $Path 'MttVDD.inf')) { return (Resolve-Path $Path).Path }

    $url = "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases/download/$Release/VirtualDisplayDriver-x86.Driver.Only.zip"
    Write-Host "fetching $url"
    $zip = Join-Path $env:TEMP 'vdd-driver.zip'
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing

    $target = Split-Path -Parent $Path
    New-Item -Path $target -ItemType Directory -Force | Out-Null
    Expand-Archive -Path $zip -DestinationPath $target -Force
    Remove-Item $zip -Force

    $inf = Get-ChildItem $target -Filter 'MttVDD.inf' -Recurse | Select-Object -First 1
    if (-not $inf) { throw "no MttVDD.inf under $target" }
    return $inf.Directory.FullName
}

# Writes the mode list the virtual monitor should offer. The tablet is
# 1200x1920, so its landscape size comes first: a monitor created at the size the
# client actually has is what removes the 23 ms 4K downscale from the pipeline.
function Write-Settings {
    $settings = @'
<?xml version='1.0' encoding='utf-8'?>
<vdd_settings>
    <monitors>
        <count>1</count>
    </monitors>
    <gpu>
        <friendlyname>default</friendlyname>
    </gpu>
    <global>
        <g_refresh_rate>60</g_refresh_rate>
    </global>
    <resolutions>
        <resolution>
            <width>1920</width>
            <height>1200</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
        <resolution>
            <width>1200</width>
            <height>1920</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
        <resolution>
            <width>1920</width>
            <height>1080</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
        <resolution>
            <width>1280</width>
            <height>800</height>
            <refresh_rate>60</refresh_rate>
        </resolution>
    </resolutions>
</vdd_settings>
'@

    New-Item -Path $configDir -ItemType Directory -Force | Out-Null
    $settings | Set-Content -Path (Join-Path $configDir 'vdd_settings.xml') -Encoding UTF8
    Write-Host "settings: $configDir\vdd_settings.xml"
}

function Trust-Catalog {
    param([string] $Folder)

    $cat = Get-ChildItem $Folder -Filter '*.cat' -Recurse | Select-Object -First 1
    if (-not $cat) { throw "no .cat in $Folder" }

    $certs = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2Collection
    $certs.Import([System.IO.File]::ReadAllBytes($cat.FullName))
    foreach ($cert in $certs) {
        $path = Join-Path $env:TEMP "$($cert.Thumbprint).cer"
        [System.IO.File]::WriteAllBytes(
            $path,
            $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert))
        Import-Certificate -FilePath $path -CertStoreLocation 'Cert:\LocalMachine\TrustedPublisher' | Out-Null
        Remove-Item $path -Force
    }
    Write-Host "trusted $($certs.Count) publisher certificate(s) from $($cat.Name)"
}

function Show-State {
    $adapters = Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
        ForEach-Object { "$($_.FriendlyName) [$($_.Status)]" }
    Write-Host "display adapters: $($adapters -join '; ')"

    $monitors = Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like '*MTT1337*' } |
        ForEach-Object { "$($_.FriendlyName) [$($_.Status)]" }
    if ($monitors) {
        Write-Host "virtual monitors: $($monitors -join '; ')"
    } else {
        Write-Host 'virtual monitors: none yet'
    }
}

Require-Elevation

if ($Uninstall) {
    $devcon = Get-Devcon
    Write-Host "removing the device node $hardwareId"
    & $devcon remove $hardwareId 2>&1 | ForEach-Object { "  $_" }

    foreach ($inf in (Get-ChildItem $dist -Filter 'MttVDD.inf' -Recurse -ErrorAction SilentlyContinue)) {
        Write-Host "deleting the driver package $($inf.Name)"
        pnputil /delete-driver $inf.Name /uninstall 2>&1 | ForEach-Object { "  $_" }
    }

    if (Test-Path $configDir) {
        Write-Host "removing $configDir"
        Remove-Item $configDir -Recurse -Force
    }

    Show-State
    Write-Host 'Reboot to finish removing it.'
    exit 0
}

$package = Get-Package -Path $PackagePath
$inf = Get-ChildItem $package -Filter 'MttVDD.inf' | Select-Object -First 1

# Before the driver loads, because it reads this at start-up.
Write-Settings
Trust-Catalog -Folder $package

Write-Host "staging $($inf.FullName)"
pnputil /add-driver $inf.FullName /install 2>&1 | ForEach-Object { "  $_" }

# Staging is not installing: the INF is root-enumerated.
$devcon = Get-Devcon
Write-Host "creating the device node $hardwareId"
& $devcon install $inf.FullName $hardwareId 2>&1 | ForEach-Object { "  $_" }

Show-State
Write-Host ''
Write-Host 'Reboot now. The monitor is enumerated at boot and will not appear before it.'
