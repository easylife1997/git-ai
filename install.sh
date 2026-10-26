#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

# ============================================================
# Ensure HOME is set when running via MDMs (e.g. JAMF) or other environments where HOME may be unbound.
# ============================================================
INSTALL_USER=""

if [ -z "${HOME:-}" ]; then
    if command -v scutil >/dev/null 2>&1; then
        CURRENT_USER=$( /usr/sbin/scutil <<< "show State:/Users/ConsoleUser" | awk '/Name :/ { print $3 }' || true )
        if [ -n "${CURRENT_USER:-}" ] && [ "$CURRENT_USER" != "loginwindow" ] && [ "$CURRENT_USER" != "_mbsetupuser" ]; then
            export HOME=$( /usr/bin/dscl . -read "/Users/$CURRENT_USER" NFSHomeDirectory | awk '{print $2}' )
            INSTALL_USER="$CURRENT_USER"
        else
            echo "Error: No console user logged in. Deferring installation." >&2
            exit 1
        fi
    elif id -un >/dev/null 2>&1; then
        INSTALL_USER="$(id -un)"
        export HOME=$(getent passwd "$INSTALL_USER" | cut -d: -f6)
        if [ -z "$HOME" ]; then
            export HOME="/root"
        fi
    else
        export HOME="/root"
    fi
fi

# Ensure SHELL is set (also may be unbound in JAMF)
if [ -z "${SHELL:-}" ]; then
    if command -v zsh >/dev/null 2>&1; then
        SHELL="$(command -v zsh)"
    elif command -v bash >/dev/null 2>&1; then
        SHELL="$(command -v bash)"
    else
        SHELL="/bin/sh"
    fi
    export SHELL
fi

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m' # No Color

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
PINNED_VERSION="__VERSION_PLACEHOLDER__"

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
EMBEDDED_CHECKSUMS="__CHECKSUMS_PLACEHOLDER__"

# ============================================================
# 用户可配置区 — 部署时按需修改以下变量
# ============================================================

# 安装目录：easylife-ai 二进制文件的安装位置
# 默认安装到当前用户 home 目录下的 .easylife-ai/bin
# 私有化部署或统一管理多用户时可改为绝对路径（如 /opt/easylife-ai/bin）
INSTALL_DIR="$HOME/.easylife-ai/bin"

# 自动更新服务端地址：客户端检查新版本时访问的 URL
# 私有化部署时改为内部服务器地址（如 https://your-internal-server.com）
# 本脚本发布到生产环境（https://easylife.work 反向代理），本地开发环境安装用 install-local.sh
UPDATE_RELEASE_URL_DEFAULT="https://easylife.work/client"

# 稳定版更新目标版本：每次发布时手动更新，格式为 x.y.z
UPDATE_RELEASE_VERSION_DEFAULT="1.0.9"

# 自动更新检查间隔（秒）：默认 86400 秒（24 小时）
# 设为更大的值可降低检查频率；设为 0 时客户端行为由 disable_auto_updates 控制
UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT=3600

# 更新通道：控制客户端跟踪哪个发布通道
# 可选值：latest（稳定版）、next（预览版）
UPDATE_CHANNEL_DEFAULT="latest"

# 是否禁用自动更新：true = 锁定当前版本，不自动升级（默认）；false = 允许自动更新
# 注意：此值仅在用户配置文件中不存在该字段时写入，已有配置的用户不受影响
DISABLE_AUTO_UPDATES_DEFAULT=true

# 是否禁用版本检查提示：false = 正常显示版本过旧提示（默认）；true = 静默跳过
# 注意：与 disable_auto_updates 相同，仅在该字段缺失时写入
DISABLE_VERSION_CHECKS_DEFAULT=false

# ============================================================

# Function to print error messages
error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning: $1${NC}" >&2
}

# Function to print success messages
success() {
    echo -e "${GREEN}$1${NC}"
}

