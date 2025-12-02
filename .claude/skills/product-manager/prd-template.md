# Feature: [Feature Name]

## Problem Statement

**Who**: [User persona - e.g., "Developers using Gru to automate issue resolution"]

**Pain**: [Current problem or friction - be specific about what's broken or missing]

**Impact**: [Why this matters - cost, time, user experience, etc.]

## Proposed Solution

[2-3 sentence summary of the solution. Focus on the "what" and "how it helps," not implementation details]

## User Stories

- As a [user type], I want [capability] so that [benefit]
- As a [user type], I want [capability] so that [benefit]
- As a [user type], I want [capability] so that [benefit]

## Core Principles Check

✅ **Local-first**: [How this feature works when offline or without Tower]

✅ **One binary**: [Confirm no new binaries, services, or dependencies required]

✅ **GitHub as state**: [How GitHub (issues/labels/PRs/comments) stores the state]

✅ **Stateless Tower**: [Confirm Tower doesn't need to persist anything critical]

✅ **Persistent Minions**: [How this relates to Minions' lifecycle if applicable]

✅ **No inter-lab coordination**: [Confirm Labs don't need to communicate with each other]

✅ **Explicit Lab identity**: [How Lab ID is used if applicable]

## MVP Scope

**In scope** (must-have for v1):
- [ ] [Essential capability 1]
- [ ] [Essential capability 2]
- [ ] [Essential capability 3]

**Out of scope** (nice-to-have, defer to later):
- [ ] [Enhancement 1]
- [ ] [Enhancement 2]
- [ ] [Future optimization]

## Success Metrics

How we'll know this is working:
- [Quantitative metric - e.g., "80% of Minions complete after handoff"]
- [User behavior change - e.g., "Users attach to Minions 5+ times per week"]
- [Qualitative feedback - e.g., "Users report feeling 'in control' of agents"]

## Design Constraints

**Technical**:
- [Any implementation constraints or requirements]
- [Performance requirements]
- [Compatibility requirements]

**User Experience**:
- [UX principles or requirements]
- [Accessibility considerations]
- [Learning curve considerations]

## Open Questions

Questions to resolve before or during implementation:
- [ ] [Design decision to make]
- [ ] [Trade-off to discuss]
- [ ] [Technical approach to validate]

## Acceptance Criteria

**Scenario 1**:
- **Given** [context or precondition]
- **When** [user action]
- **Then** [expected outcome]

**Scenario 2**:
- **Given** [context or precondition]
- **When** [user action]
- **Then** [expected outcome]

**Scenario 3**:
- **Given** [context or precondition]
- **When** [user action]
- **Then** [expected outcome]

## Non-Goals

Explicitly what we're **NOT** solving:
- [Related problem we're not addressing]
- [Feature we're explicitly excluding]
- [Use case we're not supporting]

## Dependencies

**Blocked by**:
- [Feature or issue that must be completed first]

**Blocks**:
- [Feature or issue that depends on this]

**Related work**:
- [Connected features or initiatives]

## Risk & Mitigation

| Risk | Impact | Likelihood | Mitigation |
|------|--------|------------|------------|
| [Risk description] | High/Med/Low | High/Med/Low | [How we'll address it] |

## Timeline & Phases

**Phase 1** (MVP): [Target timeline]
- [Key deliverable]
- [Key deliverable]

**Phase 2** (Enhancements): [Target timeline]
- [Enhancement]
- [Enhancement]

**Phase 3** (Polish): [Target timeline]
- [Polish item]
- [Polish item]

## Rollout Plan

**Development**:
- [ ] [Implementation step]
- [ ] [Testing approach]
- [ ] [Documentation updates]

**Launch**:
- [ ] [How users will discover this]
- [ ] [Migration plan if applicable]
- [ ] [Communication plan]

**Post-launch**:
- [ ] [Monitoring approach]
- [ ] [Feedback collection]
- [ ] [Iteration plan]

## References

- GitHub Issues: [Link to related issues]
- Design docs: [Link to technical design]
- User research: [Link to user feedback or data]
- Prior art: [Similar features in other tools]

---

## Template Usage Notes

**When to use this template**:
- New features that span multiple issues
- Significant architectural changes
- Features with user-facing impact
- When you need stakeholder alignment

**When NOT to use this template**:
- Small bug fixes
- Trivial enhancements
- Internal refactors with no user impact
- When a GitHub issue is sufficient

**Customization**:
- Remove sections that don't apply
- Add project-specific sections as needed
- Keep it concise—brevity is a feature
- Focus on "why" and "what," not "how"
