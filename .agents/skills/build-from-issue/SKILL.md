---
name: build-from-issue
description: Given a spec file in docs/specs/, plan and implement the work described in the spec. Operates iteratively - analyzes the spec, creates an implementation plan, gets user approval, then builds. Includes tests, documentation updates, and commits. Trigger keywords - build from spec, implement spec, work on spec, build spec, start spec, build from issue.
---

# Build From Spec

Plan, iterate on feedback, and implement work described in a spec file under `docs/specs/`.

This skill operates as a stateful workflow — the user provides a spec file, the agent creates a plan, the user approves, and the agent builds.

## Prerequisites

- You must be in a git repository
- A spec file must exist in `docs/specs/`

## Workflow Overview

```
Read spec file from docs/specs/
  │
  ├─ Analyze spec with principal-engineer-reviewer
  │   → Present plan to user
  │   → STOP and wait for approval
  │
  ├─ User approves plan
  │   → Scope check (warn if high complexity)
  │   → Create branch
  │   → Implement changes
  │   → Write tests
  │   → Verify (tests + lint)
  │   → Update documentation
  │   → Commit
  │
  └─ User requests changes to plan
      → Revise plan
      → Present updated plan
      → STOP and wait for approval
```

## Step 1: Read the Spec

The user provides a spec file name or path (e.g., `KEYCARD-SPEC.md` or `docs/specs/KEYCARD-SPEC.md`). Resolve the path to `docs/specs/<name>` and read the file.

If the file does not exist, list available specs:

```bash
ls docs/specs/
```

Report the available specs and ask the user which one to build from.

## Step 2: Analyze the Spec with Principal Engineer Reviewer

Pass the spec contents to the `principal-engineer-reviewer` sub-agent. Use the Task tool:

```
Task tool with subagent_type="principal-engineer-reviewer"
```

In the prompt, instruct the reviewer to:

1. Read the spec thoroughly and identify what needs to change in the codebase.
2. Map the requirements to existing code — read the relevant source files.
3. Determine the **issue type** — one of: `feat` (new feature), `fix` (bug fix), `refactor`, `chore`, `perf`, `docs`.
4. Propose the minimal set of changes that satisfies the requirements.
5. Sequence the work so each step is independently testable.
6. Identify what tests are needed (unit, integration, e2e) and where they should live.
7. Assess **complexity** on a scale:
   - **Low**: Isolated change, < 3 files, clear path forward
   - **Medium**: Multiple files/components, some design decisions, but well-scoped
   - **High**: Cross-cutting changes, architectural decisions needed, significant unknowns
8. Call out risks, unknowns, and decisions that need stakeholder input.

## Step 3: Present the Plan

Present the plan to the user in this format:

```
## Implementation Plan

**Spec:** `<spec file name>`
**Issue type:** `<feat|fix|refactor|chore|perf|docs>`
**Complexity:** <Low|Medium|High>
**Confidence:** <High — clear path | Medium — some unknowns | Low — needs discussion>

### Summary
<2-3 sentences describing what will be built/changed and the approach>

### Scope
- `<file1>`: <what changes and why>
- `<file2>`: <what changes and why>
- ...

### Implementation Steps
1. <step 1 — independently testable>
2. <step 2>
3. ...

### Test Plan
- **Unit tests:** <what will be tested and where the tests live>
- **Integration tests:** <what will be tested, or "N/A" with rationale>
- **E2E tests:** <what will be tested, or "N/A" with rationale>

### Risks & Open Questions
- <risk or unknown that may need human input>

### Documentation Impact
- <which architecture/ docs will need updating, or "None expected">
```

Ask the user to approve the plan or provide feedback. **Do not proceed to build until the user explicitly approves.**

## Step 4: Scope Check

After user approval, check the **Complexity** and **Confidence** fields.

- **If Complexity is High or Confidence is Low**, warn the user:

  > "This spec is rated High complexity / Low confidence. The plan includes open questions that may need human decisions during implementation. Proceeding, but flagging this for your awareness."

  Continue — do not hard-stop. The user chose to approve.

## Step 5: Create Branch

Determine the branch prefix from the issue type in the plan:

| Issue type | Branch prefix |
| --- | --- |
| `feat` | `feat/` |
| `fix` | `fix/` |
| `refactor` | `refactor/` |
| `chore` | `chore/` |
| `perf` | `perf/` |
| `docs` | `docs/` |

Create the branch:

```bash
git checkout main
git pull origin main
git checkout -b <prefix><short-description>
```

Use a concise, descriptive branch name derived from the spec (e.g., `feat/keycard-provider`, `fix/app-name-resolution`).

## Step 6: Implement the Changes

Follow the implementation steps from the plan. Principles:

- **Follow the plan**: The plan was reviewed and approved. Stick to it unless you discover something that requires deviation.
- **Minimal scope**: Only change what the plan calls for. No unrelated refactors.
- **If you must deviate**: Note the deviation — it will be reported to the user.

Read the relevant source files before making changes. Implement step by step per the plan's sequence.

## Step 7: Write Tests

Write tests as specified in the plan's Test Plan section. Follow the project's existing test conventions.