# Function to verify checksum of downloaded binary
verify_checksum() {
    local file="$1"
    local binary_name="$2"

    # Skip verification if no checksums are embedded
    if [ "$EMBEDDED_CHECKSUMS" = "__CHECKSUMS_PLACEHOLDER__" ]; then
        return 0
    fi

    # Extract expected checksum for this binary
    local expected=""
    local old_ifs="$IFS"
    IFS='|' read -ra CHECKSUM_ENTRIES <<< "$EMBEDDED_CHECKSUMS"
    IFS="$old_ifs"
    for entry in "${CHECKSUM_ENTRIES[@]}"; do
        if [[ "$entry" =~ ^[[:xdigit:]]+[[:space:]]+$binary_name$ ]]; then
            expected=$(echo "$entry" | awk '{print $1}')
            break
        fi
    done

    if [ -z "$expected" ]; then
        error "No checksum found for $binary_name"
    fi

    # Calculate actual checksum
    local actual=""
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | awk '{print $1}')
    else
        warn "Neither sha256sum nor shasum available, skipping checksum verification"
        return 0
    fi

    if [ "$expected" != "$actual" ]; then
        rm -f "$file" 2>/dev/null || true
        error "Checksum verification failed for $binary_name\nExpected: $expected\nActual:   $actual"
    fi

    success "Checksum verified for $binary_name"
}

# Function to detect all shells with existing config files
# Returns shell configurations in format: "shell_name|config_file" (one per line)
detect_all_shells() {
    local shells=""
    
    # Check for bash configs (prefer .bashrc over .bash_profile)
    if [ -f "$HOME/.bashrc" ]; then
        shells="${shells}bash|$HOME/.bashrc\n"
    elif [ -f "$HOME/.bash_profile" ]; then
        shells="${shells}bash|$HOME/.bash_profile\n"
    fi
    
    # Check for zsh config
    if [ -f "$HOME/.zshrc" ]; then
        shells="${shells}zsh|$HOME/.zshrc\n"
    fi
    
    # Check for fish config
    if [ -f "$HOME/.config/fish/config.fish" ]; then
        shells="${shells}fish|$HOME/.config/fish/config.fish\n"
    fi
    
    # If no configs found, fall back to $SHELL detection and create config for that shell only
    if [ -z "$shells" ]; then
        local login_shell=""
        if [ -n "${SHELL:-}" ]; then
            login_shell=$(basename "$SHELL")
        fi
        case "$login_shell" in
            fish)
                shells="fish|$HOME/.config/fish/config.fish"
                ;;
            zsh)
                shells="zsh|$HOME/.zshrc"
                ;;
            bash|*)
                shells="bash|$HOME/.bashrc"
                ;;
        esac
    fi
    
    # Remove trailing newline and output
    printf '%b' "$shells" | sed '/^$/d'
}

detect_std_git() {
    local git_path=""

    # Prefer the actual executable path, ignoring aliases and functions
    if git_path=$(type -P git 2>/dev/null); then
        :
    else
        git_path=$(command -v git 2>/dev/null || true)
    fi

    # Last resort
    if [ -z "$git_path" ]; then
        git_path=$(which git 2>/dev/null || true)
    fi

	# Ensure we never return a path for git that contains easylife-ai (recursive)
	if [ -n "$git_path" ] && [[ "$git_path" == *"easylife-ai"* ]]; then
		git_path=""
	fi

    # If detection failed or was our own shim, try to recover from saved config
    if [ -z "$git_path" ]; then
        local cfg_json="$HOME/.easylife-ai/config.json"
        if [ -f "$cfg_json" ]; then
            # Extract git_path value without jq
            local cfg_git_path
            cfg_git_path=$(sed -n 's/.*"git_path"[[:space:]]*:[[:space:]]*"\(.*\)".*/\1/p' "$cfg_json" | head -n1 || true)
            if [ -n "$cfg_git_path" ] && [[ "$cfg_git_path" != *"easylife-ai"* ]]; then
                if "$cfg_git_path" --version >/dev/null 2>&1; then
                    git_path="$cfg_git_path"
                fi
            fi
        fi
    fi

    # Fail if we couldn't find a standard git
    if [ -z "$git_path" ]; then
        error "Could not detect a standard git binary on PATH. Please ensure you have Git installed and available on your PATH. If you believe this is a bug with the installer, please file an issue at https://github.com/easylife88-2026/easylife-ai/issues."
    fi

    # Verify detected git is usable
    if ! "$git_path" --version >/dev/null 2>&1; then
        error "Detected git at $git_path is not usable (--version failed). Please ensure you have Git installed and available on your PATH. If you believe this is a bug with the installer, please file an issue at https://github.com/easylife88-2026/easylife-ai/issues."
    fi

    echo "$git_path"
}

