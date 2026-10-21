# Stage the embedding model for a Windows build.
# Equivalent of scripts/stage-embed-model.sh; idempotent.
#
# Source: $env:PROJECT_S_MODEL_SRC, defaulting to the provisioned model under
# the user's Documents. Destination: src-tauri/resources/models/embeddinggemma-2.
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows/stage-embed-model.ps1

$ErrorActionPreference = "Stop"

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
if ($env:PROJECT_S_MODEL_SRC) {
    $Src = $env:PROJECT_S_MODEL_SRC
} else {
    $Src = Join-Path $HOME "Documents/PROJECT-S/Settings/models/embeddinggemma-2"
}
$Dest = Join-Path $RepoRoot "src-tauri/resources/models/embeddinggemma-2"
$Files = @(
    "model_quantized.onnx",
    "model_quantized.onnx_data",
    "tokenizer.json",
    "tokenizer_config.json"
)

$missing = @($Files | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Dest $_) -PathType Leaf) })
if ($missing.Count -eq 0) {
    Write-Host "embedding model already staged: $Dest"
    exit 0
}

$missingSrc = @($Files | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Src $_) -PathType Leaf) })
if ($missingSrc.Count -gt 0) {
    foreach ($file in $missingSrc) {
        Write-Host "error: missing $(Join-Path $Src $file)" -ForegroundColor Red
    }
    Write-Host "error: embedding model source is incomplete: $Src" -ForegroundColor Red
    Write-Host "set PROJECT_S_MODEL_SRC to a directory containing: $($Files -join ', ')" -ForegroundColor Red
    exit 1
}

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
foreach ($file in $Files) {
    Copy-Item -LiteralPath (Join-Path $Src $file) -Destination (Join-Path $Dest $file) -Force
}
Write-Host "staged embedding model: $Src -> $Dest"
