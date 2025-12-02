# User Story Examples for Gru

Good user stories for Gru follow a consistent pattern and focus on developer outcomes, not implementation details.

## Template

```
## User Story
As a [user persona], I want [capability] so that [benefit].

## Context
[1-2 sentences explaining why this matters and how it fits into Gru]

## Acceptance Criteria
- [ ] Given [setup], when [action], then [outcome]
- [ ] [Observable behavior that can be tested]
- [ ] [Edge case handled]

## Technical Notes
[Optional: hints about implementation, constraints, or gotchas]

## Dependencies
Blocked by: #[issue]
Blocks: #[issue]

## Out of Scope
- [Explicitly excluded from this story]
```

## Example 1: Lab Management

### ✅ Good

```
## User Story
As a developer, I want to start a Lab with a specific number of Minion slots so that I can control resource usage on my machine.

## Context
Labs should support configurable concurrency to balance throughput with system resources.

## Acceptance Criteria
- [ ] Given I run `gru lab --slots 3`, when I check Lab status, then it shows maxSlots=3
- [ ] Given Lab has maxSlots=3, when 3 Minions are running, then Lab waits before claiming new issues
- [ ] Given Lab has maxSlots=3, when I update to --slots 5, then Lab picks up 2 more issues immediately

## Technical Notes
- Default slots = 2 if not specified
- Slots should be validated (min 1, max 20)
- Lab GraphQL API should expose currentSlots and maxSlots

## Dependencies
Blocked by: #5 (Basic Lab implementation)
Blocks: #12 (Dynamic slot adjustment)

## Out of Scope
- Auto-scaling based on system resources
- Different slot sizes per Minion type
```

### ❌ Bad

```
User Story: Implement slot management in Lab struct

Make the Lab track how many slots it has and don't let it exceed the limit.
Add a --slots flag to the CLI.
```

**Why it's bad**: Implementation-focused, no user benefit, no testable criteria

---

## Example 2: Minion Lifecycle

### ✅ Good

```
## User Story
As a developer, I want my Minion to stay alive after opening a PR so that it can respond to review comments automatically.

## Context
Current behavior: Minions exit after opening PR, requiring manual intervention for revisions.
Persistent Minions enable autonomous PR lifecycle management.

## Acceptance Criteria
- [ ] Given a Minion opens a PR, when the PR is created, then Minion enters "monitoring" state
- [ ] Given a Minion is monitoring, when a review comment is posted, then Minion processes it and pushes new commits
- [ ] Given a Minion is monitoring, when PR is merged, then Minion marks issue as done and archives
- [ ] Given a Minion is monitoring, when 7 days pass with no activity, then Minion times out and exits

## Technical Notes
- Monitor via GitHub webhooks or polling (polling MVP is fine)
- Store Minion state in .gru/work/<MINION_ID>/state.json
- Need to handle multiple review comments arriving in parallel

## Dependencies
Blocked by: #8 (Minion can open PR)
Blocks: #15 (Intelligent review response), #16 (CI failure handling)

## Out of Scope
- Learning from past review feedback
- Coordinating responses across multiple Minions
- Handling review assignments/requests
```

### ❌ Bad

```
User Story: Persistent Minions

Make Minions not exit after they open a PR. They should watch for new comments.
```

**Why it's bad**: No user persona, vague acceptance criteria, no context on "why"

---

## Example 3: Tower UI

### ✅ Good

```
## User Story
As a developer using Tower, I want to see a real-time list of all Minions across connected Labs so that I can monitor what's being worked on.

## Context
Tower provides a centralized view when multiple Labs are running. Users need visibility without SSH-ing into each Lab.

## Acceptance Criteria
- [ ] Given 2 Labs are connected, when I open Tower UI, then I see Minions from both Labs in one list
- [ ] Given a new Minion starts, when I'm viewing the UI, then it appears in the list within 2 seconds
- [ ] Given a Minion completes, when I'm viewing the UI, then it moves to "completed" section
- [ ] Given I click a Minion, when the detail view opens, then I see its plan, commits, and current status

## Technical Notes
- Use GraphQL subscription for real-time updates
- Tower proxies Lab GraphQL queries
- UI should handle Lab disconnects gracefully (show "offline" state)

## Dependencies
Blocked by: #20 (Tower GraphQL proxy), #21 (Lab WebSocket dial-out)
Blocks: #25 (Attach UI), #26 (Handoff UI)

## Out of Scope
- Historical Minion data (only active + recently completed)
- Filtering/searching Minions
- Custom dashboards or views
```

### ❌ Bad

```
User Story: Tower UI

Build a React app that shows Minions. Use GraphQL subscriptions for updates.
```

**Why it's bad**: No user benefit, focuses on tech stack, no acceptance criteria

---

## Example 4: GitHub Integration

### ✅ Good

