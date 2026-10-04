#!/usr/bin/env bash
# Build a signed, notarized, universal macOS DMG and attach it to a draft
# GitHub release for the version in src-tauri/tauri.conf.json.
#
# Credentials are read from the environment, or from release.env.local
# (git-ignored) at the repo root. See release.env.example.
#
# Usage: scripts/release.sh [--no-upload]
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

UPLOAD=1
for arg in "$@"; do
  case "$arg" in
    --no-upload) UPLOAD=0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

die() { echo "error: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

[ -f release.env.local ] && { set -a; . ./release.env.local; set +a; }
# Tauri treats a set-but-empty variable as present, so drop blank ones.
for v in APPLE_API_ISSUER APPLE_API_KEY APPLE_API_KEY_PATH APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID; do
  [ -n "${!v:-}" ] || unset "$v"
done

# --- preflight -------------------------------------------------------------

step "Preflight"

VERSION=$(node -p 'require("./src-tauri/tauri.conf.json").version')
TAG="v$VERSION"
echo "version $VERSION (tag $TAG)"

[ -z "$(git status --porcelain)" ] || die "working tree is not clean; commit or stash first"
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  die "tag $TAG already exists; bump the version in src-tauri/tauri.conf.json"
fi

[ -e src/ecotone/ecotone.css ] || die "src/ecotone is missing (private design assets symlink)"

: "${APPLE_SIGNING_IDENTITY:?set APPLE_SIGNING_IDENTITY (see release.env.example)}"
security find-identity -v -p codesigning | grep -qF "\"$APPLE_SIGNING_IDENTITY\"" \
  || die "signing identity not found in keychain: $APPLE_SIGNING_IDENTITY"

# Refuse to sign with a certificate that expires within 30 days.
if ! security find-certificate -c "$APPLE_SIGNING_IDENTITY" -p \
    | openssl x509 -noout -checkend $((30 * 24 * 3600)) >/dev/null; then
  die "signing certificate expires within 30 days; renew it first"
fi

# Signing needs Apple's timestamp server; check it now rather than after the build.
TS_PROBE=$(mktemp -d)/probe
cp /bin/echo "$TS_PROBE"
codesign -f --timestamp -s "$APPLE_SIGNING_IDENTITY" "$TS_PROBE" 2>/dev/null \
  || die "timestamped signing failed; is timestamp.apple.com reachable (VPN, firewall)? Try again shortly"
rm -rf "$(dirname "$TS_PROBE")"

# Notarization: an App Store Connect API key, or an Apple ID with an app-specific password.
if [ -n "${APPLE_API_KEY:-}" ]; then
  : "${APPLE_API_ISSUER:?set APPLE_API_ISSUER}" "${APPLE_API_KEY_PATH:?set APPLE_API_KEY_PATH}"
  [ -f "$APPLE_API_KEY_PATH" ] || die "APPLE_API_KEY_PATH not found: $APPLE_API_KEY_PATH"
  NOTARY_AUTH=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
elif [ -n "${APPLE_ID:-}" ]; then
  : "${APPLE_PASSWORD:?set APPLE_PASSWORD}" "${APPLE_TEAM_ID:?set APPLE_TEAM_ID}"
  NOTARY_AUTH=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
else
  die "no notarization credentials; set APPLE_API_* or APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID"
fi

if [ "$UPLOAD" = 1 ]; then
  gh auth status >/dev/null 2>&1 || die "gh is not logged in (run: gh auth login)"
fi

for target in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target list --installed | grep -qx "$target" || rustup target add "$target"
done

# --- build, sign, notarize -------------------------------------------------

step "Building universal app (Tauri signs and notarizes the .app)"
npm ci
npm run tauri build -- --target universal-apple-darwin --bundles app,dmg

BUNDLE="$ROOT/src-tauri/target/universal-apple-darwin/release/bundle"
APP=$(ls -d "$BUNDLE"/macos/*.app | head -1)
DMG=$(ls "$BUNDLE"/dmg/*_"$VERSION"_universal.dmg | head -1)
[ -d "$APP" ] && [ -f "$DMG" ] || die "build output not found under $BUNDLE"

step "Notarizing and stapling the DMG"
xcrun notarytool submit "$DMG" "${NOTARY_AUTH[@]}" --wait
xcrun stapler staple "$DMG"

# --- verify ----------------------------------------------------------------

step "Verifying"
codesign --verify --deep --strict --verbose=2 "$APP"
lipo -archs "$APP/Contents/MacOS/"* | grep -q arm64 || die "app binary is not universal"
spctl --assess --type execute --verbose "$APP"
spctl --assess --type open --context context:primary-signature --verbose "$DMG"
xcrun stapler validate "$DMG"

echo
echo "Built: $DMG"
if [ "$UPLOAD" = 0 ]; then
  echo "Skipping upload (--no-upload)."
  exit 0
fi

# --- release ---------------------------------------------------------------

step "Creating draft GitHub release $TAG"
git tag -a "$TAG" -m "Release $TAG"
git push origin "$TAG"
gh release create "$TAG" "$DMG" --draft --title "$TAG" --generate-notes

echo
echo "Draft release created. Review and publish it on GitHub:"
gh release view "$TAG" --json url -q .url
