#!/bin/bash

set -euo pipefail
IFS=$'\n\t'

# ============================================================
# easylife-ai local installer
# Supports both source build (cargo) and pre-built binary fallback.
# No network access required for binary mode.
# ============================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

error() {
    echo -e "${RED}Error: $1${NC}" >&2
    exit 1
}

warn() {
    echo -e "${YELLOW}Warning: $1${NC}" >&2
}

success() {
    echo -e "${GREEN}$1${NC}"
}

# ============================================================
# 用户可配置区 — 部署时按需修改以下变量
# ============================================================

# 安装目录：easylife-ai 二进制文件的安装位置
# 默认安装到当前用户 home 目录下的 .easylife-ai/bin
# 私有化部署或统一管理多用户时可改为绝对路径（如 /opt/easylife-ai/bin）
INSTALL_DIR="$HOME/.easylife-ai/bin"

# 自动更新服务端地址：客户端检查新版本时访问的 URL
# 私有化部署时改为内部服务器地址（如 https://your-internal-server.com）
UPDATE_RELEASE_URL_DEFAULT="https://github.com/easylife1997/easylife-ai/releases"

# 稳定版更新目标版本：每次发布时手动更新，格式为 x.y.z
UPDATE_RELEASE_VERSION_DEFAULT="1.0.6"

# 自动更新检查间隔（秒）：默认 86400 秒（24 小时）
# 设为更大的值可降低检查频率；设为 0 时客户端行为由 disable_auto_updates 控制
UPDATE_CHECK_INTERVAL_SECONDS_DEFAULT=3600

# 更新通道：控制客户端跟踪哪个发布通道
# 可选值：latest（稳定版）、next（预览版）
UPDATE_CHANNEL_DEFAULT="latest"

# 是否禁用自动更新：false = 允许自动更新（默认）；true = 锁定当前版本，不自动升级
# 注意：此值仅在用户配置文件中不存在该字段时写入，已有配置的用户不受影响
DISABLE_AUTO_UPDATES_DEFAULT=false

# 是否禁用版本检查提示：false = 正常显示版本过旧提示（默认）；true = 静默跳过
# 注意：与 disable_auto_updates 相同，仅在该字段缺失时写入
DISABLE_VERSION_CHECKS_DEFAULT=false

# ============================================================

# Directory containing this script (where the binaries live)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case $ARCH in
    "x86_64")  ARCH="x64" ;;
    "aarch64"|"arm64") ARCH="arm64" ;;
    *) error "Unsupported architecture: $ARCH" ;;
esac

case $OS in
    "darwin") OS="macos" ;;
    "linux")  OS="linux" ;;
    *) error "Unsupported operating system: $OS" ;;
esac

BINARY_NAME="easylife-ai-${OS}-${ARCH}"
BINARY_PATH="${SCRIPT_DIR}/${BINARY_NAME}"

if [ ! -f "$BINARY_PATH" ]; then
    error "Binary not found: ${BINARY_PATH}\nMake sure ${BINARY_NAME} is in the same directory as this script."
fi

# Detect standard git (needed for git-og symlink and config)
detect_std_git() {
    local git_path=""
    if git_path=$(type -P git 2>/dev/null); then
        :
    else
        git_path=$(command -v git 2>/dev/null || true)
    fi
    if [ -z "$git_path" ]; then
        git_path=$(which git 2>/dev/null || true)
    fi
    if [ -n "$git_path" ] && [[ "$git_path" == *"git-ai"* ]]; then
        git_path=""
    fi
    if [ -z "$git_path" ]; then
        local cfg_json="$HOME/.easylife-ai/config.json"
        if [ -f "$cfg_json" ]; then
            local cfg_git_path
            cfg_git_path=$(sed -n 's/.*"git_path"[[:space:]]*:[[:space:]]*"\(.*\)".*/\1/p' "$cfg_json" | head -n1 || true)
            if [ -n "$cfg_git_path" ] && [[ "$cfg_git_path" != *"git-ai"* ]]; then
                if "$cfg_git_path" --version >/dev/null 2>&1; then
                    git_path="$cfg_git_path"
                fi
            fi
        fi
    fi
    if [ -z "$git_path" ]; then
        error "Could not detect a standard git binary on PATH. Please ensure Git is installed."
    fi
    echo "$git_path"
}