# Detect standard git path (needed early for install)
STD_GIT_PATH=$(detect_std_git)

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)


# Map architecture to binary name
case $ARCH in
    "x86_64")
        ARCH="x64"
        ;;
    "aarch64"|"arm64")
        ARCH="arm64"
        ;;
    *)
        error "Unsupported architecture: $ARCH"
        ;;
esac

# Map OS to binary name
case $OS in
    "darwin")
        OS="macos"
        ;;
    "linux")
        OS="linux"
        ;;
    *)
        error "Unsupported operating system: $OS"
        ;;
esac

# Determine binary name
BINARY_NAME="easylife-ai-${OS}-${ARCH}"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if [ -n "${EASYLIFE_AI_LOCAL_BINARY:-}" ]; then
    RELEASE_TAG="local"
    DOWNLOAD_URL=""
elif [ "$PINNED_VERSION" != "__VERSION_PLACEHOLDER__" ]; then
    # Version-pinned install script from a release
    RELEASE_TAG="$PINNED_VERSION"
    DOWNLOAD_URL="${UPDATE_RELEASE_URL_DEFAULT}/${RELEASE_TAG}/${BINARY_NAME}"
elif [ -n "${EASYLIFE_AI_RELEASE_TAG:-}" ] && [ "${EASYLIFE_AI_RELEASE_TAG:-}" != "latest" ]; then
    # Environment variable override
    RELEASE_TAG="$EASYLIFE_AI_RELEASE_TAG"
    DOWNLOAD_URL="${UPDATE_RELEASE_URL_DEFAULT}/${RELEASE_TAG}/${BINARY_NAME}"
else
    # Default to latest
    RELEASE_TAG="latest"
    DOWNLOAD_URL="${UPDATE_RELEASE_URL_DEFAULT}/latest/${BINARY_NAME}"
fi

# Create directory if it doesn't exist
mkdir -p "$INSTALL_DIR"

# Download and install
TMP_FILE="${INSTALL_DIR}/easylife-ai.tmp.$$"
if [ -n "${EASYLIFE_AI_LOCAL_BINARY:-}" ]; then
    echo "Using local easylife-ai binary (release: ${RELEASE_TAG})..."
    if [ ! -f "$EASYLIFE_AI_LOCAL_BINARY" ]; then
        error "Local binary not found at $EASYLIFE_AI_LOCAL_BINARY"
    fi
    cp "$EASYLIFE_AI_LOCAL_BINARY" "$TMP_FILE"
else
    echo "Downloading easylife-ai (release: ${RELEASE_TAG})..."
    if ! curl --fail --location --silent --show-error -o "$TMP_FILE" "$DOWNLOAD_URL"; then
        rm -f "$TMP_FILE" 2>/dev/null || true
        error "Failed to download binary (HTTP error)"
    fi
fi

# Basic validation: ensure file is not empty
if [ ! -s "$TMP_FILE" ]; then
    rm -f "$TMP_FILE" 2>/dev/null || true
    error "Downloaded file is empty"
fi

# Verify checksum if embedded (release builds only)
verify_checksum "$TMP_FILE" "$BINARY_NAME"

mv -f "$TMP_FILE" "${INSTALL_DIR}/easylife-ai"

# Make executable
chmod +x "${INSTALL_DIR}/easylife-ai"

# Symlink git to easylife-ai
ln -sf "${INSTALL_DIR}/easylife-ai" "${INSTALL_DIR}/git"

# Symlink git-og to the detected standard git path
ln -sf "$STD_GIT_PATH" "${INSTALL_DIR}/git-og"

# Remove quarantine attribute on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/easylife-ai" 2>/dev/null || true
fi

# Create ~/.local/bin/easylife-ai symlink for systems where ~/.local/bin is already on PATH
LOCAL_BIN_DIR="$HOME/.local/bin"
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null && ln -sf "${INSTALL_DIR}/easylife-ai" "${LOCAL_BIN_DIR}/easylife-ai" 2>/dev/null; then
    success "Created symlink at ${LOCAL_BIN_DIR}/easylife-ai"
else
    warn "Failed to create ~/.local/bin/easylife-ai symlink. This is non-fatal."
fi

success "Successfully installed easylife-ai into ${INSTALL_DIR}"
success "You can now run 'easylife-ai' from your terminal"