```
## User Story
As a developer, I want my Lab to claim issues optimistically so that other Labs don't waste effort on duplicate work.

## Context
Multiple Labs may poll the same repo. Claiming issues via labels prevents collisions and makes work visible in GitHub.

## Acceptance Criteria
- [ ] Given an issue has label "ready-for-minion", when Lab claims it, then Lab adds "in-progress:M42" label
- [ ] Given an issue has "in-progress:M42", when another Lab polls, then it skips that issue
- [ ] Given a Minion fails, when Lab archives it, then Lab replaces "in-progress:M42" with "ready-for-minion"
- [ ] Given a Minion completes, when PR merges, then Lab replaces "in-progress:M42" with "minion:done"

## Technical Notes
- Label format: `in-progress:<MINION_ID>` where MINION_ID is Lab hostname + counter
- Race condition possible: two Labs claim simultaneously → first PR wins (document in README)
- Need GitHub token with `repo` scope

## Dependencies
Blocked by: #3 (Lab can poll GitHub issues)
Blocks: #10 (First-PR-wins conflict resolution)

## Out of Scope
- Distributed locking mechanism (GitHub eventual consistency is sufficient)
- Reassigning issues if Lab crashes
- Priority-based claiming
```

### ❌ Bad

```
User Story: Use GitHub labels for state

Add labels to issues when Minions start working on them.
```

**Why it's bad**: Doesn't explain why (collision avoidance), no specifics on label format

---

## Common Patterns

### For Lab Features

```
As a developer, I want [Lab capability] so that [control/visibility/reliability benefit].
```

Examples:
- "...start Lab with custom config so that I can use my preferred settings"
- "...see Lab resource usage so that I can tune slot count"
- "...restart Lab without losing active Minions so that I can update config"

### For Minion Features

```
As a developer, I want my Minion to [behavior] so that [autonomous work benefit].
```

Examples:
- "...retry failed tests so that I don't have to manually re-trigger CI"
- "...ask me questions when stuck so that it can complete work without failing"
- "...use project-specific context so that it follows our code style"

### For Tower Features

```
As a developer using Tower, I want to [remote capability] so that [multi-Lab/collaboration benefit].
```

Examples:
- "...see Minions from all Labs so that I know what's in progress across my team"
- "...attach to a remote Minion so that I can debug issues without SSH"
- "...receive handoff notifications so that I can respond quickly when Minions need help"

### For GitHub Integration

```
As a developer, I want [GitHub behavior] so that [visibility/coordination benefit].
```

Examples:
- "...see Minion progress in GitHub comments so that I don't need to check Tower"
- "...have PRs link to their issues so that I can track which Minion handled what"
- "...use GitHub labels to prioritize work so that Labs pick up urgent issues first"

---

## Anti-Patterns to Avoid

### ❌ Implementation-Focused

```
As a developer, I want Lab to use async/await so that it's performant.
```

**Fix**: Focus on user outcome, not implementation
```
As a developer, I want Lab to handle 10+ concurrent Minions so that I can maximize throughput on my machine.
```

### ❌ Too Vague

```
As a developer, I want better Minion visibility.
```

**Fix**: Be specific about what visibility means
```
As a developer, I want to see my Minion's current task and recent logs so that I can understand what it's doing without attaching.
```

### ❌ No Benefit

```
As a developer, I want to configure Tower port.
```

**Fix**: Explain why it matters
```
As a developer, I want to configure Tower port so that I can avoid conflicts with other services on my machine.
```

### ❌ Too Large

```
As a developer, I want full Minion lifecycle management.
```

**Fix**: Break into atomic stories
```
Story 1: I want to start a Minion manually for testing
Story 2: I want to pause a running Minion without killing it
Story 3: I want to resume a paused Minion from where it stopped
Story 4: I want to cancel a Minion and free its slot
```

---

## Size Guidance

**Small** (1-3 days): Single capability, clear implementation
```
As a developer, I want Lab to validate my GitHub token on startup so that I get immediate feedback if it's invalid.
```

**Medium** (3-7 days): Multiple scenarios, some complexity
```
As a developer, I want my Minion to respond to review comments so that PRs can iterate without my intervention.
```

**Large** (1-2 weeks): Needs to be broken down further
```
As a developer, I want a web UI to manage all my Labs and Minions.
→ Break into: Minion list view, Lab status view, attach feature, handoff UI, etc.
```

If a story feels large, ask: "What's the smallest useful increment?"

---

## Quality Checklist

Before finalizing a user story:

- [ ] Has clear user persona (not just "user")
- [ ] Explains the "why" (benefit)
- [ ] Includes 3+ testable acceptance criteria
- [ ] Specifies what's out of scope
- [ ] States dependencies if any
- [ ] Small enough to complete in <2 weeks
- [ ] Uses "I want" not "should be able to" language
- [ ] Focuses on outcome, not implementation
- [ ] Context explains how it fits into Gru

---

Use these examples as templates when creating new user stories for Gru features!
