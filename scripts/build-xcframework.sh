#!/usr/bin/env bash
#
# build-xcframework.sh — build the Rust core (mahi-ffi) for Apple targets,
# generate the UniFFI Swift bindings, and package everything as an XCFramework.
#
# Outputs:
#   macos/MahiKit/Generated/        UniFFI-generated Swift bindings
#   macos/Frameworks/Mahi.xcframework
#
# Requirements (macOS only):
#   - rustup with the Apple targets installed (missing targets are skipped
#     with a warning rather than failing the whole build)
#   - uniffi-bindgen (workspace bin or on PATH)
#   - Xcode command-line tools (xcodebuild, lipo)
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

FFI_CRATE="mahi-ffi"
LIB_NAME="libmahi_ffi.a"
PROFILE="release"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

GENERATED_DIR="$REPO_ROOT/macos/MahiKit/Generated"
FRAMEWORKS_DIR="$REPO_ROOT/macos/Frameworks"
XCFRAMEWORK="$FRAMEWORKS_DIR/Mahi.xcframework"
STAGING_DIR="$TARGET_DIR/xcframework-staging"

IOS_TARGET="aarch64-apple-ios"
MACOS_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")

step()  { echo ""; echo "==> $*"; }
warn()  { echo "WARNING: $*" >&2; }
die()   { echo "ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
step "Preflight checks"
# ---------------------------------------------------------------------------

[[ "$(uname -s)" == "Darwin" ]] \
    || die "This script needs macOS (xcodebuild, lipo). Run it on a Mac."

command -v cargo >/dev/null     || die "cargo not found. Install Rust via rustup: https://rustup.rs"
command -v xcodebuild >/dev/null || die "xcodebuild not found. Install Xcode or the command-line tools."
command -v rustup >/dev/null    || die "rustup not found — needed to manage Apple cross-compile targets."

INSTALLED_TARGETS="$(rustup target list --installed)"

have_target() {
    grep -qx "$1" <<<"$INSTALLED_TARGETS"
}

require_or_skip() {
    local target="$1"
    if have_target "$target"; then
        return 0
    fi
    warn "Rust target '$target' is not installed — skipping this slice."
    warn "  To include it, run:  rustup target add $target"
    return 1
}

# ---------------------------------------------------------------------------
step "Building $FFI_CRATE static libraries ($PROFILE)"
# ---------------------------------------------------------------------------

BUILT_IOS=""
BUILT_MACOS=()

if require_or_skip "$IOS_TARGET"; then
    echo "--- cargo build -p $FFI_CRATE --target $IOS_TARGET"
    cargo build -p "$FFI_CRATE" --"$PROFILE" --target "$IOS_TARGET"
    BUILT_IOS="$TARGET_DIR/$IOS_TARGET/$PROFILE/$LIB_NAME"
fi

for target in "${MACOS_TARGETS[@]}"; do
    if require_or_skip "$target"; then
        echo "--- cargo build -p $FFI_CRATE --target $target"
        cargo build -p "$FFI_CRATE" --"$PROFILE" --target "$target"
        BUILT_MACOS+=("$TARGET_DIR/$target/$PROFILE/$LIB_NAME")
    fi
done

if [[ -z "$BUILT_IOS" && ${#BUILT_MACOS[@]} -eq 0 ]]; then
    die "No Apple targets are installed; nothing to package.
Install at least one of:
  rustup target add $IOS_TARGET
  rustup target add ${MACOS_TARGETS[0]}
  rustup target add ${MACOS_TARGETS[1]}"
fi

# ---------------------------------------------------------------------------
step "Generating UniFFI Swift bindings -> macos/MahiKit/Generated/"
# ---------------------------------------------------------------------------

# Prefer a workspace-local bindgen bin (UniFFI's recommended setup), then a
# uniffi-bindgen on PATH. Library mode works for both UDL and proc-macro
# exports, so point it at one of the freshly built static libs.
BINDGEN_INPUT="${BUILT_MACOS[0]:-$BUILT_IOS}"

run_bindgen() {
    if cargo run -q --bin uniffi-bindgen -- --help >/dev/null 2>&1; then
        cargo run -q --bin uniffi-bindgen -- "$@"
    elif command -v uniffi-bindgen >/dev/null 2>&1; then
        uniffi-bindgen "$@"
    else
        die "uniffi-bindgen not found.
Either add a 'uniffi-bindgen' bin target to the workspace (see
https://mozilla.github.io/uniffi-rs/latest/tutorial/foreign_language_bindings.html)
or install a standalone uniffi-bindgen on your PATH."
    fi
}

mkdir -p "$GENERATED_DIR"
run_bindgen generate --library "$BINDGEN_INPUT" --language swift --out-dir "$GENERATED_DIR"
echo "Generated:"
ls -1 "$GENERATED_DIR"

# ---------------------------------------------------------------------------
step "Staging headers and module maps"
# ---------------------------------------------------------------------------

# xcodebuild -create-xcframework wants, per slice, the static lib plus an
# include dir containing the C header and a module.modulemap. The .swift file
# stays in MahiKit/Generated and is compiled as app source.
rm -rf "$STAGING_DIR"
INCLUDE_DIR="$STAGING_DIR/include"
mkdir -p "$INCLUDE_DIR"

shopt -s nullglob
HEADERS=("$GENERATED_DIR"/*.h)
MODULEMAPS=("$GENERATED_DIR"/*.modulemap)
shopt -u nullglob

[[ ${#HEADERS[@]} -gt 0 ]] \
    || die "No C header found in $GENERATED_DIR — did uniffi-bindgen run correctly?"

cp "${HEADERS[@]}" "$INCLUDE_DIR/"
if [[ ${#MODULEMAPS[@]} -gt 0 ]]; then
    # Clang only auto-discovers a map named module.modulemap; merge if several.
    cat "${MODULEMAPS[@]}" > "$INCLUDE_DIR/module.modulemap"
else
    warn "No .modulemap emitted; generating a minimal one."
    {
        echo "module MahiFFI {"
        for h in "${HEADERS[@]}"; do echo "    header \"$(basename "$h")\""; done
        echo "    export *"
        echo "}"
    } > "$INCLUDE_DIR/module.modulemap"
fi

# Fat macOS library when both darwin slices built (an XCFramework may carry
# only one library per platform).
MACOS_LIB=""
if [[ ${#BUILT_MACOS[@]} -eq 2 ]]; then
    MACOS_LIB="$STAGING_DIR/macos/$LIB_NAME"
    mkdir -p "$(dirname "$MACOS_LIB")"
    echo "--- lipo: creating universal macOS library"
    lipo -create "${BUILT_MACOS[@]}" -output "$MACOS_LIB"
elif [[ ${#BUILT_MACOS[@]} -eq 1 ]]; then
    MACOS_LIB="${BUILT_MACOS[0]}"
    warn "Only one macOS slice built — the xcframework will not be universal."
fi

# ---------------------------------------------------------------------------
step "Creating $XCFRAMEWORK"
# ---------------------------------------------------------------------------

CREATE_ARGS=()
[[ -n "$MACOS_LIB" ]] && CREATE_ARGS+=(-library "$MACOS_LIB" -headers "$INCLUDE_DIR")
[[ -n "$BUILT_IOS" ]] && CREATE_ARGS+=(-library "$BUILT_IOS" -headers "$INCLUDE_DIR")

mkdir -p "$FRAMEWORKS_DIR"
rm -rf "$XCFRAMEWORK"
xcodebuild -create-xcframework "${CREATE_ARGS[@]}" -output "$XCFRAMEWORK"

step "Done"
echo "  Swift bindings : $GENERATED_DIR"
echo "  XCFramework    : $XCFRAMEWORK"
[[ -n "$BUILT_IOS" ]]  || warn "iOS slice missing (target not installed)."
[[ -n "$MACOS_LIB" ]]  || warn "macOS slice missing (targets not installed)."
echo "Next: cd macos && xcodegen generate && open Mahi.xcodeproj"