STD_GIT_PATH=$(detect_std_git)

mkdir -p "$INSTALL_DIR"

# ============================================================
# Optional: Build from source if Cargo.toml is found
# Source directory can be set via SOURCE_DIR env var,
# otherwise defaults to the script's own directory (i.e. the git-ai repo root).
# If no Cargo.toml or no cargo, falls back to pre-built binary.
# ============================================================
SOURCE_DIR="${SOURCE_DIR:-${SCRIPT_DIR}}"
BUILT_FROM_SOURCE=false

if [ -f "${SOURCE_DIR}/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    echo "Building from source (${SOURCE_DIR})..."
    if (cd "${SOURCE_DIR}" && RUSTFLAGS="-A warnings" cargo build --release --bin git-ai >/dev/null 2>&1); then
        BUILT_FROM_SOURCE=true
        # Replace the pre-built binary with the freshly compiled one
        rm -f "${BINARY_PATH}"
        cp "${SOURCE_DIR}/target/release/git-ai" "${BINARY_PATH}"
        success "Built from source and updated ${BINARY_NAME}"
    else
        warn "Source build failed, falling back to pre-built binary"
    fi
else
    echo "No source build (set SOURCE_DIR or ensure cargo is available for source builds)"
fi

echo "Installing easylife-ai from ${BINARY_PATH}..."
rm -f "${INSTALL_DIR}/easylife-ai"
cp "$BINARY_PATH" "${INSTALL_DIR}/easylife-ai"
chmod +x "${INSTALL_DIR}/easylife-ai"

# Symlinks
ln -sf "${INSTALL_DIR}/easylife-ai" "${INSTALL_DIR}/git"
ln -sf "$STD_GIT_PATH" "${INSTALL_DIR}/git-og"

# Remove quarantine on macOS
if [ "$OS" = "macos" ]; then
    xattr -d com.apple.quarantine "${INSTALL_DIR}/easylife-ai" 2>/dev/null || true
fi

# ~/.local/bin symlink (non-fatal)
LOCAL_BIN_DIR="$HOME/.local/bin"
if mkdir -p "$LOCAL_BIN_DIR" 2>/dev/null; then
    ln -sf "${INSTALL_DIR}/easylife-ai" "${LOCAL_BIN_DIR}/easylife-ai" 2>/dev/null || true
fi

success "Installed to ${INSTALL_DIR}"

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

# Write tracker-config.json if TRACKER_URL + TEAM_ID + TEAM_KEY are provided
TRACKER_CONFIG_PATH="$CONFIG_DIR/tracker-config.json"
if [ -n "${TRACKER_URL:-}" ] && [ -n "${TEAM_ID:-}" ] && [ -n "${TEAM_KEY:-}" ]; then
    # Load existing blacklist if config already exists
    EXISTING_BLACKLIST="[]"
    if [ -f "$TRACKER_CONFIG_PATH" ]; then
        EXISTING_BLACKLIST=$(python3 -c "
import json, sys
try:
    with open('$TRACKER_CONFIG_PATH') as f:
        d = json.load(f)
    print(json.dumps(d.get('blacklist', [])))
except: print('[]')
" 2>/dev/null || echo "[]")
    fi

    # USER_NAME is the sole source of tracker identity. Blank values must not
    # create or overwrite tracker configuration.
    case "${USER_NAME:-}" in
        *[![:space:]]*) USERNAME_FIELD="$(python3 -c 'import json, os; print(json.dumps(os.environ["USER_NAME"]))')" ;;
        *)
            warn "USER_NAME is missing or blank; tracker configuration was not written."
            USERNAME_FIELD=""
            ;;
    esac

    if [ -n "$USERNAME_FIELD" ]; then
        TMP_TRACKER="$TRACKER_CONFIG_PATH.tmp.$$"
        python3 -c "
