#!/bin/bash
# Simulates gru do output for demo purposes

DIM='\033[2m'
BOLD='\033[1m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
RESET='\033[0m'

echo ""
echo -e "${DIM}[gru]${RESET} Fetching issue #47..."
sleep 0.4
echo -e "${DIM}[gru]${RESET} ${BOLD}Fix: nil pointer dereference in user login${RESET}"
echo -e "${DIM}[gru]${RESET} labels: bug, gru:todo"
sleep 0.4
echo ""
echo -e "${DIM}[gru]${RESET} Claiming issue #47..."
sleep 0.3
echo -e "${DIM}[gru]${RESET} Setting up worktree..."
sleep 0.5
echo -e "${DIM}[gru]${RESET} Worktree ready: ~/.gru/work/owner/myapp/minion/issue-47-M003/"
sleep 0.4
echo ""
echo -e "${DIM}[gru]${RESET} Spawning Claude Code agent..."
sleep 0.7
echo ""
echo -e "${GREEN}◆${RESET} Reading issue and exploring codebase..."
sleep 0.5
echo -e "  ${DIM}→ src/handlers/auth.rs${RESET}"
sleep 0.3
echo -e "  ${DIM}→ src/models/user.rs${RESET}"
sleep 0.3
echo -e "  ${DIM}→ tests/auth_test.rs${RESET}"
sleep 0.6
echo ""
echo -e "${GREEN}◆${RESET} Found the bug — unwrap() on Option in auth.rs:47"
sleep 0.5
echo -e "${GREEN}◆${RESET} Applying fix..."
sleep 0.6
echo -e "  ${DIM}→ Replace unwrap() with proper error handling${RESET}"
sleep 0.3
echo -e "  ${DIM}→ Add test case for missing user scenario${RESET}"
sleep 0.6
echo ""
echo -e "${GREEN}◆${RESET} Running tests..."
sleep 1.0
echo -e "  ${GREEN}✓${RESET} 47 tests passed in 2.3s"
sleep 0.5
echo ""
echo -e "${GREEN}◆${RESET} Running code review..."
sleep 0.8
echo -e "  ${GREEN}✓${RESET} No issues found"
sleep 0.5
echo ""
echo -e "${GREEN}◆${RESET} Committing changes..."
sleep 0.4
echo -e "${GREEN}◆${RESET} Opening pull request..."
sleep 0.7
echo ""
echo -e "${BOLD}${GREEN}✓ PR #48 opened${RESET}"
echo -e "  Fix: nil pointer dereference in user login"
echo -e "  ${BLUE}https://github.com/owner/myapp/pull/48${RESET}"
sleep 0.7
echo ""
echo -e "${DIM}[gru]${RESET} Monitoring CI and reviews..."
sleep 0.9
echo -e "  ${GREEN}✓${RESET} CI passed"
sleep 0.4
echo -e "  ${GREEN}✓${RESET} PR auto-merged"
echo ""