# Print installed version
INSTALLED_VERSION=$(${INSTALL_DIR}/easylife-ai --version 2>&1 || echo "unknown")
echo "Installed easylife-ai ${INSTALLED_VERSION}"

# Login user with install token if provided
NEED_LOGIN=false
if [ -n "${INSTALL_NONCE:-}" ] && [ -n "${API_BASE:-}" ]; then
    if ! ${INSTALL_DIR}/easylife-ai exchange-nonce; then
        NEED_LOGIN=true
    fi
fi

echo "Setting up IDE/agent hooks..."
if ! ${INSTALL_DIR}/easylife-ai install-hooks; then
    warn "Warning: Failed to set up IDE/agent hooks. Please try running 'easylife-ai install-hooks' manually."
else
    success "Successfully set up IDE/agent hooks"
fi

# Initialize update configuration without overwriting user settings.
CONFIG_DIR="$HOME/.easylife-ai"
CONFIG_JSON_PATH="$CONFIG_DIR/config.json"
mkdir -p "$CONFIG_DIR"

TMP_CFG="$CONFIG_JSON_PATH.tmp.$$"
if command -v jq >/dev/null 2>&1; then
    if [ ! -f "$CONFIG_JSON_PATH" ]; then
        jq -n \
            --arg git_path "$STD_GIT_PATH" \
            --arg update_release_url "$UPDATE_RELEASE_URL_DEFAULT" \
            --arg update_release_version "$UPDATE_RELEASE_VERSION_DEFAULT" \
            --arg update_channel "$UPDATE_CHANNEL_DEFAULT" \
            --argjson update_check_interval_seconds "$UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT" \
            --argjson disable_auto_updates "$DISABLE_AUTO_UPDATES_DEFAULT" \
            --argjson disable_version_checks "$DISABLE_VERSION_CHECKS_DEFAULT" \
            '{git_path: $git_path,
              update_release_url: $update_release_url,
              update_release_version: $update_release_version,
              update_check_interval_seconds: $update_check_interval_seconds,
              update_channel: $update_channel,
              disable_auto_updates: $disable_auto_updates,
              disable_version_checks: $disable_version_checks,
              feature_flags: {async_mode: true}}' > "$TMP_CFG"
        mv -f "$TMP_CFG" "$CONFIG_JSON_PATH"
    else
        if jq \
            --arg update_release_url "$UPDATE_RELEASE_URL_DEFAULT" \
            --arg update_release_version "$UPDATE_RELEASE_VERSION_DEFAULT" \
            --arg update_channel "$UPDATE_CHANNEL_DEFAULT" \
            --argjson update_check_interval_seconds "$UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT" \
            --argjson disable_auto_updates "$DISABLE_AUTO_UPDATES_DEFAULT" \
            --argjson disable_version_checks "$DISABLE_VERSION_CHECKS_DEFAULT" \
            'if type != "object" then error("config root must be an object") else . end
             | .update_release_url = $update_release_url
             | .update_release_version = $update_release_version
             | .update_check_interval_seconds = $update_check_interval_seconds
             | .update_channel = $update_channel
             | if has("disable_auto_updates") then . else .disable_auto_updates = $disable_auto_updates end
             | if has("disable_version_checks") then . else .disable_version_checks = $disable_version_checks end' \
            "$CONFIG_JSON_PATH" > "$TMP_CFG"; then
            mv -f "$TMP_CFG" "$CONFIG_JSON_PATH"
        else
            rm -f "$TMP_CFG"
            warn "Could not update existing config.json; preserving it unchanged"
        fi
    fi
elif command -v python3 >/dev/null 2>&1; then
    if CONFIG_JSON_PATH="$CONFIG_JSON_PATH" \
        TMP_CFG="$TMP_CFG" \
        STD_GIT_PATH="$STD_GIT_PATH" \
        UPDATE_RELEASE_URL_DEFAULT="$UPDATE_RELEASE_URL_DEFAULT" \
        UPDATE_RELEASE_VERSION_DEFAULT="$UPDATE_RELEASE_VERSION_DEFAULT" \
        UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT="$UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT" \
        UPDATE_CHANNEL_DEFAULT="$UPDATE_CHANNEL_DEFAULT" \
        DISABLE_AUTO_UPDATES_DEFAULT="$DISABLE_AUTO_UPDATES_DEFAULT" \
        DISABLE_VERSION_CHECKS_DEFAULT="$DISABLE_VERSION_CHECKS_DEFAULT" \
        python3 <<'PY'
