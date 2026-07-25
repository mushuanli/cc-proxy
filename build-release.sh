#!/usr/bin/env bash
set -euo pipefail

# ============================================================
# CC Proxy Release Builder
# Usage:
#   ./build-release.sh                        # 自动递增 patch，生成 changelog，编译 + 发布
#   ./build-release.sh --build-only           # 仅编译
#   ./build-release.sh --build-only darwin-arm64  # 仅编译指定架构
#
#   ./build-release.sh 2.1.0                  # 指定版本，自动生成 changelog
#   ./build-release.sh 2.1.0 --notes "xxx"    # 指定版本 + 自定义 notes
#   ./build-release.sh 2.1.0 darwin-arm64     # 指定架构 + 发布
# ============================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

# Ensure ~/.cargo/bin is in PATH (Homebrew Rust puts cargo-installed bins there)
export PATH="$HOME/.cargo/bin:$PATH"

# --- Arch short name -> (target_triple, pkg_name, ext) ---
declare -A TARGETS=()
declare -A PKG_NAMES=()
declare -A PKG_EXTS=()

_register() {
    local arch="$1" triple="$2" ext="$3"
    TARGETS["$arch"]="$triple"
    PKG_NAMES["$arch"]="cc-proxy-${arch}"
    PKG_EXTS["$arch"]="$ext"
}

_register "darwin-arm64"   "aarch64-apple-darwin"     "zip"
_register "linux-x86_64"   "x86_64-unknown-linux-gnu" "tar.gz"
_register "windows-x86_64" "x86_64-pc-windows-gnu"    "zip"

VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
OUTPUT_DIR="$SCRIPT_DIR/release"
SKIP_BUILD=false
BUILD_ONLY=false
SELECTED_ARCH=""
BUILT_COUNT=0
SKIPPED_COUNT=0
RELEASE_VERSION=""
RELEASE_NOTES=""

