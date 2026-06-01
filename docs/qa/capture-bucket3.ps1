# capture-bucket3.ps1 — launch the updated wylde-gui and capture the three
# live-display QA screenshots for the 2026-05-30 E2E follow-up.
#
# No elevation required (no UAC). Run from anywhere:
#   powershell -ExecutionPolicy Bypass -File docs\qa\capture-bucket3.ps1
#
# It launches the freshly-built debug binary, then walks you through the three
# scenarios. At each prompt, set the window up as described, press Enter, and it
# saves a full-screen PNG into this folder (docs\qa\).

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)   # ...\Wylde
$qaDir = $PSScriptRoot
$exe = Join-Path $repo 'Core\GUI\target\debug\wylde-gui.exe'

if (-not (Test-Path $exe)) {
    Write-Host "Binary not found at $exe" -ForegroundColor Red
    Write-Host "Build it first:  cargo build -p wylde-gui   (from Core\GUI)" -ForegroundColor Yellow
    exit 1
}

function Save-Screen([string]$name) {
    $bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
    $bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $gfx = [System.Drawing.Graphics]::FromImage($bmp)
    $gfx.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $path = Join-Path $qaDir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $gfx.Dispose(); $bmp.Dispose()
    Write-Host "  saved $path" -ForegroundColor Green
}

Write-Host "Launching wylde-gui (debug, with the bucket-3 patches)..." -ForegroundColor Cyan
$proc = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 4
Write-Host "If the window is blank, the harness/lifecycle daemons may not be up." -ForegroundColor DarkGray
Write-Host "Launch the full stack (launch_wylde.ps1) for a populated UI; the" -ForegroundColor DarkGray
Write-Host "layout behaviours below render regardless of backend data." -ForegroundColor DarkGray
Write-Host ""

Write-Host "[a] PASTE WRAP — open Chat. Paste ~10k chars with NO newline into the" -ForegroundColor White
Write-Host "    prompt (clipboard is pre-loaded for you). The text must WRAP inside" -ForegroundColor White
Write-Host "    the bar; the window width must NOT change." -ForegroundColor White
[System.Windows.Forms.Clipboard]::SetText(('x' * 10000))
Write-Host "    (10k 'x' chars are on your clipboard — Ctrl+V in the prompt.)" -ForegroundColor DarkGray
Read-Host "    Set it up, then press Enter to capture"
Save-Screen 'bucket3-a-paste.png'

Write-Host ""
Write-Host "[b] FOCUS RESTORE — open the model pill, click a model. WITHOUT clicking" -ForegroundColor White
Write-Host "    anywhere, type a few chars — they should land in the prompt." -ForegroundColor White
Read-Host "    Do it, then press Enter to capture"
Save-Screen 'bucket3-b-focus.png'

Write-Host ""
Write-Host "[c] RESIZE MID-STREAM — start a turn; while tokens stream, drag the" -ForegroundColor White
Write-Host "    window narrower then wider. Bubbles should reflow; tokens keep" -ForegroundColor White
Write-Host "    arriving without truncation or freeze." -ForegroundColor White
Read-Host "    Do it (capture while streaming), then press Enter to capture"
Save-Screen 'bucket3-c-resize.png'

Write-Host ""
Write-Host "Done. Three PNGs are in docs\qa\. Leave wylde-gui running or close it." -ForegroundColor Cyan
Write-Host "(launched pid $($proc.Id))" -ForegroundColor DarkGray
