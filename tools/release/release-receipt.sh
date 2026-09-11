#!/usr/bin/env bash
# Phase 4.4: the release receipt — the final artifact of the release
# flow. Records build/provenance/signature-verification outcomes as a
# signed-state receipt. Usage: release-receipt.sh <bundle-dir> > receipt.json
set -euo pipefail
BUNDLE="${1:?usage: release-receipt.sh <bundle-dir>}"
cd "$BUNDLE"
# Production signature = Developer ID (macOS) / Authenticode (Windows).
# Ad-hoc local signatures do NOT count (see operator-signing.md).
SIGNED="false"
if command -v codesign >/dev/null 2>&1    && codesign --verify --strict bin/modbit-core 2>/dev/null    && codesign -dv bin/modbit-core 2>&1 | grep -q "Authority=Developer ID Application"; then
  SIGNED="true"
elif command -v signtool >/dev/null 2>&1   && signtool verify /pa bin/modbit-core.exe >/dev/null 2>&1; then
  SIGNED="true"
fi
cat <<EOF
{
  "bundle": "$(pwd)",
  "sha256sums": "$(shasum -a 256 SHA256SUMS | awk '{print $1}')",
  "provenance": $(cat provenance.json),
  "signature_verified": $SIGNED,
  "release_gate": "$( [ "$SIGNED" = "true" ] && echo PRODUCTION_OK || echo BLOCKED_EXTERNAL_CREDENTIAL )"
}
