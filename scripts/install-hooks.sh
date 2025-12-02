#!/bin/bash
# Install git hooks for the Gru project
# This script creates symlinks from .git/hooks to scripts/

set -e

# Color codes for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

# Get the repository root directory
REPO_ROOT=$(git rev-parse --show-toplevel)
SCRIPTS_DIR="$REPO_ROOT/scripts"

echo "🔧 Installing git hooks for Gru..."
echo ""

# Check if we're in a git repository (handles both regular repos and worktrees)
if [ ! -d "$REPO_ROOT/.git" ] && [ ! -f "$REPO_ROOT/.git" ]; then
    echo -e "${RED}Error: Not in a git repository${NC}"
    exit 1
fi

# Get the actual git directory (handles worktrees)
GIT_DIR=$(git rev-parse --git-dir)
GIT_HOOKS_DIR="$GIT_DIR/hooks"

# Create hooks directory if it doesn't exist
mkdir -p "$GIT_HOOKS_DIR"

# Install pre-commit hook
HOOK_SOURCE="$SCRIPTS_DIR/pre-commit"
HOOK_TARGET="$GIT_HOOKS_DIR/pre-commit"

# Verify the source hook exists and is executable
if [ ! -f "$HOOK_SOURCE" ]; then
    echo -e "${RED}✗ Error: Hook source not found: $HOOK_SOURCE${NC}"
    exit 1
fi

if [ ! -x "$HOOK_SOURCE" ]; then
    echo -e "${YELLOW}⚠ Warning: Hook source is not executable${NC}"
    echo "  Making it executable..."
    chmod +x "$HOOK_SOURCE"
fi

if [ -f "$HOOK_TARGET" ] || [ -L "$HOOK_TARGET" ]; then
    echo -e "${YELLOW}⚠ Pre-commit hook already exists${NC}"
    read -p "  Replace it? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        echo "  Skipping pre-commit hook installation"
    else
        if ! rm "$HOOK_TARGET" 2>/dev/null; then
            echo -e "${RED}✗ Error: Failed to remove existing hook${NC}"
            exit 1
        fi
        if ln -s "$HOOK_SOURCE" "$HOOK_TARGET"; then
            echo -e "${GREEN}✓ Pre-commit hook installed${NC}"
        else
            echo -e "${RED}✗ Error: Failed to create symlink for pre-commit hook${NC}"
            exit 1
        fi
    fi
else
    if ln -s "$HOOK_SOURCE" "$HOOK_TARGET"; then
        echo -e "${GREEN}✓ Pre-commit hook installed${NC}"
    else
        echo -e "${RED}✗ Error: Failed to create symlink for pre-commit hook${NC}"
        exit 1
    fi
fi

echo ""
echo -e "${GREEN}✅ Git hooks installation complete!${NC}"
echo ""
echo "The pre-commit hook will now run automatically before each commit."
echo "It will check:"
echo "  • Code formatting (cargo fmt)"
echo "  • Linting (cargo clippy)"
echo "  • Tests (cargo test)"
echo "  • Branch protection (prevents commits to main)"
echo "  • TODO/FIXME comments (warning only)"
echo ""
echo "To bypass the hooks in emergencies, use:"
echo "  git commit --no-verify"
echo ""
