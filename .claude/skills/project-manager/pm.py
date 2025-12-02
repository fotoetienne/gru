#!/usr/bin/env python3
"""
Gru Project Manager - Manage issues with dependency awareness

Usage:
    ./scripts/pm.py status          - Show project status and progress
    ./scripts/pm.py next            - Show what issues are ready to work on
    ./scripts/pm.py blocked         - Show blocked vs ready issues
    ./scripts/pm.py critical-path   - Show the critical path
    ./scripts/pm.py create          - Interactively create a new issue
    ./scripts/pm.py graph           - Show dependency graph visualization
"""

import json
import subprocess
import sys
import re
from typing import Dict, List, Set, Tuple, Optional
from collections import defaultdict

class Issue:
    def __init__(self, number: int, title: str, state: str, labels: List[str],
                 milestone: Optional[str], body: str):
        self.number = number
        self.title = title
        self.state = state
        self.labels = labels
        self.milestone = milestone
        self.body = body
        self.dependencies = self._parse_dependencies()

    def _parse_dependencies(self) -> List[int]:
        """Parse 'Blocked by: #X, #Y' from issue body"""
        if not self.body:
            return []

        match = re.search(r'\*\*Blocked by:\*\*\s+([#\d,\s]+)', self.body)
        if not match:
            return []

        deps_str = match.group(1)
        return [int(n) for n in re.findall(r'#?(\d+)', deps_str)]

    def phase(self) -> Optional[str]:
        """Extract phase from labels"""
        for label in self.labels:
            if label.startswith('phase-'):
                return label
        return None

    def is_ready(self, completed_issues: Set[int]) -> bool:
        """Check if all dependencies are completed"""
        return all(dep in completed_issues for dep in self.dependencies)

    def __repr__(self):
        return f"Issue(#{self.number}: {self.title})"


