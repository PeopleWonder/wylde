# Phase 8.3 cleanup re-implementation — same target list as
# _phase8_3_cleanup.bat, but in PowerShell so we don't hit the
# cmd parser bug ("... was unexpected at this time" caused by an
# unescaped paren inside the bat's parenthesised echo block).

$ErrorActionPreference = 'Continue'
Set-Location -LiteralPath '%USERPROFILE%\Documents\Obsidian Vault\Wylde'

$Log = Join-Path (Get-Location) '_phase8_3_cleanup_output.log'
"=== Phase 8.3 cleanup (PowerShell wrapper) ===" | Set-Content -LiteralPath $Log -Encoding utf8
"CWD: $(Get-Location)"                            | Add-Content -LiteralPath $Log
"Started: $(Get-Date -Format o)"                  | Add-Content -LiteralPath $Log
""                                                | Add-Content -LiteralPath $Log

function Remove-Target {
    param([string]$Path, [switch]$Recursive)
    if (Test-Path -LiteralPath $Path) {
        try {
            if ($Recursive) {
                Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction Stop
            } else {
                Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            }
            if (Test-Path -LiteralPath $Path) {
                "  FAIL  $Path  (still exists after delete)" | Add-Content -LiteralPath $Log
                return $false
            } else {
                "  OK    $Path" | Add-Content -LiteralPath $Log
                return $true
            }
        } catch {
            "  FAIL  $Path  ($_)" | Add-Content -LiteralPath $Log
            return $false
        }
    } else {
        "  SKIP  $Path  (not present)" | Add-Content -LiteralPath $Log
        return $false
    }
}

"[1/3] Remove N8N\templates (must be empty)..." | Add-Content -LiteralPath $Log
$tmpl = 'N8N\templates'
if (Test-Path -LiteralPath $tmpl) {
    $kids = Get-ChildItem -LiteralPath $tmpl -Force -ErrorAction SilentlyContinue
    if ($kids) {
        "  FAIL  N8N\templates not empty:" | Add-Content -LiteralPath $Log
        $kids | ForEach-Object { "    $($_.Name)" | Add-Content -LiteralPath $Log }
    } else {
        Remove-Target -Path $tmpl | Out-Null
    }
} else {
    "  SKIP  N8N\templates  (not present)" | Add-Content -LiteralPath $Log
}
"" | Add-Content -LiteralPath $Log

"[2/3] Remove vault-root standalone smoke wrappers..." | Add-Content -LiteralPath $Log
foreach ($f in @(
    '_phase8_3_n8n_smoke.bat',
    '_phase8_3_n8n_smoke.ps1',
    '_phase8_3_n8n_smoke_check.py',
    '_phase8_3_n8n_smoke_output.log'
)) {
    Remove-Target -Path $f | Out-Null
}
"" | Add-Content -LiteralPath $Log

"[3/3] Remove N8N\_n8n_service_merge recursively..." | Add-Content -LiteralPath $Log
Remove-Target -Path 'N8N\_n8n_service_merge' -Recursive | Out-Null
"" | Add-Content -LiteralPath $Log

"=== Cleanup done ===" | Add-Content -LiteralPath $Log
"Finished: $(Get-Date -Format o)" | Add-Content -LiteralPath $Log

Get-Content -LiteralPath $Log | Write-Host
exit 0
