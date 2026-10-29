#!/bin/sh
# Package the local release binary. Never install or publish the application.
set -eu

usage() {
    cat <<'HELP'
Usage: ./scripts/bundle.sh [--no-build] [--force]

  --no-build  Package the existing target/release/leafterm binary.
  --force     Replace this repository's dist/LeafTerm.app.

The default builds with Cargo and refuses to replace an existing app bundle.
HELP
}

build=1
force=0
for argument in "$@"; do
    case "$argument" in
        --no-build) build=0 ;;
        --force) force=1 ;;
        --help|-h) usage; exit 0 ;;
        *) printf 'Unknown argument: %s\n' "$argument" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' 'LeafTerm currently requires macOS and Metal.' >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
app_dir="$project_dir/dist/LeafTerm.app"

if [ -L "$app_dir" ]; then
    printf '%s\n' 'Refusing to replace a symbolic link at dist/LeafTerm.app.' >&2
    exit 1
fi
if [ -e "$app_dir" ] && [ "$force" -ne 1 ]; then
    printf '%s\n' 'dist/LeafTerm.app already exists. Use --force to rebuild it.' >&2
    exit 1
fi

cd "$project_dir"
if [ "$build" -eq 1 ]; then
    if command -v cargo >/dev/null 2>&1; then
        cargo_program=$(command -v cargo)
    elif [ -x "$HOME/.cargo/bin/cargo" ]; then
        cargo_program="$HOME/.cargo/bin/cargo"
    else
        printf '%s\n' 'Rust/Cargo is required. Install Rust, then run this script again.' >&2
        exit 1
    fi
    "$cargo_program" build --release --locked
fi

target_dir=${CARGO_TARGET_DIR:-"$project_dir/target"}
binary="$target_dir/release/leafterm"
if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    printf 'Release binary not found: %s\n' "$binary" >&2
    printf '%s\n' 'Run cargo build --release, or omit --no-build.' >&2
    exit 1
fi

version=$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)
version=${version:-0.1.0}
mkdir -p "$project_dir/dist"
staging_dir=$(mktemp -d "$project_dir/dist/.leafterm-bundle.XXXXXX")
trap 'rm -rf "$staging_dir"' 0
trap 'exit 130' INT
trap 'exit 143' TERM

staged_app="$staging_dir/LeafTerm.app"
mkdir -p "$staged_app/Contents/MacOS" "$staged_app/Contents/Resources"
cp "$binary" "$staged_app/Contents/MacOS/leafterm"
chmod 755 "$staged_app/Contents/MacOS/leafterm"

# Build native Dock/Finder icon representations from the project-owned source.
icon_source="$project_dir/assets/LeafTerm.png"
if [ ! -f "$icon_source" ]; then
    printf 'Icon source not found: %s\n' "$icon_source" >&2
    exit 1
fi
iconset="$staging_dir/LeafTerm.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$icon_source" --out "$iconset/icon_${size}x${size}.png" >/dev/null
    retina_size=$((size * 2))
    sips -z "$retina_size" "$retina_size" "$icon_source" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$staged_app/Contents/Resources/LeafTerm.icns"

cat > "$staged_app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>cn.arise.LeafTerm</string>
    <key>CFBundleExecutable</key><string>leafterm</string>
    <key>CFBundleName</key><string>LeafTerm</string>
    <key>CFBundleDisplayName</key><string>LeafTerm</string>
    <key>CFBundleIconFile</key><string>LeafTerm.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

plutil -lint "$staged_app/Contents/Info.plist"
codesign --force --sign - "$staged_app"
codesign --verify "$staged_app"

# Keep an existing local bundle until the replacement is fully built and signed.
if [ -e "$app_dir" ]; then
    mv "$app_dir" "$staging_dir/previous.app"
fi
if ! mv "$staged_app" "$app_dir"; then
    if [ -e "$staging_dir/previous.app" ]; then
        mv "$staging_dir/previous.app" "$app_dir"
    fi
    exit 1
fi

printf 'Created %s\n' "$app_dir"
printf '%s\n' 'Open it with: open dist/LeafTerm.app'
