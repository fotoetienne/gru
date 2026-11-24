#!/bin/bash
# Simulates claude / gru output for the README demo (docs/demo.tape).
# Output formats mirror the real commands; timestamps are staged to show
# a blocked issue being picked up after its blocker merges.
#
# Usage: demo-script.sh claude -p "<prompt>"
#        demo-script.sh gru lab
#        demo-script.sh gru status

DIM='\033[2m'
BOLD='\033[1m'
RESET='\033[0m'

REPO="owner/myapp"

claude_print() {
    # Stands in for an agent filing issues with gh. The agent's reply is
    # free-form; this is a plausible example.
    sleep 2.2
    echo ""
    echo "Filed two issues in ${REPO}, both labeled gru:todo:"
    echo ""
    sleep 0.3
    echo "- #101 Add rate-limit headers to API responses"
    sleep 0.3
    echo "- #102 Show rate limits in CLI output (blocked by #101)"
}

# Mirrors tprintln! in src/commands/lab.rs: blank lines carry no timestamp.
gh_api() {
    # Nested lookup of the blocker's internal ID, or a silent POST of the
    # blocked-by relationship.
    case " $* " in
        *" --silent "*) sleep 0.6 ;;
        *) echo "3047751961" ;;
    esac
}

lab_line() {
    if [ -z "$2" ]; then echo ""; else echo -e "[$1] $2"; fi
}

gru_lab() {
    trap gru_lab_shutdown INT
    local t="09:14:01"
    lab_line "$t" "🚀 Starting Gru Lab daemon"
    lab_line "$t" "📋 Monitoring 1 repository(ies)"
    lab_line "$t" "🔄 Poll interval: 30s"
    lab_line "$t" "🎰 Max concurrent slots: 2"
    lab_line "$t" "🏷️  Watching for label: gru:todo"
    lab_line "$t" ""
    lab_line "$t" "  • ${REPO}"
    lab_line "$t" ""
    lab_line "$t" "Press Ctrl-C to stop..."
    lab_line "$t" ""
    sleep 1.5
    lab_line "09:14:03" "✨ Spawned Minion for ${REPO}/issues/101"
    lab_line "09:14:03" ""
    sleep 3.5
    lab_line "09:52:34" "✨ Spawned Minion for ${REPO}/issues/102"
    lab_line "09:52:34" ""
    while true; do sleep 1; done
}

gru_lab_shutdown() {
    echo ""
    lab_line "09:53:10" ""
    lab_line "09:53:10" "🛑 Received shutdown signal, stopping daemon..."
    lab_line "09:53:10" "👋 1 Minion(s) still running — they will continue independently."
    lab_line "09:53:10" "   Use \`gru status\` to check on them, or \`gru stop <id>\` to stop one."
    exit 0
}

gru_status() {
    sleep 0.4
    printf "${BOLD}%-6s %-7s %-12s %-6s %-5s %-5s %-22s %-8s %-7s %s${RESET}\n" \
        MINION AGENT REPO ISSUE TASK PR BRANCH MODE UPTIME TOKENS
    printf "%-6s %-7s %-12s %-6s %-5s %-5s %-22s %-8s %-7s %s\n" \
        M002 claude "$REPO" "#102" do "-" "minion/issue-102-M002" working 1m 84K
}

case "$1 $2" in
    "claude -p") claude_print ;;
    "gru lab") gru_lab ;;
    "gru status") gru_status ;;
    *) echo -e "${DIM}demo-script: unsupported command: $*${RESET}" >&2; exit 1 ;;
esac
