#!/usr/bin/env bash
# Run once a day after 18:00 KST: refresh prices and context, collect investor flows,
# then record and score the frozen model's forward-test ledger.
set -u
BIN=${BIN:-data/bin/asterion-daily.exe}
MODEL=${MODEL:-checkpoints/final_model.json}
RANK_BIN=${RANK_BIN:-data/bin/asterion-rank.exe}
# Must include buffer support (rank-score refuses buffer entries written without it).
BUFFER_BIN=${BUFFER_BIN:-data/bin/asterion-buffer-ledger.exe}
BUFFER_DIR=${BUFFER_DIR:-data/ledger/buffer20_2026-11-22}
LOG=data/logs/daily-$(date +%Y%m%d).log
{
  "$BIN" fetch --universe config/universe/kr-union.txt --from 2020-01-01 || echo "fetch_failed exit=$?"
  "$BIN" fetch-index --from 2020-01-01 || echo "fetch_index_failed exit=$?"
  "$BIN" fetch-macro --from 2015-01-01 || echo "fetch_macro_failed exit=$?"
  # The model's disclosure and fundamental features go stale without these two.
  "$BIN" fetch-disclosures --from 2015-01-01 || echo "fetch_disclosures_failed exit=$?"
  "$BIN" fetch-financials || echo "fetch_financials_failed exit=$?"
  "$BIN" fetch-investors --universe config/universe/kr-union.txt || echo "fetch_investors_failed exit=$?"
  "$RANK_BIN" fetch-overseas --from 2010-01-01 || echo "fetch_overseas_failed exit=$?"
  "$BIN" ledger-record --model "$MODEL" || echo "ledger_record_failed exit=$?"
  "$BIN" ledger-score --model "$MODEL" || echo "ledger_score_failed exit=$?"
  # Paper-only ranking-arm ledger registered 2026-11-13 (see data/ledger/rank4_2026-11-13).
  "$RANK_BIN" rank-record --universe config/universe/kr-union.txt || echo "rank_record_failed exit=$?"
  "$RANK_BIN" rank-score || echo "rank_score_failed exit=$?"
  # Forward-only paper ledger of the 20% holding buffer (README pre-registration 2026-11-22).
  if [ -f "$BUFFER_DIR/hypothesis.json" ]; then
    "$BUFFER_BIN" rank-record --universe config/universe/kr-union.txt --dir "$BUFFER_DIR" || echo "buffer_record_failed exit=$?"
    "$BUFFER_BIN" rank-score --dir "$BUFFER_DIR" || echo "buffer_score_failed exit=$?"
  fi
  echo "daily_done"
} >> "$LOG" 2>&1
tail -12 "$LOG"
