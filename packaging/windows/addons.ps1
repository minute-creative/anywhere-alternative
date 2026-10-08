# Installs the free add-ons the setup page ticked, each from its maker:
# winget (Windows' own package manager) for ViGEmBus, usbip-win2 and
# Tailscale; VB-CABLE from vb-audio.com (it isn't on winget).
# Run by the installer as administrator:  addons.ps1 controllers mic tailscale
# Anything that fails is skipped; the app's Extras page offers it again.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Wanted)
$ErrorActionPreference = 'Continue'
$log = Join-Path $env:ProgramData 'AnywhereAlternative-setup\addons.log'
New-Item -ItemType Directory -Force -Path (Split-Path $log) | Out-Null
function Say($m) { "$(Get-Date -Format s)  $m" | Add-Content -Path $log }

function Winget($id, $extra = @()) {
    $wg = Get-Command winget.exe -ErrorAction SilentlyContinue
    if (-not $wg) { Say "winget not found; skipped $id"; return }
    Say "installing $id"
    & $wg.Source install --id $id --exact --silent --source winget `
        --accept-package-agreements --accept-source-agreements --disable-interactivity @extra 2>&1 |
        ForEach-Object { Say "  $_" }
    Say "$id finished (exit $LASTEXITCODE)"
}

$sys = Join-Path $env:SystemRoot 'System32\drivers'
if ($Wanted -contains 'controllers') {
    if (-not (Test-Path "$sys\ViGEmBus.sys")) { Winget 'ViGEm.ViGEmBus' }
    if (-not (Test-Path "$env:ProgramFiles\USBip\usbip.exe")) { Winget 'vadimgrn.usbip-win2' }
}
if ($Wanted -contains 'tailscale') {
    if (-not (Test-Path "$env:ProgramFiles\Tailscale\tailscale.exe")) {
        # Unattended: Tailscale stays connected before anyone signs in, so
        # the sign-in screen is reachable from another network.
        Winget 'Tailscale.Tailscale' @('--custom', 'TS_UNATTENDEDMODE=always')
    }
}
if ($Wanted -contains 'mic') {
    if (-not (Test-Path "$sys\vbaudio_cable64_win10.sys") -and -not (Test-Path "$sys\vbaudio_cable64_win7.sys")) {
        try {
            Say 'installing VB-CABLE'
            $page = Invoke-WebRequest -UseBasicParsing 'https://vb-audio.com/Cable/'
            $link = ($page.Links | Where-Object { $_.href -match 'VBCABLE_Driver_Pack\d+\.zip' } | Select-Object -First 1).href
            if (-not $link) { throw 'download link not found on vb-audio.com' }
            if ($link -notmatch '^https?://') { $link = "https://vb-audio.com/Cable/$link" }
            $dir = Join-Path $env:TEMP 'anywhere-vbcable'
            Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            Invoke-WebRequest -UseBasicParsing $link -OutFile "$dir\cable.zip"
            Expand-Archive "$dir\cable.zip" $dir -Force
            # -i install, -h hidden (VB-Audio's own switches).
            $p = Start-Process "$dir\VBCABLE_Setup_x64.exe" -ArgumentList '-i', '-h' -Wait -PassThru
            Say "VB-CABLE finished (exit $($p.ExitCode))"
        } catch { Say "VB-CABLE skipped: $_" }
    }
}
exit 0
