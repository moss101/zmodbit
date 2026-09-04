#!/usr/bin/env bash
# Live M2 close-out (M2.6 → M2.7), docs/15 § Live provider proof.
#
# Runs the env-gated live qualifications against the production provider
# endpoint and captures auditable evidence logs (keys are NEVER read into
# the logs — output only). After both pass, the graph chain M2.6 → M2.7 →
# M2.8 → M2.9 → M2.10 → M2 COMPLETE is unblocked.
#
# Usage:
#   export OPENAI_API_KEY=sk-...      # or ANTHROPIC_API_KEY=sk-ant-...
#   export MODBIT_LIVE_OPENAI=1       # or MODBIT_LIVE_ANTHROPIC=1
#   ./tools/live_m2_close.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="$ROOT/docs/evidence"
mkdir -p "$EVIDENCE_DIR"

fail() { echo "live-m2-close: BLOCKED — $*" >&2; exit 1; }

# 1. Credentials present?
if [[ -z "${OPENAI_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]; then
  fail "no provider key in env. Export OPENAI_API_KEY or ANTHROPIC_API_KEY (never committed) and retry."
fi
if [[ -z "${MODBIT_LIVE_OPENAI:-}" && -z "${MODBIT_LIVE_ANTHROPIC:-}" ]]; then
  fail "live mode not enabled. Export MODBIT_LIVE_OPENAI=1 or MODBIT_LIVE_ANTHROPIC=1 and retry."
fi

# 2. M2.6 — provider gateway live streaming qualification.
echo "== M2.6: provider gateway live streaming qualification =="
cargo test -p modbit-providers --test live_qualification -- --nocapture \
  | tee "$EVIDENCE_DIR/m2-6-live-qualification.log"
grep -q "test result: ok" "$EVIDENCE_DIR/m2-6-live-qualification.log" \
  || fail "M2.6 qualification log does not show ok — not recording evidence."

# 3. M2.7 — one-agent runtime live e2e through the same endpoint.
echo "== M2.7: one-agent runtime live e2e =="
cargo test -p modbit-core-runtime --test live_one_agent -- --nocapture \
  | tee "$EVIDENCE_DIR/m2-7-live-one-agent.log"
grep -q "test result: ok" "$EVIDENCE_DIR/m2-7-live-one-agent.log" \
  || fail "M2.7 live e2e log does not show ok — not recording evidence."

SHA="$(git -C "$ROOT" rev-parse --short HEAD)"
echo
echo "live-m2-close: both live qualifications PASSED."
echo "Evidence logs: docs/evidence/m2-6-live-qualification.log, docs/evidence/m2-7-live-one-agent.log"
echo "Next: commit the evidence logs, then with their commit SHA record:"
echo "  python3 tools/graph.py set M2.6 COMPLETE --evidence 'commit:<sha>' --note 'live streaming qualification per docs/15'"
echo "  python3 tools/graph.py set M2.7 COMPLETE --evidence 'commit:<sha>' --note 'live one-agent e2e per docs/14+15'"
echo "  python3 tools/graph.py set M2.8 COMPLETE --evidence 'run:<3-OS CI run url>' --evidence 'commit:<sha>'"
echo "  python3 tools/graph.py set M2.9 COMPLETE --evidence 'run:<3-OS CI run url>' --evidence 'commit:<sha>'"
echo "  python3 tools/graph.py set M2.10 COMPLETE --evidence 'run:<3-OS CI run url>' --evidence 'commit:<sha>'"
echo "  (then M2 closes automatically when all M2 work items are COMPLETE)"