import json
import os
from pathlib import Path

config_path = Path(os.environ["CONFIG_JSON_PATH"])
tmp_path = Path(os.environ["TMP_CFG"])
if config_path.exists():
    try:
        with config_path.open(encoding="utf-8") as handle:
            config = json.load(handle)
        if not isinstance(config, dict):
            raise ValueError("config root must be an object")
    except Exception as error:
        print(f"Warning: Could not update existing config.json: {error}", file=os.sys.stderr)
        raise SystemExit(2)
else:
    config = {
        "git_path": os.environ["STD_GIT_PATH"],
        "feature_flags": {"async_mode": True},
    }

config["update_release_url"] = os.environ["UPDATE_RELEASE_URL_DEFAULT"]
config["update_release_version"] = os.environ["UPDATE_RELEASE_VERSION_DEFAULT"]
config["update_check_interval_seconds"] = int(os.environ["UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT"])
config["update_channel"] = os.environ["UPDATE_CHANNEL_DEFAULT"]
config.setdefault("disable_auto_updates", os.environ["DISABLE_AUTO_UPDATES_DEFAULT"].lower() == "true")
config.setdefault("disable_version_checks", os.environ["DISABLE_VERSION_CHECKS_DEFAULT"].lower() == "true")
with tmp_path.open("w", encoding="utf-8") as handle:
    json.dump(config, handle, indent=2)
    handle.write("\n")
PY
    then
        mv -f "$TMP_CFG" "$CONFIG_JSON_PATH"
    else
        rm -f "$TMP_CFG"
        warn "Could not update config.json; preserving it unchanged"
    fi
else
    if [ ! -f "$CONFIG_JSON_PATH" ]; then
        cat >"$TMP_CFG" <<EOF
{
  "git_path": "${STD_GIT_PATH}",
  "update_release_url": "${UPDATE_RELEASE_URL_DEFAULT}",
  "update_release_version": "${UPDATE_RELEASE_VERSION_DEFAULT}",
  "update_check_interval_seconds": ${UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT},
  "update_channel": "${UPDATE_CHANNEL_DEFAULT}",
  "disable_auto_updates": ${DISABLE_AUTO_UPDATES_DEFAULT},
  "disable_version_checks": ${DISABLE_VERSION_CHECKS_DEFAULT},
  "feature_flags": {
    "async_mode": true
  }
}
EOF
        mv -f "$TMP_CFG" "$CONFIG_JSON_PATH"
    else
        warn "Neither jq nor python3 is available; existing config.json was left unchanged"
    fi
fi

TRACKER_CONFIG_PATH="$CONFIG_DIR/tracker-config.json"
if [ -n "${TRACKER_URL:-}" ] && [ -n "${TEAM_ID:-}" ] && [ -n "${TEAM_KEY:-}" ]; then
    # Extract existing blacklist using jq (safely)
    EXISTING_BLACKLIST="[]"
    if [ -f "$TRACKER_CONFIG_PATH" ] && command -v jq >/dev/null 2>&1; then
        EXISTING_BLACKLIST=$(jq -c '.blacklist // []' "$TRACKER_CONFIG_PATH" 2>/dev/null) || EXISTING_BLACKLIST="[]"
    fi

    # USER_NAME is the sole source of tracker identity. Do not fall back to
    # shell login variables or git email, and do not write a partial config.
    case "${USER_NAME:-}" in
        *[![:space:]]*) INSTALL_USERNAME="$USER_NAME" ;;
        *)
            warn "USER_NAME is missing or blank; tracker configuration was not written."
            INSTALL_USERNAME=""
            ;;
    esac

    if [ -n "$INSTALL_USERNAME" ]; then
        echo "Configuring tracker with username: $INSTALL_USERNAME"
        TMP_TRACKER_CFG="$TRACKER_CONFIG_PATH.tmp.$$"
        if command -v jq >/dev/null 2>&1; then
            # 合并进现有配置：保留客户端写入的字段（如 token_report），
            # 仅覆盖安装器管理的五个字段；现有文件缺失或非法时回退为全新构建。
            if ! jq \
                --arg url "$TRACKER_URL" \
                --arg id "$TEAM_ID" \
                --arg key "$TEAM_KEY" \
                --arg user "$INSTALL_USERNAME" \
                --argjson blacklist "$EXISTING_BLACKLIST" \
                '. + {tracker_url: $url, team_id: $id, team_key: $key, username: $user, blacklist: $blacklist}' \
                "$TRACKER_CONFIG_PATH" > "$TMP_TRACKER_CFG" 2>/dev/null; then
                jq -n \
                    --arg url "$TRACKER_URL" \
                    --arg id "$TEAM_ID" \
                    --arg key "$TEAM_KEY" \
                    --arg user "$INSTALL_USERNAME" \
                    --argjson blacklist "$EXISTING_BLACKLIST" \
                    '{tracker_url: $url, team_id: $id, team_key: $key, username: $user, blacklist: $blacklist}' > "$TMP_TRACKER_CFG"
            fi
        else
            error "jq is required but not found. Cannot write tracker config safely."
            exit 1
        fi
        mv -f "$TMP_TRACKER_CFG" "$TRACKER_CONFIG_PATH"
        success "Tracker configuration written to $TRACKER_CONFIG_PATH"
    fi
