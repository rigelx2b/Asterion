#!/usr/bin/env bash
# Full-universe training on every cached symbol; the test split stays unscored.
set -u
BIN=${BIN:-data/bin/asterion-train.exe}
OUT=${OUT:-checkpoints/tiered_h192.json}
"$BIN" train --epochs "${EPOCHS:-40}" --hidden-dim "${HIDDEN:-192}" --batch-size 1024 --lr 0.002 \
  --eval-every 2 --patience 5 --model-out "$OUT" "$@"
echo "train_done exit=$?"
