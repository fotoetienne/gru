# Skills Skill

A meta-skill that helps you create new Claude skills.

## What It Does

The `skills` skill guides you through the entire process of creating a new Claude skill:

1. **Concept Development** - Helps you define what your skill should do
2. **Naming** - Ensures your skill name follows conventions
3. **Description Writing** - Crafts effective descriptions that trigger properly
4. **Tool Selection** - Helps you choose appropriate `allowed-tools`
5. **Content Structure** - Guides you through organizing your SKILL.md
6. **Implementation** - Creates the files and directory structure
7. **Testing** - Helps verify your skill works correctly

## How to Use It

Simply invoke the skill and describe what you want to create:

```
skill: skills

I want to create a skill that helps with API documentation
```

Or ask for specific help:

```
skill: skills

How do I create a skill that validates JSON schemas?
```

```
skill: skills

Show me a template for a skill with a Python script
```

## What It Provides

### Templates
- Basic skill template
- Advanced skill template (with supporting scripts)
- YAML frontmatter examples
- Section structure recommendations

### Validation
- Name format checking (lowercase, hyphens, max 64 chars)
- Description effectiveness review
- Tool permission recommendations
- YAML syntax validation

### Best Practices
- Focus and scope guidance
- Boundary definition
- Error handling patterns
- Testing strategies
- Activation trigger optimization

### Examples
- References the `pm` skill as a real-world example
- Provides conversation flow examples
- Shows command usage patterns

## Skill Creation Workflow

When you invoke the skills skill, it will:

1. Ask about your use case and requirements
2. Suggest an appropriate name
3. Help draft the description
4. Recommend tool permissions
5. Provide a template to start from
6. Create the directory structure and SKILL.md
7. Optionally create supporting files
8. Help you test the activation

## Types of Skills You Can Create

### Read-Only Skills
Skills that analyze, report, or guide without modifying files:
- Project status checkers
- Code analyzers
- Documentation helpers
- Linters and validators

### Interactive Skills
Skills that create or modify content:
- Code generators
- File formatters
- Refactoring assistants
- Test creators

### Scripted Skills
Skills with supporting Python/Bash scripts:
- Project management (like `pm`)
- Build tools
- Deployment helpers
- Custom CLI wrappers

## Key Files

- `SKILL.md` - The main skill definition with instructions
- `README.md` - This documentation file
- `basic-template.md` - Simple skill template
- `advanced-template.md` - Template for skills with scripts

## Tips for Success

1. **Start Simple** - Create a basic skill first, add complexity later
2. **Test Often** - Try different ways of activating your skill
3. **Be Specific** - Clear, focused skills work better than broad ones
4. **Use Examples** - Include real conversation flows in your SKILL.md
5. **Define Boundaries** - Explicitly state what's in and out of scope

## Example Skills You Might Create

- `docs` - Documentation generator/maintainer
- `test-runner` - Test execution and analysis
- `api-check` - API validation and compliance
- `security-scan` - Security analysis and recommendations
- `perf-profile` - Performance profiling and optimization
- `db-migrate` - Database migration helper
- `deploy` - Deployment workflow assistant

## Reference: The PM Skill

This project's `pm` skill is an excellent real-world example. Check out:
- `.claude/skills/pm/SKILL.md` - Well-structured skill definition
- `.claude/skills/pm/pm.py` - Supporting Python script
- `.claude/skills/pm/README.md` - User documentation

Use it as inspiration for your own skills!

## Getting Help

When using the skills skill, you can ask:
- "Show me a template for X"
- "How do I structure a skill that does Y?"
- "What tools should I allow for Z?"
- "Help me write the description for my skill"
- "Create a skill that helps with X"

The skill will guide you through the entire process collaboratively.
