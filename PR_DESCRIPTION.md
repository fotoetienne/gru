## Summary
- Added PR description generation feature that allows Minions to write meaningful PR descriptions when work is complete
- Updated `.claude/commands/fix.md` with instructions for writing `PR_DESCRIPTION.md` when implementation is ready for review
- Modified `src/commands/fix.rs` to detect `PR_DESCRIPTION.md`, use its content for PR body, and mark PR ready for review
- Added `PR_DESCRIPTION.md` to ephemeral files list in `src/commands/clean.rs` for automatic cleanup
- Refactored PR creation logic to use async `try_exists()` and extracted WIP template to helper function

## Test plan
- ✅ All unit tests pass: `just test` (149 passed, 4 ignored)
- ✅ Added test case `test_is_ephemeral_file_pr_description()` to verify cleanup logic
- ✅ Linter clean: `just lint` passes with no warnings
- ✅ Pre-commit hooks pass (formatting, linting, tests)
- ✅ Code reviewed by autonomous code-reviewer agent

## Implementation Details

**Claude Side:**
- Instructions in `.claude/commands/fix.md` guide Minions to write `PR_DESCRIPTION.md` only when truly ready for review
- Format includes Summary, Test plan, and Notes sections

**Gru Side:**
- Checks for `PR_DESCRIPTION.md` using async `tokio::fs::try_exists()`
- If file exists and readable: uses content as PR body, removes "[WIP]" from title, marks PR ready
- If file missing/empty/unreadable: uses template, keeps "[WIP]" prefix, stays as draft
- Automatically deletes `PR_DESCRIPTION.md` after PR creation
- Graceful error handling for all failure modes (file read, mark ready, file delete)

**Code Quality:**
- Extracted `create_wip_template()` helper function to eliminate duplication
- Uses proper async patterns with `tokio::fs`
- Comprehensive error handling with user-friendly messages
- Backward compatible - existing behavior unchanged when file not present

## Notes
- This feature enables Minions to signal completion and provide context to reviewers
- Only marks PR ready when description file exists, ensuring quality control
- Falls back gracefully to draft PR with template if anything goes wrong
- The code-reviewer agent identified and all medium-priority issues were addressed:
  - Used `tokio::fs::try_exists()` for async consistency
  - Extracted duplicated WIP template to helper function
- Remaining suggestions (like moving `mark_pr_ready()` to CLI function) are architectural and can be addressed in future refactoring
