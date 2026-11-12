Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
BASE_BRANCH=<branch>                # the branch to build on, e.g. PS-11299-trunk
PLAN=<plan file>                    # what to implement, e.g. /work/LLM-plans/PS-11299-port-vector-to-26.07/plan5-Opus-5.5-high.md
OUTPUT_FILE=<summary path>          # e.g. /work/LLM-implementations/PS-11299-trunk/implement-$MODEL_ID.md

Default variables (used unless the caller sets them):
BRANCH=$BASE_BRANCH-$MODEL_ID       # your own branch with the implementation, created on BASE_BRANCH
SOURCE_DIR=$PWD                     # git checkout with BASE_BRANCH and the models' branches
PROMPT_DIR=<the directory of this file>  # where implement.md and environment.md are
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # your own build dir

If a required variable is missing, stop and ask for it.

On top of BASE_BRANCH create BRANCH and implement PLAN there. Other models implement
the same plan at the same time, each in its own worktree and branch, and the branches
will be compared using your summaries, so keep to your own worktree, branch and build
dir, and make the summary accurate.

Environment rules: those in $PROMPT_DIR/environment.md, and
- Work only in your own git worktree $WORKTREE_DIR, on your own new branch BRANCH,
  created on BASE_BRANCH:
  - if BRANCH already exists (`git -C $SOURCE_DIR rev-parse --verify BRANCH`),
    stop and ask; it may hold an earlier run's work.
  - if $WORKTREE_DIR doesn't exist, create it:
    `git -C $SOURCE_DIR worktree add -b BRANCH $WORKTREE_DIR BASE_BRANCH`
  - if it exists, check that it is a worktree of $SOURCE_DIR's repository
    (`git -C $SOURCE_DIR worktree list`) and has no uncommitted changes (stop and
    ask if it has), then switch it, leaving the branch it was on as it is:
    `git -C $WORKTREE_DIR checkout -b BRANCH BASE_BRANCH`
  Make all source changes there, on BRANCH. Never change BASE_BRANCH or other
  models' branches.
- Build in $BUILD_DIR as environment.md says. An existing $BUILD_DIR is reused:
  ninja rebuilds what changed.

Implementation rules
- Read PLAN fully before changing anything, and follow it. Where it is wrong,
  unclear or doesn't match the code, make the smallest sensible change and record
  it under `deviations:` in the summary; stop and ask only if you can't go on.
- Commit in the steps PLAN describes (or in logical steps if it gives none), each
  commit building and passing its tests. Write commit messages in the style of
  BASE_BRANCH's history, and end each with the trailer
  `Implemented by <Model> <X.Y> <reasoning level>` (MODEL_ID with spaces, e.g.
  `Implemented by Opus 5.5 high`).
- Add or update the tests PLAN asks for, and tests for every behaviour you add or
  fix. Run them and the existing tests of the code you changed.
- Leave no uncommitted changes. Before finishing, check that $BUILD_DIR is built from
  BRANCH's HEAD (`git -C $WORKTREE_DIR rev-parse HEAD`) and that the tests pass there.

Summary (write it to $OUTPUT_FILE, creating its directory if needed, and print it as your
final answer; keep the field names exactly)

---
model: <MODEL_ID>
branch: <BRANCH> @ <full SHA>
base: <BASE_BRANCH> @ <full SHA>
plan: <PLAN>
started: <YYYY-MM-DD HH:MM:SS TZ, when you received the prompt>
finished: <YYYY-MM-DD HH:MM:SS TZ, when you wrote the summary>
duration: <finished minus started, in hours and minutes, e.g. 1h05m>
status: done | partial | blocked
built: <build dir, type and the SHA it was built from, or "no">
tests_run: <exact MTR/gunit commands and pass/fail counts, or "none">
---

## Commits
- <short SHA> <subject>: <what it does, which PLAN step>

## Deviations
- <where you departed from PLAN, and why; or "none">

## Not done
- <anything in PLAN that is not implemented or not tested, and why; or "none">