class ProjectManager:
    def __init__(self):
        self.issues: Dict[int, Issue] = {}
        self.load_issues()

    def load_issues(self):
        """Load all issues from GitHub"""
        cmd = ['gh', 'issue', 'list', '--limit', '1000', '--state', 'all',
               '--json', 'number,title,state,labels,milestone,body']
        result = subprocess.run(cmd, capture_output=True, text=True)

        if result.returncode != 0:
            print(f"Error loading issues: {result.stderr}", file=sys.stderr)
            sys.exit(1)

        issues_data = json.loads(result.stdout)

        for issue_data in issues_data:
            labels = [l['name'] for l in issue_data['labels']]
            milestone = issue_data['milestone']['title'] if issue_data['milestone'] else None

            issue = Issue(
                number=issue_data['number'],
                title=issue_data['title'],
                state=issue_data['state'],
                labels=labels,
                milestone=milestone,
                body=issue_data.get('body', '')
            )
            self.issues[issue.number] = issue

    def get_completed_issues(self) -> Set[int]:
        """Get set of completed issue numbers"""
        return {n for n, i in self.issues.items() if i.state == 'CLOSED'}

    def get_open_issues(self) -> List[Issue]:
        """Get list of open issues"""
        return [i for i in self.issues.values() if i.state == 'OPEN']

    def calculate_critical_path(self) -> List[int]:
        """Calculate the longest path through dependencies"""
        memo = {}

        def longest_path(issue_num: int) -> List[int]:
            if issue_num in memo:
                return memo[issue_num]

            if issue_num not in self.issues:
                return [issue_num]

            issue = self.issues[issue_num]
            if not issue.dependencies:
                return [issue_num]

            longest = []
            for dep in issue.dependencies:
                path = longest_path(dep)
                if len(path) > len(longest):
                    longest = path

            result = longest + [issue_num]
            memo[issue_num] = result
            return result

        # Find longest path among all open issues
        open_issues = self.get_open_issues()
        if not open_issues:
            return []

        longest = max((longest_path(i.number) for i in open_issues), key=len)
        return longest

    def get_ready_issues(self) -> List[Issue]:
        """Get issues that are ready to work on (all dependencies met)"""
        completed = self.get_completed_issues()
        ready = []

        for issue in self.get_open_issues():
            if issue.is_ready(completed):
                ready.append(issue)

        return ready

    def get_blocked_issues(self) -> List[Tuple[Issue, List[int]]]:
        """Get issues that are blocked with their blocking issue numbers"""
        completed = self.get_completed_issues()
        blocked = []

        for issue in self.get_open_issues():
            if not issue.is_ready(completed):
                blocking = [d for d in issue.dependencies if d not in completed]
                blocked.append((issue, blocking))

        return blocked

    def get_milestone_progress(self) -> Dict[str, Tuple[int, int]]:
        """Get progress by milestone (open, closed)"""
        progress = defaultdict(lambda: [0, 0])

        for issue in self.issues.values():
            if issue.milestone:
                idx = 0 if issue.state == 'OPEN' else 1
                progress[issue.milestone][idx] += 1

        return dict(progress)

    def cmd_status(self):
        """Show project status"""
        print("📊 Gru V0 Project Status\n")

        # Overall progress
        open_count = len(self.get_open_issues())
        closed_count = len(self.get_completed_issues())
        total = open_count + closed_count
        pct = (closed_count / total * 100) if total > 0 else 0

        print(f"Overall Progress: {closed_count}/{total} issues complete ({pct:.1f}%)")
        print(f"  ✅ Closed: {closed_count}")
        print(f"  ⏳ Open: {open_count}")
        print()

        # Milestone progress
        print("Progress by Phase:")
        progress = self.get_milestone_progress()

        milestones = [
            "Phase 1: Pure Delegation",
            "Phase 2: Workspace Ownership",
            "Phase 3: Stream Monitoring",
            "Phase 4: GitHub Integration",
            "Phase 5: Full Lifecycle"
        ]

        for milestone in milestones:
            if milestone in progress:
                open_m, closed_m = progress[milestone]
                total_m = open_m + closed_m
                pct_m = (closed_m / total_m * 100) if total_m > 0 else 0
                status = "🟢" if closed_m == total_m else "🟡" if closed_m > 0 else "⚪"
                print(f"  {status} {milestone}: {closed_m}/{total_m} ({pct_m:.0f}%)")

        print()

        # Ready vs blocked
        ready = self.get_ready_issues()
        blocked = self.get_blocked_issues()

        print(f"Work Status:")
        print(f"  ✅ Ready to work on: {len(ready)} issues")
        print(f"  ⛔ Blocked: {len(blocked)} issues")
        print()

        # Critical path
        critical_path = self.calculate_critical_path()
        print(f"Critical Path: {len(critical_path)} issues")
        print(f"  {' → '.join(f'#{n}' for n in critical_path[:5])}{'...' if len(critical_path) > 5 else ''}")
        print()

    def cmd_next(self):
        """Show what to work on next"""
        print("🎯 What to Work on Next\n")

        ready = self.get_ready_issues()
        if not ready:
            print("🎉 No issues ready! Either all done or all blocked.")
            return

        # Prioritize by critical path
        critical_path = self.calculate_critical_path()
        critical_set = set(critical_path)

        # Sort: critical path first, then by phase, then by issue number
        def priority_key(issue):
            on_critical = issue.number in critical_set
            phase_num = 99
            if issue.phase():
                phase_match = re.search(r'phase-(\d+)', issue.phase())
                if phase_match:
                    phase_num = int(phase_match.group(1))
            return (not on_critical, phase_num, issue.number)

        ready.sort(key=priority_key)

        print(f"Found {len(ready)} issues ready to work on:\n")

        for i, issue in enumerate(ready[:10], 1):
            critical = "⚡" if issue.number in critical_set else "  "
            phase = issue.phase() or "no-phase"
            print(f"{critical} {i}. #{issue.number}: {issue.title}")
            print(f"      {phase} | {issue.milestone or 'No milestone'}")
            print()

        if len(ready) > 10:
            print(f"... and {len(ready) - 10} more")
            print()

        print("💡 Tip: Issues marked with ⚡ are on the critical path")
        print()

    def cmd_blocked(self):
        """Show blocked issues"""
        print("⛔ Blocked Issues\n")

        blocked = self.get_blocked_issues()

        if not blocked:
            print("✅ No blocked issues! Everything is ready or done.")
            return

        print(f"Found {len(blocked)} blocked issues:\n")

        # Group by blocking issues
        by_blocker = defaultdict(list)
        for issue, blockers in blocked:
            key = tuple(sorted(blockers))
            by_blocker[key].append(issue)

        for blockers, issues in sorted(by_blocker.items()):
            blockers_str = ", ".join(f"#{b}" for b in blockers)
            print(f"Blocked by {blockers_str}:")
            for issue in issues:
                print(f"  • #{issue.number}: {issue.title}")
            print()

    def cmd_critical_path(self):
        """Show the critical path"""
        print("⚡ Critical Path Analysis\n")

        path = self.calculate_critical_path()

        if not path:
            print("No critical path found (all issues complete?)")
            return

        print(f"Critical path length: {len(path)} issues\n")
        print("This is the minimum sequence that must be completed:\n")

        completed = self.get_completed_issues()

        for i, issue_num in enumerate(path, 1):
            if issue_num in self.issues:
                issue = self.issues[issue_num]
                status = "✅" if issue_num in completed else "⏳"
                phase = issue.phase() or "no-phase"
                print(f"{i:2d}. {status} #{issue_num}: {issue.title}")
                print(f"       {phase} | {issue.milestone or 'No milestone'}")
            else:
                print(f"{i:2d}. ❓ #{issue_num}: (issue not found)")

        print()
        print(f"💡 {len([n for n in path if n in completed])} of {len(path)} complete")
        print()

    def cmd_graph(self):
        """Show dependency graph visualization"""
        print("📊 Dependency Graph\n")

        # Group by phase
        by_phase = defaultdict(list)
        for issue in self.get_open_issues():
            phase = issue.phase() or "no-phase"
            by_phase[phase].append(issue)

        phases = sorted(by_phase.keys())
        completed = self.get_completed_issues()

        for phase in phases:
            print(f"\n{phase.upper()}:")
            issues = sorted(by_phase[phase], key=lambda i: i.number)

            for issue in issues:
                status = "✅" if issue.number in completed else "⏳"
                deps_str = ""
                if issue.dependencies:
                    deps_str = f" (depends on: {', '.join(f'#{d}' for d in issue.dependencies)})"

                print(f"  {status} #{issue.number}: {issue.title}{deps_str}")

        print()

    def cmd_create(self):
        """Interactively create a new issue"""
        print("📝 Create New Issue\n")

        # Get title
        title = input("Issue title: ").strip()
        if not title:
            print("Title required!")
            return

        # Get description
        print("\nDescription (press Ctrl+D when done):")
        print("---")
        lines = []
        try:
            while True:
                line = input()
                lines.append(line)
        except EOFError:
            pass
        description = "\n".join(lines)

        # Get phase
        print("\n\nWhich phase?")
        print("  1. Phase 1: Pure Delegation")
        print("  2. Phase 2: Workspace Ownership")
        print("  3. Phase 3: Stream Monitoring")
        print("  4. Phase 4: GitHub Integration")
        print("  5. Phase 5: Full Lifecycle")
        phase_choice = input("Phase (1-5): ").strip()

        phase_map = {
            "1": ("Phase 1: Pure Delegation", "phase-1"),
            "2": ("Phase 2: Workspace Ownership", "phase-2"),
            "3": ("Phase 3: Stream Monitoring", "phase-3"),
            "4": ("Phase 4: GitHub Integration", "phase-4"),
            "5": ("Phase 5: Full Lifecycle", "phase-5"),
        }

        if phase_choice not in phase_map:
            print("Invalid phase!")
            return

        milestone, phase_label = phase_map[phase_choice]

        # Get dependencies
        print("\n\nDependencies (comma-separated issue numbers, or press Enter for none):")
        deps_input = input("Depends on issues: ").strip()

        dependencies = []
        if deps_input:
            dependencies = [int(d.strip().lstrip('#')) for d in deps_input.split(',')]

        # Get labels
        print("\n\nAdditional labels?")
        print("  Common: feature, bug, enhancement, documentation, testing")
        labels_input = input("Labels (comma-separated): ").strip()

        labels = [phase_label]
        if labels_input:
            labels.extend([l.strip() for l in labels_input.split(',')])

        # Build issue body
        body_parts = []

        if dependencies:
            deps_str = ", ".join(f"#{d}" for d in dependencies)
            body_parts.append(f"**Blocked by:** {deps_str}\n")

        body_parts.append(f"**Labels:** {', '.join(labels)}\n")
        body_parts.append(f"\n{description}")

        body = "\n".join(body_parts)

        # Confirm
        print("\n\n" + "="*60)
        print("PREVIEW:")
        print("="*60)
        print(f"Title: {title}")
        print(f"Milestone: {milestone}")
        print(f"Labels: {', '.join(labels)}")
        print(f"\nBody:\n{body}")
        print("="*60)

        confirm = input("\nCreate this issue? (y/n): ").strip().lower()
        if confirm != 'y':
            print("Cancelled.")
            return

        # Create issue
        cmd = [
            'gh', 'issue', 'create',
            '--title', title,
            '--body', body,
            '--milestone', milestone,
        ]

        for label in labels:
            cmd.extend(['--label', label])

        result = subprocess.run(cmd, capture_output=True, text=True)

        if result.returncode == 0:
            print(f"\n✅ Issue created: {result.stdout.strip()}")
        else:
            print(f"\n❌ Failed to create issue: {result.stderr}")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)

    command = sys.argv[1]
    pm = ProjectManager()

    commands = {
        'status': pm.cmd_status,
        'next': pm.cmd_next,
        'blocked': pm.cmd_blocked,
        'critical-path': pm.cmd_critical_path,
        'graph': pm.cmd_graph,
        'create': pm.cmd_create,
    }

    if command not in commands:
        print(f"Unknown command: {command}")
        print(__doc__)
        sys.exit(1)

    commands[command]()


if __name__ == '__main__':
    main()