### Unit tests

- Place alongside existing tests for the module (e.g., `#[cfg(test)]` blocks in Rust, `test_*.py` for Python)
- Cover the new/changed behavior, edge cases, and error paths
- Ensure pre-existing behavior still works

### Integration tests

- Place in the project's existing integration test directories
- Cover interactions between the changed components
- Test realistic scenarios including error conditions

### E2E tests

- Only if the plan calls for them
- Cover the full user-facing workflow affected by the change

### Test naming

Use descriptive names that document intent:
- `test_pagination_returns_correct_page_count`
- `test_rejects_negative_offset_parameter`
- `test_retry_succeeds_after_transient_failure`

## Step 8: Verify — Tests, Lint, Pre-commit (Retry Loop)

Verification has two phases: unit tests + pre-commit, then E2E tests (if applicable). Run with up to **3 attempts per phase**.

### Phase 1: Unit Tests and Pre-commit

On each attempt:

```bash
mise run pre-commit
```

**If verification fails:**

1. Read the error output carefully.
2. Fix the issues (test failures, lint errors, formatting).
3. Decrement the retry counter and try again.

**If all 3 attempts fail**, stop and report to the user:
- What passed and what failed
- The specific errors from the last attempt
- That manual intervention is needed

Do not proceed to Phase 2 or commits if Phase 1 is not green.

### Phase 2: E2E Tests (Conditional)

**Trigger**: Run this phase if any files under `e2e/` were added or modified in this build. Check with:

```bash
git diff --name-only main -- e2e/
```

If there are no changes under `e2e/`, skip this phase entirely.

If E2E files were modified, deploy to the local cluster and run the E2E test suite:

```bash
mise run cluster:deploy
mise run test:e2e:sandbox
```

`mise run test:e2e:sandbox` depends on `cluster:deploy` and `python:proto`, then runs `uv run pytest -o python_files='test_*.py' e2e/python`. However, since the cluster may need explicit deploy for code changes beyond just E2E test files, always run `mise run cluster:deploy` first as a separate step to ensure all sandbox/proxy/policy changes are live on the cluster before running E2E tests.

**E2E retry loop** (up to 3 attempts):

1. Run `mise run cluster:deploy` (only on the first attempt, or if code was changed between attempts).
2. Run `mise run test:e2e:sandbox`.
3. If tests fail:
   - Read the pytest output carefully — identify which tests failed and why.
   - Distinguish between **test bugs** (the test itself is wrong) and **implementation bugs** (the code under test is wrong).
   - Fix the failing code or tests.
   - If code changes were made (not just test fixes), re-run `mise run cluster:deploy` before retrying.
   - Decrement the retry counter and try again.
4. If tests pass, Phase 2 is green.

**If all 3 E2E attempts fail**, stop and report to the user:
- Which E2E tests are failing
- The pytest output from the last attempt
- Whether the failures appear to be test issues or implementation issues
- That manual intervention is needed

Do not proceed to commits if E2E verification is not green.

## Step 9: Update Documentation

Use the `arch-doc-writer` sub-agent to update architecture documentation. Use the Task tool:

```
Task tool with subagent_type="arch-doc-writer"
```

In the prompt, provide:
- Which files were changed and why (from the plan + any deviations)
- The spec context (what was built/fixed)
- Which architecture docs in `architecture/` are likely affected

Launch one `arch-doc-writer` instance per documentation file that needs updating. If no documentation changes are needed, the `arch-doc-writer` will make that determination.

## Step 10: Commit

Commit all changes using conventional commit format. The `<type>` comes from the issue type in the plan:

```bash
git add <files>
git commit -m "$(cat <<'EOF'
<type>(<scope>): <short description>

<brief explanation of what was implemented>
EOF
)"
```

## Step 11: Report to User

After committing, report a summary:

- **Spec:** which spec was built
- **Branch:** the branch name
- **What was built:** 1-2 sentence summary
- **Tests:** count of tests added (unit / integration / e2e)
- **Docs updated:** list of updated architecture docs, or "None needed"
- **Deviations from plan:** any deviations, or "None — implemented as planned"

## Example Usage

### First run — generate plan

User says: "Build from spec KEYCARD-SPEC.md"

1. Read `docs/specs/KEYCARD-SPEC.md`
2. Pass spec to `principal-engineer-reviewer` for analysis
3. Reviewer produces a plan: feat type, High complexity, 5 implementation steps, unit + integration tests needed
4. Present the plan to the user
5. Ask for approval — stop and wait

### Second run — user provides feedback

User says: "Looks good but skip the E2E tests for now"

1. Revise the plan to remove E2E test section
2. Present updated plan
3. Ask for approval — stop and wait

### Third run — user approves

User says: "Approved, go ahead"

1. Scope check: High complexity — warn user
2. Create branch `feat/keycard-provider`
3. Implement changes per the plan
4. Add unit tests for the new provider
5. `mise run pre-commit` passes on first attempt
6. E2E tests skipped (not in plan)
7. `arch-doc-writer` updates `architecture/sandbox-providers.md`
8. Commit with conventional commit message
9. Report summary to user