fi

# Add to PATH in all detected shell configurations
SHELLS_CONFIGURED=""
SHELLS_ALREADY_CONFIGURED=""
CREATED_SHELL_PATHS=""

while IFS='|' read -r shell_name config_file; do
    [ -z "$shell_name" ] && continue
    
    # Generate shell-appropriate PATH command
    if [ "$shell_name" = "fish" ]; then
        path_cmd="fish_add_path -g \"$INSTALL_DIR\""
        # Create fish config directory if it doesn't exist (for fallback case)
        config_dir="$(dirname "$config_file")"
        if [ ! -d "$config_dir" ]; then
            mkdir -p "$config_dir"
            CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_dir}\n"
        fi
    else
        path_cmd="export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
    
    # Create config file if it doesn't exist (for fallback case when no configs found)
    if [ ! -f "$config_file" ]; then
        CREATED_SHELL_PATHS="${CREATED_SHELL_PATHS}${config_file}\n"
    fi
    touch "$config_file"
    
    # Append if not already present
    if ! grep -qsF "$INSTALL_DIR" "$config_file"; then
        echo "" >> "$config_file"
        echo "# Added by easylife-ai installer on $(date)" >> "$config_file"
        echo "$path_cmd" >> "$config_file"
        SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${shell_name}|${config_file}\n"
    else
        SHELLS_ALREADY_CONFIGURED="${SHELLS_ALREADY_CONFIGURED}${shell_name}|${config_file}\n"
    fi
done <<< "$(detect_all_shells)"

# Display results to user
if [ -n "$SHELLS_CONFIGURED" ]; then
    echo ""
    echo "Updated shell configurations:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        success "  ✓ $config_file"
    done
    
    echo ""
    echo "To apply changes immediately:"
    printf '%b' "$SHELLS_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        if [ "$shell_name" = "fish" ]; then
            echo "  - For fish: source $config_file"
        else
            echo "  - For $shell_name: source $config_file"
        fi
    done
fi

if [ -n "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Already configured (no changes needed):"
    printf '%b' "$SHELLS_ALREADY_CONFIGURED" | while IFS='|' read -r shell_name config_file; do
        [ -z "$shell_name" ] && continue
        echo "  ✓ $config_file"
    done
fi

if [ -z "$SHELLS_CONFIGURED" ] && [ -z "$SHELLS_ALREADY_CONFIGURED" ]; then
    echo ""
    echo "Could not detect any shell config files."
    echo "Please add the following line to your shell config and restart:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# Fix file ownership when running as root for a different user (MDM deployments)
if [ "$(id -u)" = "0" ] && [ -n "$INSTALL_USER" ]; then
    chown -R "$INSTALL_USER" "$HOME/.easylife-ai" 2>/dev/null || true
    if [ -n "$CREATED_SHELL_PATHS" ]; then
        printf '%b' "$CREATED_SHELL_PATHS" | while IFS= read -r created_path; do
            [ -z "$created_path" ] && continue
            chown "$INSTALL_USER" "$created_path" 2>/dev/null || true
        done
    fi
fi

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use easylife-ai.${NC}"

# If nonce exchange failed, run interactive login
if [ "$NEED_LOGIN" = true ]; then
    echo ""
    echo "Launching login..."
    ${INSTALL_DIR}/easylife-ai login
fi