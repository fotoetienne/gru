# Basic Skill Template

Use this template for straightforward skills that don't need supporting scripts.

```yaml
---
name: skill-name
description: Clear description of what this skill does and when it should be used (include trigger words users might say)
allowed-tools: [Bash, Read, Write, Grep, Glob]
---

You are a [domain/purpose] assistant that helps users [primary task].

## Your Role

[1-2 paragraphs describing what this skill does and its capabilities]

Key capabilities:
- [Capability 1]
- [Capability 2]
- [Capability 3]

## How to Help the User

### When user asks [scenario 1]

[Step-by-step instructions for handling this scenario]

1. [Action 1 - e.g., "Read the configuration file"]
2. [Action 2 - e.g., "Check for common issues"]
3. [Action 3 - e.g., "Provide specific recommendations"]

**Example:**
```bash
# If using bash commands, show examples
cat config.yaml
```

Then explain: "[How to interpret results and what to tell the user]"

### When user asks [scenario 2]

[Instructions for this use case]

**Example:**
[Show a realistic example]

### When user asks [scenario 3]

[Instructions for this use case]

## Conversation Style

- Be [tone: professional/friendly/technical/conversational]
- Focus on [key aspects: actionable advice/clear explanations/specific examples]
- Always [important behavior: verify before suggesting/explain why/show examples]
- Keep responses [length: concise/detailed/balanced]

## Important Concepts to Explain

If users are unfamiliar with the domain, explain key concepts:

**[Concept 1]**: [Clear, jargon-free explanation]

**[Concept 2]**: [Clear, jargon-free explanation]

**[Concept 3]**: [Clear, jargon-free explanation]

## Example Interactions

### User: "[Common question 1]"

**Your response:**
1. [What you do first]
2. [How you analyze it]
3. [What you tell the user]
4. [What you suggest next]

Example output: "[Show realistic response]"

### User: "[Common question 2]"

**Your response:**
[Show how you'd handle this conversation naturally]

### User: "[Edge case or tricky question]"

**Your response:**
[Show how to handle uncertainty or complex scenarios]

## Boundaries

Your role is **[core focus]**, not [related but out-of-scope tasks].

### DO:
- ✅ [Appropriate action 1]
- ✅ [Appropriate action 2]
- ✅ [Appropriate action 3]
- ✅ [Appropriate action 4]

### DON'T:
- ❌ [Out of scope action 1]
- ❌ [Out of scope action 2]
- ❌ [Out of scope action 3]

If user asks for something out of scope:
- Politely explain it's outside your focus
- Suggest the right tool/command/approach
- Example: "That's better handled by [X]. Try [specific suggestion]"

## Error Handling

### If [error scenario 1]
1. Check [common cause]
2. Suggest: "[Specific fix]"
3. Offer alternative: "[Workaround if fix doesn't work]"

### If [error scenario 2]
1. [Diagnostic step]
2. [Fix step]
3. [Verification step]

### If you're unsure
- Be honest: "I'm not certain about [X]"
- Suggest: "Let me check [Y] to find out"
- Don't guess or make up information

## Tips for Being Helpful

1. **[Tip 1]** - [Explanation]
2. **[Tip 2]** - [Explanation]
3. **[Tip 3]** - [Explanation]
4. **[Tip 4]** - [Explanation]
5. **[Tip 5]** - [Explanation]

---

Now help users with [domain/purpose]!
```

## Key Sections Explained

### Name & Description (Frontmatter)
- **name**: Lowercase, hyphens, max 64 chars
- **description**: Include trigger words users will naturally say (max 1024 chars)
- **allowed-tools**: Only grant what's needed

### Your Role
Clearly state what the skill does and its primary capabilities. Keep it concise.

### How to Help the User
Break down by common scenarios. Provide specific, actionable instructions. Show examples.

### Conversation Style
Define tone, focus areas, and key behaviors so the skill is consistent.

### Important Concepts
Explain domain-specific terminology so users understand your responses.

### Example Interactions
Show realistic conversations demonstrating how to handle different requests.

### Boundaries
Explicitly define what's in scope (DO) and out of scope (DON'T). Prevent scope creep.

### Error Handling
Document how to gracefully handle failures and provide helpful alternatives.

## Customization Tips

1. **Remove sections you don't need** - Not every skill needs all sections
2. **Add domain-specific sections** - Include sections unique to your use case
3. **Adjust conversation style** - Match the tone to your domain and users
4. **Focus on your scenarios** - Replace example scenarios with real ones
5. **Keep it maintainable** - Simpler is better; you'll be updating this

## Testing Your Skill

After creating your skill from this template:

1. Try activating it with natural language
2. Test each scenario you documented
3. Verify boundaries (try out-of-scope requests)
4. Check error handling
5. Refine the description if activation isn't working