import json
config = {
    'tracker_url': '${TRACKER_URL}',
    'team_id': '${TEAM_ID}',
    'team_key': '${TEAM_KEY}',
    'username': ${USERNAME_FIELD},
    'blacklist': json.loads('${EXISTING_BLACKLIST}')
}
with open('$TMP_TRACKER', 'w') as f:
    json.dump(config, f, indent=2)
" 2>/dev/null && mv -f "$TMP_TRACKER" "$TRACKER_CONFIG_PATH"
        success "Tracker config written to ${TRACKER_CONFIG_PATH}"
    fi
else
    echo "Tracker config skipped (set TRACKER_URL, TEAM_ID, TEAM_KEY to enable)"
fi

# Install hooks
echo "Setting up IDE/agent hooks..."
if ! "${INSTALL_DIR}/easylife-ai" install-hooks; then
    warn "Failed to set up IDE/agent hooks. Run 'easylife-ai install-hooks' manually."
else
    success "IDE/agent hooks configured"
fi

# Detect shell configs and inject PATH
detect_all_shells() {
    local shells=""
    [ -f "$HOME/.bashrc" ]  && shells="${shells}bash|$HOME/.bashrc\n"
    [ -f "$HOME/.bash_profile" ] && [ -z "$(echo "$shells" | grep bash)" ] && shells="${shells}bash|$HOME/.bash_profile\n"
    [ -f "$HOME/.zshrc" ]   && shells="${shells}zsh|$HOME/.zshrc\n"
    [ -f "$HOME/.config/fish/config.fish" ] && shells="${shells}fish|$HOME/.config/fish/config.fish\n"
    if [ -z "$shells" ]; then
        local login_shell=""
        [ -n "${SHELL:-}" ] && login_shell=$(basename "$SHELL")
        case "$login_shell" in
            fish) shells="fish|$HOME/.config/fish/config.fish" ;;
            zsh)  shells="zsh|$HOME/.zshrc" ;;
            *)    shells="bash|$HOME/.bashrc" ;;
        esac
    fi
    printf '%b' "$shells" | sed '/^$/d'
}

SHELLS_CONFIGURED=""
SHELLS_ALREADY_CONFIGURED=""

while IFS='|' read -r shell_name config_file; do
    [ -z "$shell_name" ] && continue
    if [ "$shell_name" = "fish" ]; then
        path_cmd="fish_add_path -g \"$INSTALL_DIR\""
        mkdir -p "$(dirname "$config_file")" 2>/dev/null || true
    else
        path_cmd="export PATH=\"$INSTALL_DIR:\$PATH\""
    fi
    touch "$config_file"
    if ! grep -qsF "$INSTALL_DIR" "$config_file"; then
        echo "" >> "$config_file"
        echo "# Added by easylife-ai installer on $(date)" >> "$config_file"
        echo "$path_cmd" >> "$config_file"
        SHELLS_CONFIGURED="${SHELLS_CONFIGURED}${shell_name}|${config_file}\n"
    else
        SHELLS_ALREADY_CONFIGURED="${SHELLS_ALREADY_CONFIGURED}${shell_name}|${config_file}\n"
    fi
done <<< "$(detect_all_shells)"

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
        echo "  - source $config_file"
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

echo ""
echo -e "${YELLOW}Close and reopen your terminal and IDE sessions to use easylife-ai.${NC}"

INSTALLED_VERSION=$("${INSTALL_DIR}/easylife-ai" --version 2>&1 || echo "unknown")
echo "Installed ${INSTALLED_VERSION}"
