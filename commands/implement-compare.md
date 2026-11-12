Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
BASE_BRANCH=<branch>                # the branch the implementations are built on, e.g. PS-11299-trunk
PLAN=<plan file>                    # the plan they implement
IMPLEMENT_FILES=<summary files or glob>  # each model's summary from implement.md, e.g. /work/LLM-implementations/PS-11299-trunk/implement-*.md
OUTPUT_FILE=<report path>           # the comparison, e.g. /work/LLM-implementations/PS-11299-trunk/compare.md

Default variables (used unless the caller sets them):
BRANCH=$BASE_BRANCH-$MODEL_ID       # your own branch from implement.md, if you implemented PLAN
SOURCE_DIR=$PWD                     # git checkout with BASE_BRANCH and the models' branches
PROMPT_DIR=<the directory of this file>  # where implement.md and environment.md are
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # your own build dir

If a required variable is missing, stop and ask for it.

Compare the implementations of PLAN summarized in $IMPLEMENT_FILES and write the
comparison to $OUTPUT_FILE. Each model created its own branch on top of BASE_BRANCH
by $PROMPT_DIR/implement.md, and its summary follows that file's format and names
the branch under `branch:`. The next step picks the best branch as the base for
the final implementation and improves it using this comparison, so say clearly which
branch is the best base and what the others do better.

Environment rules: those in $PROMPT_DIR/environment.md, except that this step is
read-only:
- Don't create, switch or change any worktree, branch, commit or build dir, your
  own $WORKTREE_DIR, BRANCH and $BUILD_DIR included, and don't build or run tests.
- Read code without a worktree: `git -C $SOURCE_DIR diff BASE_BRANCH...<branch>`,
  `git -C $SOURCE_DIR log BASE_BRANCH..<branch>`, `git -C $SOURCE_DIR show <branch>:path`.
- Check that each summary's branch exists, is at the SHA the summary gives, and is
  built on BASE_BRANCH (`git -C $SOURCE_DIR merge-base --is-ancestor BASE_BRANCH <branch>`).
  Note any mismatch; compare the branch as it is, not as the summary says.

For each branch
1. Plan coverage: which PLAN steps it implements fully, partly or not at all, and
   whether its deviations (its summary's and any it doesn't report) are justified.
2. Correctness: bugs, missed cases, and behaviour changes PLAN doesn't ask for, each
   with file:line at the branch and how you know: code-read > suspected.
3. Tests: what it adds, whether they cover the changed behaviour, and whether its
   reported results look complete and consistent with the code; flag any that
   look doubtful for the next step to re-run.
4. Code: size and focus of the diff, fit with the surrounding code and BASE_BRANCH's
   style, and its commits (logical steps, each building, with the `Implemented by`
   trailer).

The comparison (keep the field names exactly)

---
model: <MODEL_ID> (compare)
base: <BASE_BRANCH> @ <full SHA>
plan: <PLAN>
branches: <branch @ short SHA, comma-separated, one per summary>
best: <the branch to use as the base>
started: <YYYY-MM-DD HH:MM:SS TZ, when you received the prompt>
finished: <YYYY-MM-DD HH:MM:SS TZ, when you wrote the comparison>
sources: <the summary files compared>
---

## Overview
A table: rows are PLAN's steps, then tests, build and commits; columns are the
branches; each cell is done / partial (why) / missing / wrong (finding id).

## Findings
- B<n> <branch>, <file:line>: <the problem>
  - severity: critical | high | medium | low | nit
  - verification: code-read | suspected, with the evidence

## Best base
The branch to build the final implementation on, and why: the fewest serious
problems and the least work to fix, not the most features. Say what would change
your choice.

## Improvements
For the best base, everything to change, most important first:
- fix <B id>: <how>
- take from <branch>: <commit or file:line>: <what, and why it's better>
- add test: <what it should catch>