# --- Parse args ---
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=true; shift ;;
        --build-only) BUILD_ONLY=true; shift ;;
        --notes)
            RELEASE_NOTES="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage:"
            echo "  Release:     ./build-release.sh               # auto-bump + changelog + build + release"
            echo "  Release:     ./build-release.sh <version>     # specified version"
            echo "  Build only:  ./build-release.sh --build-only [arch]"
            echo "  Custom:      ./build-release.sh <version> --notes \"changelog\" [arch]"
            exit 0
            ;;
        *)
            if [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                RELEASE_VERSION="$1"
            elif [[ -n "${TARGETS[$1]:-}" ]]; then
                SELECTED_ARCH="$1"
            else
                echo "Unknown arg: $1"
                echo "Available arch: ${!TARGETS[*]}"
                exit 1
            fi
            shift
            ;;
    esac
done

mkdir -p "$OUTPUT_DIR"

# --- Auto-bump patch version ---
auto_bump_version() {
    local major minor patch
    IFS='.' read -r major minor patch <<< "$VERSION"
    patch=$((patch + 1))
    echo "${major}.${minor}.${patch}"
}

# --- Generate changelog from git history ---
generate_changelog() {
    # Find the latest tag, fall back to the first commit if none exists
    local last_tag
    last_tag=$(git describe --tags --abbrev=0 2>/dev/null) || last_tag=""

    if [[ -z "$last_tag" ]]; then
        git log --oneline --no-merges || echo "(no commits)"
        return
    fi

    local log
    log=$(git log "${last_tag}..HEAD" --oneline --no-merges 2>/dev/null) || true
    if [[ -z "$log" ]]; then
        echo "(no changes since $last_tag)"
    else
        echo "$log"
    fi
}

# --- Output ---
if [[ "$BUILD_ONLY" == "true" ]]; then
    echo "=== CC Proxy Release Builder v${VERSION} (build only) ==="
else
    # When no version specified, auto-bump patch
    if [[ -z "$RELEASE_VERSION" ]]; then
        RELEASE_VERSION=$(auto_bump_version)
        echo "[INFO] Auto-bumped version: $VERSION -> $RELEASE_VERSION"
    fi
    echo "=== CC Proxy Release Builder $RELEASE_VERSION (publish mode) ==="

    # Auto-generate changelog if not provided
    if [[ -z "$RELEASE_NOTES" ]]; then
        RELEASE_NOTES=$(generate_changelog)
        echo "[INFO] Auto-generated changelog from git history"
    fi
fi
echo ""

# --- Prerequisite checks for release mode ---
check_release_prereqs() {
    local errors=0

    if ! command -v gh &>/dev/null; then
        echo "[ERROR] gh CLI not found. Install: brew install gh && gh auth login"
        errors=$((errors + 1))
    fi

    if ! git diff --quiet; then
        echo "[ERROR] Working tree has unstaged changes. Please commit or stash them first."
        errors=$((errors + 1))
    fi

    if ! git diff --cached --quiet; then
        echo "[ERROR] Index has staged changes. Please commit or unstage them first."
        errors=$((errors + 1))
    fi

    local branch
    branch=$(git branch --show-current)
    if [[ "$branch" != "main" && ! "$branch" =~ ^v[0-9]+\.[0-9]+$ ]]; then
        echo "[WARN] Current branch is '$branch' (unusual for release)."
    fi

    if [[ "$errors" -gt 0 ]]; then
        echo ""
        echo "Abort: $errors prerequisite check(s) failed."
        exit 1
    fi
}

# --- Update Cargo.toml version ---
update_cargo_version() {
    local new_ver="$1"
    local cargo="$SCRIPT_DIR/Cargo.toml"

    if [[ "$VERSION" == "$new_ver" ]]; then
        echo "[INFO] Cargo.toml already at version $new_ver, skip update."
        return 0
    fi

    echo "[INFO] Updating Cargo.toml: $VERSION -> $new_ver"
    sed -i.bak "s/^version = \"$VERSION\"/version = \"$new_ver\"/" "$cargo"
    rm -f "${cargo}.bak"

    # Refresh VERSION after update
    VERSION="$new_ver"

    git add "$cargo"
    git commit -m "chore: bump version to $new_ver"
    echo "[OK] Committed version bump."
}

# --- Build artifacts (same as before) ---
build_arch() {
    local arch="$1"
    local target="${TARGETS[$arch]}"
    local pkg_name="${PKG_NAMES[$arch]}"
    local ext="${PKG_EXTS[$arch]}"
    local pkg_dir="$OUTPUT_DIR/$pkg_name"
    local binary_name="proxy-server"

    # Windows binary has .exe extension
    if [[ "$target" == *windows* ]]; then
        binary_name="proxy-server.exe"
    fi

    echo "--- Building: $pkg_name (.$ext) ---"

    # Check if target is available (works with both rustup and Homebrew Rust)
    local target_libdir
    target_libdir=$(rustc --print target-libdir --target "$target" 2>/dev/null) || true
    if [[ -z "$target_libdir" || ! -f "$target_libdir/libcore-"*.rlib ]]; then
        if command -v rustup &>/dev/null; then
            echo "  [INFO] Installing target: $target"
            rustup target add "$target"
        else
            echo "  [SKIP] target not available (run 'brew install rustup' or use rustup.rs)"
            SKIPPED_COUNT=$((SKIPPED_COUNT + 1))
            return 0
        fi
    fi

    # Build
    if [[ "$SKIP_BUILD" != "true" ]]; then
        # Use zigbuild for cross-compilation, standard cargo for native
        if [[ "$target" == "aarch64-apple-darwin" ]]; then
            echo "  [BUILD] cargo build -p proxy-server --release"
            cargo build -p proxy-server --release
        else
            if ! command -v cargo-zigbuild &>/dev/null; then
                echo "  [ERROR] cargo-zigbuild required for cross-compilation"
                echo "  Install: brew install zig && cargo install cargo-zigbuild"
                SKIPPED_COUNT=$((SKIPPED_COUNT + 1))
                return 1
            fi
            echo "  [BUILD] cargo zigbuild -p proxy-server --release --target $target"
            cargo zigbuild -p proxy-server --release --target "$target"
        fi
    else
        echo "  [SKIP] build"
    fi

    local src_bin="$SCRIPT_DIR/target/$target/release/$binary_name"
    if [[ ! -f "$src_bin" ]]; then
        echo "  [ERROR] Binary not found: $src_bin"
        return 1
    fi

    # Prepare package directory
    rm -rf "$pkg_dir"
    mkdir -p "$pkg_dir"

    cp "$src_bin" "$pkg_dir/"
    cp "$SCRIPT_DIR/config.toml" "$pkg_dir/"
    cp "$SCRIPT_DIR/settings.json" "$pkg_dir/"
    cp "$SCRIPT_DIR/statbar.sh" "$pkg_dir/"

    # Package
    cd "$OUTPUT_DIR"
    local archive="$pkg_name.$ext"
    rm -f "$archive"

    if [[ "$ext" == "tar.gz" ]]; then
        tar czf "$archive" "$pkg_name"
    else
        zip -qr "$archive" "$pkg_name"
    fi

    local size
    size=$(du -h "$archive" | cut -f1)
    echo "  [DONE] $archive ($size)"
    cd "$SCRIPT_DIR"
    BUILT_COUNT=$((BUILT_COUNT + 1))
}

# --- Build all artifacts, abort on first failure ---
build_all() {
    if [[ -n "$SELECTED_ARCH" ]]; then
        build_arch "$SELECTED_ARCH"
    else
        for arch in "${!TARGETS[@]}"; do
            build_arch "$arch" || return 1
            echo ""
        done
    fi
}

# --- Push and create GitHub release ---
do_release() {
    local tag="v$RELEASE_VERSION"
    local artifacts=()

    echo ""
    echo "=== Release: $tag ==="

    # Collect artifacts
    for arch in "${!PKG_NAMES[@]}"; do
        local pkg_name="${PKG_NAMES[$arch]}"
        local ext="${PKG_EXTS[$arch]}"
        local archive="$OUTPUT_DIR/$pkg_name.$ext"
        if [[ -f "$archive" ]]; then
            artifacts+=("$archive")
        fi
    done

    if [[ ${#artifacts[@]} -eq 0 ]]; then
        echo "[ERROR] No artifacts found in $OUTPUT_DIR/"
        exit 1
    fi

    echo "Artifacts:"
    for a in "${artifacts[@]}"; do
        echo "  - $a"
    done
    echo ""

    # Confirm before proceeding
    echo "============================================"
    echo "  About to:"
    echo "  1. git push origin \$(git branch --show-current)"
    echo "  2. git push origin $tag"
    echo "  3. gh release create $tag with ${#artifacts[@]} artifacts"
    echo "  Notes:"
    echo "$RELEASE_NOTES"
    echo "============================================"
    read -rp "  Proceed? (y/N): " confirm
    if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
        echo "Aborted by user."
        exit 0
    fi

    # Tag (if a tag with same name exists, confirm overwrite)
    if git rev-parse "$tag" &>/dev/null; then
        echo "[WARN] Tag $tag already exists (points to $(git rev-parse --short "$tag"))."
        read -rp "  Delete and recreate? (y/N): " confirm_tag
        if [[ "$confirm_tag" != "y" && "$confirm_tag" != "Y" ]]; then
            echo "Aborted by user."
            exit 0
        fi
        git tag -d "$tag"
        # If remote tag exists, warn
        if git ls-remote --tags origin "$tag" | grep -q "$tag"; then
            echo "[WARN] Remote tag $tag also exists, will be overwritten on push."
        fi
    fi

    git tag "$tag"
    echo "[OK] Created tag $tag"

    # Push
    echo "[PUSH] git push origin..."
    git push origin "$(git branch --show-current)"
    git push origin "$tag"
    echo "[OK] Pushed."

    # Create GitHub release
    echo "[RELEASE] Creating GitHub release..."
    gh release create "$tag" "${artifacts[@]}" --title "$tag" --notes "$RELEASE_NOTES"
    echo "[DONE] GitHub release created: $tag"
}

# ============================================================
# Main
# ============================================================

if [[ "$BUILD_ONLY" == "true" ]]; then
    build_all || true
    echo "=== 完成: $BUILT_COUNT 个已构建, $SKIPPED_COUNT 个已跳过. 包在 $OUTPUT_DIR/ ==="
    ls -lh "$OUTPUT_DIR"/*.{zip,tar.gz} 2>/dev/null || true
elif [[ -n "$RELEASE_VERSION" ]]; then
    check_release_prereqs
    update_cargo_version "$RELEASE_VERSION"
    build_all
    do_release
else
    echo "[ERROR] No release version determined."
    exit 1
fi
