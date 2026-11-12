Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
BASE_BRANCH=<branch>                # the branch the implementations are built on, e.g. PS-11299-trunk
PLAN=<plan file>                    # the plan they implement
COMPARE_FILE=<comparison path>      # from implement-compare.md, e.g. /work/LLM-implementations/PS-11299-trunk/compare.md
OUTPUT_FILE=<summary path>          # e.g. /work/LLM-implementations/PS-11299-trunk/final.md

Default variables (used unless the caller sets them):
BRANCH=$BASE_BRANCH-$MODEL_ID       # your own branch from implement.md, if you implemented PLAN
FINAL_BRANCH=$BASE_BRANCH-final     # the improved branch, created on START
START=<the best branch>             # picked below
SOURCE_DIR=$PWD                     # git checkout with BASE_BRANCH and the models' branches
PROMPT_DIR=<the directory of this file>  # where implement.md and environment.md are
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # your own build dir

If a required variable is missing, stop and ask for it.

Select the best branch as a base (START), create FINAL_BRANCH on it, and improve it using
$COMPARE_FILE. Start from the comparison's `best:` and its "## Best base" reasons.
If checking the code shows another branch is a better base, use that one and say why
under `## Deviations`.

Environment rules: those in $PROMPT_DIR/environment.md, and
- Work only in your own git worktree $WORKTREE_DIR, on your own new branch
  FINAL_BRANCH, created on START:
  - if FINAL_BRANCH already exists (`git -C $SOURCE_DIR rev-parse --verify FINAL_BRANCH`),
    stop and ask; it may hold an earlier run's work.
  - if $WORKTREE_DIR doesn't exist, create it:
    `git -C $SOURCE_DIR worktree add -b FINAL_BRANCH $WORKTREE_DIR START`
  - if it exists, check that it is a worktree of $SOURCE_DIR's repository
    (`git -C $SOURCE_DIR worktree list`) and has no uncommitted changes (stop and
    ask if it has), then switch it, leaving the branch it was on as it is:
    `git -C $WORKTREE_DIR checkout -b FINAL_BRANCH START`
  Make all source changes there, on FINAL_BRANCH. Never change BASE_BRANCH, BRANCH
  or other models' branches; take code from them with `git cherry-pick`,
  `git show <branch>:path` or by hand.
- Build in $BUILD_DIR as environment.md says. An existing $BUILD_DIR is reused:
  ninja rebuilds what changed.

Implementation rules
- Make every change listed under the comparison's "## Improvements" for the branch
  you picked, most important first. If you pick another branch, work out its
  improvements the same way from the comparison's findings and the other branches.
  Check each against the code before making it; skip one that turns out wrong and
  say why.
- FINAL_BRANCH is new, so you may rewrite its history: fold each fix into the commit
  it belongs to, and add new commits only for new steps or tests. Every commit must
  build and pass its tests. Keep each commit's existing trailers and add
  `Improved by <Model> <X.Y> <reasoning level>: <what changed>` to each commit you
  change; new commits end with `Implemented by <Model> <X.Y> <reasoning level>`
  (MODEL_ID with spaces, e.g. `Implemented by Opus 5.5 high`).
- Run the tests of the code you changed and every test the branches added.
- Leave no uncommitted changes. Before finishing, check that $BUILD_DIR is built from
  FINAL_BRANCH's HEAD and that the tests pass there.

Summary (write it to $OUTPUT_FILE and print it as your final answer; keep the field
names exactly)

---
model: <MODEL_ID> (improve)
branch: <FINAL_BRANCH> @ <full SHA>
from: <START> @ <full SHA>
base: <BASE_BRANCH> @ <full SHA>
plan: <PLAN>
comparison: <COMPARE_FILE>
started: <YYYY-MM-DD HH:MM:SS TZ, when you received the prompt>
finished: <YYYY-MM-DD HH:MM:SS TZ, when you wrote the summary>
duration: <finished minus started, in hours and minutes, e.g. 1h05m>
status: done | partial | blocked
built: <build dir, type and the SHA it was built from, or "no">
tests_run: <exact MTR/gunit commands and pass/fail counts, or "none">
---

## Commits
- <short SHA> <subject>: <what it does, which PLAN step, and what you changed in it>

## Improvements
- <each item from the comparison's "## Improvements">: done (<commit>) | skipped (why)

## Deviations
- <where you departed from the comparison or PLAN, and why; or "none">

## Not done
- <anything in PLAN that is still not implemented or not tested, and why; or "none">
