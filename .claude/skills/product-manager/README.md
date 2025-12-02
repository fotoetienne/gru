# Product Manager Skill

A pragmatic product management assistant for shaping Gru features with user empathy and design discipline.

## What It Does

This skill acts as your product manager for Gru, helping you:

- **Shape features** with user empathy and Gru's core principles
- **Write PRDs** and user stories that are crisp and actionable
- **Evaluate designs** against local-first, one-binary, GitHub-as-state principles
- **Break down features** into shippable GitHub issues
- **Push back** on complexity that violates Gru's philosophy
- **Create product artifacts** (PRDs, user stories, acceptance criteria)

## When It Activates

The skill automatically activates when you:

- Ask about feature design or product direction for Gru
- Want to write a PRD or user story
- Need help evaluating a feature against Gru's principles
- Want to break down a feature into GitHub issues
- Ask product questions like "Should Gru support X?"

## Core Principles It Enforces

The skill is opinionated about Gru's architecture:

1. **Local-first**: Labs run autonomously, work offline
2. **One binary**: Single executable for all modes
3. **GitHub as database**: Issues, labels, PRs are the state
4. **Stateless Tower**: Tower can restart without affecting Labs
5. **Persistent Minions**: Agents stay alive post-PR for follow-up
6. **No inter-lab coordination**: Labs work independently
7. **Explicit Lab identity**: Clear Lab IDs in all APIs

## Example Interactions

### Shaping a Feature

```
User: Should Gru support running agents in the cloud?

Skill: This goes against Gru's local-first principle. Here's why...
[Provides analysis of user impact, alternatives, and recommendation]
```

### Writing a PRD

```
User: Write a PRD for Minion handoffs

Skill: [Creates structured PRD with problem statement, user stories,
principles check, MVP scope, success metrics, and acceptance criteria]
```

### Breaking Down Features

```
User: Break down the attach feature into GitHub issues

Skill: [Creates 3-4 issues with clear titles, user stories,
acceptance criteria, dependencies, and labels]
```

### Evaluating Proposals

```
User: I want Tower to cache GraphQL responses for performance

Skill: This violates the stateless Tower principle. Here are alternatives...
[Suggests HTTP caching, client-side caching, Lab optimizations]
```

## What Makes It Different

This isn't a generic PM assistant. It's:

- **Opinionated**: Enforces Gru's specific design principles
- **Pragmatic**: Focuses on MVP and shippable increments
- **User-focused**: Always asks "what pain does this solve?"
- **Technical**: Understands architecture and implementation constraints
- **Direct**: Will push back on features that violate principles

## Files It Can Create

- PRDs (Product Requirements Documents)
- User stories with acceptance criteria
- GitHub issues via `gh issue create`
- Feature breakdowns and roadmaps

## Tools It Uses

- **Bash**: For running `gh` commands to create issues
- **Read**: To review existing docs, plans, and code
- **Write/Edit**: To create PRDs and product documents
- **Glob/Grep**: To search codebase and documentation

## Tips for Working With It

1. **Be specific about the problem**: "Users struggle with X" not "Add feature Y"
2. **Ask for evaluation**: "Should Gru support X?" triggers principle-based analysis
3. **Request artifacts**: "Write a PRD for X" or "Create issues for Y"
4. **Challenge designs**: "Does this violate any principles?"
5. **Explore alternatives**: "What are other ways to solve X?"

## Example Workflows

### Planning a New Feature

1. Describe the user pain or idea
2. Skill asks clarifying questions about user value
3. Skill evaluates against core principles
4. Skill proposes MVP scope
5. Skill writes PRD or user stories
6. Skill creates GitHub issues

### Evaluating a Design

1. Propose an approach or architecture
2. Skill checks against Gru's principles
3. Skill highlights concerns or violations
4. Skill suggests alternatives
5. Skill recommends a path forward

### Breaking Down Work

1. Describe a feature or epic
2. Skill identifies shippable increments
3. Skill writes issue descriptions with acceptance criteria
4. Skill creates issues via `gh issue create`
5. Skill tags with appropriate labels (feature, p0/p1/p2)

## Boundaries

The skill will **not**:
- Implement code (that's for engineers)
- Accept vague requirements without pushing back
- Let features violate principles without raising concerns
- Write 50-page specs (it's concise by design)

The skill **will**:
- Challenge complexity and scope creep
- Ask hard questions about user value
- Recommend rejecting features that don't fit
- Break down work into pragmatic increments
- Create clear, actionable product artifacts

## Integration with Other Skills

- **project-manager**: Technical project management (issues, dependencies, critical path)
- **product-manager** (this skill): Product direction, user empathy, feature shaping
- **git-worktrees**: Development workflow management

Use `product-manager` for "what to build" and `project-manager` for "how to execute it."

## Customization

Edit `.claude/skills/product-manager/SKILL.md` to:
- Adjust the tone or style
- Add project-specific principles
- Include additional product templates
- Modify the evaluation framework

## Feedback

If the skill isn't activating when you expect, check that your query mentions:
- Feature design or product questions
- Gru specifically (not generic PM questions)
- Terms like "PRD", "user story", "should we build", etc.

The description in SKILL.md controls activation—make it more specific if needed.
