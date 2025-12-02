#!/bin/bash
# Install git hooks for the Gru project
# This script configures git to use the .githooks directory

# Color codes for output
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

echo "🔧 Installing git hooks for Gru..."
echo ""

# Check if we're in a git repository
if ! git rev-parse --git-dir > /dev/null 2>&1; then
    echo -e "${RED}✗ Error: Not in a git repository${NC}"
    exit 1
fi

# Get the repository root directory
REPO_ROOT=$(git rev-parse --show-toplevel)
GITHOOKS_DIR="$REPO_ROOT/.githooks"

# Verify .githooks directory exists
if [ ! -d "$GITHOOKS_DIR" ]; then
    echo -e "${RED}✗ Error: .githooks directory not found${NC}"
    echo "  Expected location: $GITHOOKS_DIR"
    exit 1
fi

# Verify hooks are executable
for hook in "$GITHOOKS_DIR"/*; do
    if [ -f "$hook" ] && [ ! -x "$hook" ]; then
        echo -e "${YELLOW}⚠ Warning: Making $hook executable${NC}"
        chmod +x "$hook"
    fi
done

# Configure git to use .githooks directory
echo "Configuring git to use .githooks directory..."
if git config core.hooksPath .githooks; then
    echo -e "${GREEN}✓ Git hooks configured successfully${NC}"
else
    echo -e "${RED}✗ Error: Failed to configure git hooks${NC}"
    exit 1
fi

echo ""
echo -e "${GREEN}✅ Git hooks installation complete!${NC}"
echo ""
echo "The pre-commit hook will now run automatically before each commit."
echo "It will check:"
echo "  • Code formatting (cargo fmt)"
echo "  • Linting (cargo clippy --all-targets)"
echo "  • Tests (cargo test)"
echo "  • Branch protection (prevents commits to main)"
echo "  • TODO/FIXME comments (warning only)"
echo ""
echo "To bypass the hooks in emergencies, use:"
echo "  git commit --no-verify"
echo ""
echo "Note: This uses 'git config core.hooksPath' (requires Git 2.9+)"
echo "      No symlinks needed - git uses the directory directly!"
echo ""
