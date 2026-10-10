<#
  Builds the signed sparse MSIX that gives raze_shell.dll a package identity, which is
  what Windows 11 needs to show "RAZE" in the new right-click menu.

  Needs the Windows SDK (makeappx.exe, signtool.exe) and a release build:
      cargo build --release --workspace
      packaging\build-package.ps1

  Output: target\<Configuration>\RAZE.msix and RAZE.cer. Both are copied next to raze.exe by
  `raze --install` and registered by `raze --register-package`.

  The package is signed with a self-signed certificate (CN=RERCON RAZE) whose private key stays,
  non-exportable, in the current user's store. RAZE pins that certificate's thumbprint in
  src\shell\install.rs; if the certificate is ever recreated the pin has to be updated and the new
  certificate trusted again (one UAC prompt).
#>
param([string]$Configuration = 'release')
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$outDir = Join-Path $root "target\$Configuration"
$stage = Join-Path $root 'target\package-stage'
$subject = 'CN=RERCON RAZE'

# --- tools --------------------------------------------------------------------------------------
$bin = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Directory -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '^10\.' -and (Test-Path "$($_.FullName)\x64\makeappx.exe") -and (Test-Path "$($_.FullName)\x64\signtool.exe") } |
  Sort-Object Name -Descending | Select-Object -First 1
if (-not $bin) { throw 'Windows SDK signing tools (makeappx.exe, signtool.exe) were not found.' }
$makeappx = "$($bin.FullName)\x64\makeappx.exe"
$signtool = "$($bin.FullName)\x64\signtool.exe"

# --- version: Cargo "0.1.0" -> package "0.1.0.0" ----------------------------------------------
$match = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"' | Select-Object -First 1
if (-not $match) { throw 'Cannot read the package version from Cargo.toml.' }
$version = '{0}.{1}.{2}.0' -f $match.Matches[0].Groups[1].Value, $match.Matches[0].Groups[2].Value, $match.Matches[0].Groups[3].Value

# --- certificate --------------------------------------------------------------------------------
$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $subject -and $_.HasPrivateKey } | Select-Object -First 1
if (-not $cert) {
  $cert = New-SelfSignedCertificate -Type Custom -Subject $subject -KeyUsage DigitalSignature `
    -FriendlyName 'RAZE package signing' -CertStoreLocation 'Cert:\CurrentUser\My' -KeyExportPolicy NonExportable `
    -NotAfter (Get-Date).AddYears(5) -TextExtension @('2.5.29.37={text}1.3.6.1.5.5.7.3.3', '2.5.29.19={text}')
  Write-Host "created certificate $($cert.Thumbprint)"
}
$pinned = (Select-String -Path (Join-Path $root 'src\shell\install.rs') -Pattern 'PINNED_CERT_THUMBPRINT: &str = "([0-9A-F]{40})"' | Select-Object -First 1).Matches[0].Groups[1].Value
if ($pinned -ne $cert.Thumbprint) {
  throw "Certificate $($cert.Thumbprint) differs from PINNED_CERT_THUMBPRINT ($pinned) in src\shell\install.rs. Update the constant and rebuild raze.exe."
}

# --- stage, pack, sign --------------------------------------------------------------------------
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
(Get-Content (Join-Path $PSScriptRoot 'AppxManifest.xml') -Raw -Encoding UTF8).Replace('@VERSION@', $version) |
  Set-Content (Join-Path $stage 'AppxManifest.xml') -Encoding UTF8

Add-Type -AssemblyName System.Drawing
$source = [System.Drawing.Image]::FromFile((Join-Path $root 'icons\icon-app.png'))
$logo = New-Object System.Drawing.Bitmap 150, 150
$g = [System.Drawing.Graphics]::FromImage($logo)
$g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
$g.DrawImage($source, 0, 0, 150, 150); $g.Dispose()
$logo.Save((Join-Path $stage 'logo.png'), [System.Drawing.Imaging.ImageFormat]::Png); $logo.Dispose(); $source.Dispose()

New-Item -ItemType Directory -Force $outDir | Out-Null
$msix = Join-Path $outDir 'RAZE.msix'
Remove-Item $msix -ErrorAction SilentlyContinue
# /nv: a sparse package does not contain the executable it points at, so semantic validation is skipped.
& $makeappx pack /d $stage /p $msix /nv | Out-Null
if ($LASTEXITCODE) { throw "makeappx failed ($LASTEXITCODE)" }
& $signtool sign /fd SHA256 /sha1 $cert.Thumbprint $msix | Out-Null
if ($LASTEXITCODE) { throw "signtool failed ($LASTEXITCODE)" }
Export-Certificate -Cert $cert -FilePath (Join-Path $outDir 'RAZE.cer') -Force | Out-Null

Write-Host "package $msix (version $version)"
Write-Host "certificate $($cert.Thumbprint)"
