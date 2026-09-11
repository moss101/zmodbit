# Operator signing gate (Phase 4.4 item 4 — BLOCKED_EXTERNAL_CREDENTIAL / RELEASE_GATE)

Production signing is an OPERATOR step on an operator-controlled machine.
No Apple Developer private credentials, notarization secrets, or Windows
signing private keys may ever enter the agent runtime, this repository, or
CI. The release flow:

```
build → test → package (tools/release/package.sh)
      → SBOM/hash/provenance (same script; verify with verify-bundle.sh)
      → OPERATOR SIGNING (this document)
      → notarization/signature verification (tools/release/verify-bundle.sh)
      → release receipt (tools/release/release-receipt.sh)
```

## macOS (operator machine, signed in to Xcode with Developer ID)

```bash
cd dist/modbit-<version>-<target>
codesign --force --options runtime --timestamp \
  --sign "Developer ID Application: <NAME> (<TEAMID>)" bin/modbit-core bin/modbit bin/modbit-execd
# Notarize and staple (requires an App Store Connect API key in the
# operator's environment, NEVER in the repo):
xcrun notarytool submit <zip of bundle> --keychain-profile "AC_NOTARY" --wait
xcrun stapler staple bin/modbit-core
```

## Windows (operator machine with the signing certificate installed)

```powershell
signtool sign /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 bin\modbit-core.exe
signtool verify /pa bin\modbit-core.exe
```

## Verification (any machine)

```bash
tools/release/verify-bundle.sh dist/modbit-<version>-<target>
```

A signed bundle must show `SIGNED` from verify-bundle.sh and pass
notarization staple checks. The release receipt (release-receipt.sh)
records the verification output; a receipt with `signed: false` is a
DEVELOPMENT artifact and must not ship as a production release
(docs/73 stop-the-line rule).
