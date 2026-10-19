#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC="${PROJECT_S_MODEL_SRC:-$HOME/Documents/PROJECT-S/Settings/models/embeddinggemma-2}"
DEST="$REPO_ROOT/src-tauri/resources/models/embeddinggemma-2"
FILES=(
  model_quantized.onnx
  model_quantized.onnx_data
  tokenizer.json
  tokenizer_config.json
)

dest_complete=1
for file in "${FILES[@]}"; do
  if [ ! -f "$DEST/$file" ]; then
    dest_complete=0
  fi
done
if [ "$dest_complete" -eq 1 ]; then
  echo "embedding model already staged: $DEST"
  exit 0
fi

src_complete=1
for file in "${FILES[@]}"; do
  if [ ! -f "$SRC/$file" ]; then
    echo "error: missing $SRC/$file" >&2
    src_complete=0
  fi
done
if [ "$src_complete" -ne 1 ]; then
  echo "error: embedding model source is incomplete: $SRC" >&2
  echo "set PROJECT_S_MODEL_SRC to a directory containing: ${FILES[*]}" >&2
  exit 1
fi

mkdir -p "$DEST"
for file in "${FILES[@]}"; do
  cp -p "$SRC/$file" "$DEST/$file"
done
echo "staged embedding model: $SRC -> $DEST"
