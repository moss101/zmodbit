#!/usr/bin/env bash
# Live M2 close-out (M2.11 → docs/51 E2E-001/002/003), daemon-driven.
#
# Drives the REAL modbit-core binary (socket + HTTP+SSE daemon + scheduler)
# over the surface protocol — never the runtime in process — against a real
# ts-webapp-style git fixture, real worktrees, the real execd broker, and a
# LIVE model provider. Evidence logs land in docs/evidence/ (secret-scanned;
# keys are never read into the logs).
#
# Two stages:
#   1. Scripted-model daemon E2E (always runs; proves the full machinery:
#      daemon → scheduler → worktree → tools → execd → repair loop).
#   2. Live-model daemon E2E (requires credentials; E2E-001/002/003 against
#      the production provider endpoint).
#
# Usage:
#   export OPENAI_API_KEY=sk-...                 # provider credential
#   export MODBIT_LIVE_MODEL=glm-4.5-flash       # optional model override
#   export OPENAI_BASE_URL=https://.../v4        # optional gateway override
#   ./tools/live_m2_close.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EVIDENCE_DIR="$ROOT/docs/evidence"
mkdir -p "$EVIDENCE_DIR"
STAMP="$(date -u +%Y-%m-%dT%H-%MZ)"  # colon-free: windows-safe path

fail() { echo "live-m2-close: BLOCKED — $*" >&2; exit 1; }

# 1. Machinery proof (scripted model fixture, no credentials needed).
echo "== M2.11 stage 1: scripted-model daemon E2E (machinery) =="
cargo test -p modbit-core-runtime --test daemon_scripted_e2e -- --test-threads=1 --nocapture \
  | tee "$EVIDENCE_DIR/m2-11-scripted-e2e.log"
grep -q "test result: ok" "$EVIDENCE_DIR/m2-11-scripted-e2e.log" \
  || fail "scripted daemon E2E did not pass — machinery is broken; not running live stage."

# 2. Live proof (real provider). Requires credentials by docs/15 § Live
#    provider proof; without them this stage is SKIPPED and M2 stays below
#    E2E_PROVEN.
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "live-m2-close: OPENAI_API_KEY not set — live stage SKIPPED (docs/15 live proof pending credentials)."
  exit 0
fi
[[ -n "${MODBIT_LIVE_E2E:-}" ]] || export MODBIT_LIVE_E2E=1
# Live reasoning models spend turns exploring; 8 (the local default) runs
# out mid-repair (observed: agent fixed code + tests and hit the cap while
# diagnosing the runner itself).
[[ -n "${MODBIT_MAX_TURNS:-}" ]] || export MODBIT_MAX_TURNS=24

echo "== M2.11 stage 2: live-model daemon E2E (E2E-001/002/003) =="
cargo test -p modbit-core-runtime --test daemon_live_e2e -- --test-threads=1 --nocapture \
  | tee "$EVIDENCE_DIR/m2-11-live-e2e-$STAMP.log"
grep -q "test result: ok" "$EVIDENCE_DIR/m2-11-live-e2e-$STAMP.log" \
  || fail "live daemon E2E did not pass — M2 stays below E2E_PROVEN."

# 3. Secret scan before anything is committed.
if grep -rEn "sk-[A-Za-z0-9]{20,}|Bearer [A-Za-z0-9]{20,}" "$EVIDENCE_DIR/m2-11-live-e2e-$STAMP.log"; then
  fail "credential-like string found in evidence log — refusing to record."
fi

echo "live-m2-close: OK — evidence in $EVIDENCE_DIR/m2-11-*.log"
