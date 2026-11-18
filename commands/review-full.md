Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
TARGET=<branch or commit>           # e.g. PS-11934-8.4-H07
BASE=<base commit>                  # e.g. percona/8.4
OUTPUT_FILE=<report path>           # e.g. /data/sh/LLM/reviews/$TARGET/review-$MODEL_ID.md

Default variables (used unless the caller sets them):
SOURCE_DIR=$PWD                     # git checkout with TARGET
PROMPT_DIR=<the directory of this file>  # where environment.md is
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree at TARGET; always used
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # only if you need a build

If a required variable is missing, stop and ask for it.

Review the commits in BASE..TARGET for correctness. Other models review the same
range at the same time, and the reports will be merged by script and by another
model, so follow the format below exactly.

Environment rules: those in $PROMPT_DIR/environment.md, and
- Work in your own git worktree $WORKTREE_DIR, at TARGET's SHA, detached (TARGET's
  branch may be checked out in $SOURCE_DIR):
  - if it doesn't exist, create it:
    `git -C $SOURCE_DIR worktree add --detach $WORKTREE_DIR TARGET`
  - if it exists, reuse it: check that it is a worktree of $SOURCE_DIR's repository
    (`git -C $SOURCE_DIR worktree list`), then `git -C $WORKTREE_DIR checkout
    --detach TARGET`. If it has uncommitted changes from an earlier run, stop and
    ask before discarding them.
  Make all source changes (revert checks, mutants, extra tests) there.
- Build only if you need to.
- Check that $WORKTREE_DIR is at TARGET's SHA (`git -C $WORKTREE_DIR rev-parse HEAD`)
  before building and again after testing; record that SHA under `built:`.
- Keep extra or modified tests in $WORKTREE_DIR. Quote the essential lines of such
  tests under evidence and give their path, since the worktree may be removed later.
- To revert a hunk or add a debug hook, change $WORKTREE_DIR and rebuild only the
  affected target (for example the component) into a separate output, then swap it
  into your build dir and restore it after the run.
- While it builds, also use subagents for research; combine reproduction and
  control scenarios into one `.test` file; pre-build mutant `.so`/binary variants
  up front before swapping and running only the relevant tests for each.
- Write the report to $OUTPUT_FILE and print nothing else of substance. Just
  before writing it, run `date '+%F %T %Z'` again for `finished:`, and put the
  difference under `duration:`.

Review rules
- Anchor every location to TARGET's tree (`git show TARGET:path`), not to the working
  tree. Use repo-relative paths and the line number in TARGET.
- One finding = one root cause. Don't bundle unrelated problems, and don't split one
  root cause into several findings.
- Say whether the defect was introduced in BASE..TARGET or already existed before it.
  Decide this from BASE's behaviour, not from where the code changed: run the same
  scenario against BASE (or with the range's changes to those files reverted), or
  show from BASE's code why it behaves differently. If BASE fails the same way and
  the range only changes the cost or the message, the finding is pre-existing.
- In a range of several commits, name the commit that introduced the defect, so the
  fix can be folded into it.
- Prefer evidence over reasoning: a failing test, a crash with a stack trace, or a
  revert/mutant check. State exactly what you ran. When the evidence needs a hook,
  a sleep or a mutant, also run a control (the same test without the trigger, or on
  the unmodified build) and quote both results.
- Before reporting a code-read finding, look for existing tests that cover the path,
  including .combinations variants, and run them. A passing test that exercises the
  scenario refutes the finding.
- Check that the range's tests cover its changes: for each hunk that fixes or adds
  behaviour, revert that hunk alone and run the range's tests. A hunk whose revert
  no test catches is a test-gap finding (minor); name the hunk and the mutant.
- For races, widen the window with a temporary DBUG_EXECUTE_IF sleep or hook in
  your worktree; don't rely on timing alone.
- Report pre-existing defects only in code the range changes or relies on.
- mysqltest: `--error 0,ER_X` does not print the error when it occurs; use
  `--disable_abort_on_error` to see what a statement returned.
- Also report what you checked and found correct, so the merge knows what was covered.

Severity (use exactly these words)
- blocker: crash, data loss, security hole, or wrong results in a normal configuration
- major:   wrong behaviour that needs an unusual but realistic configuration or failure
- minor:   wrong behaviour in rare double faults, misleading status or messages,
           test gaps, or extra work with correct results (major if unbounded)
- nit:     style, naming, comments, or commit message wording

Verification (use exactly these words)
- reproduced: shown by a command whose output is quoted under evidence
- code-read:  follows from the code, but nothing was run
- suspected:  plausible, not confirmed

Report format (Markdown; keep the headings and field names exactly)

---
model: <MODEL_ID>
target: <TARGET> @ <full SHA>
base: <BASE> @ <full SHA>
date: <YYYY-MM-DD>
started: <YYYY-MM-DD HH:MM:SS TZ, when you received the prompt>
finished: <YYYY-MM-DD HH:MM:SS TZ, when you wrote the final report>
duration: <finished minus started, in hours and minutes, e.g. 1h05m>
verdict: approve | approve-with-nits | changes-requested
built: <build dir, type and the SHA it was built from, or "no">
tests_run: <exact MTR/gunit commands and pass/fail counts, or "none">
---

## Findings

### <MODEL_ID>-F1
- title: <one line, at most 80 chars, states the defect, not the fix>
- severity: blocker | major | minor | nit
- origin: introduced | pre-existing
- commit: <short SHA and subject of the commit that introduced it, or "before BASE">
- at_base: <what the same scenario does at BASE and how you know (ran / code-read)>
- verification: reproduced | code-read | suspected
- location: <path>:<line>[-<line>] (<function>)
- also_affects: <other path:line, or "none">
- mechanism: <2-4 sentences: the root cause in code terms>
- scenario: <concrete steps or inputs, then the wrong result or crash>
- evidence: <command and the important output lines, or "none">
- fix: <smallest correct change, 1-3 sentences; no patch unless it is under 10 lines>
- test: <how a test can catch it, or which existing test should have>

(repeat for F2, F3, …; numbers are never reused, and an empty list is written "none")

## Checked and correct
- <path or area>: <what was checked> (<how: code-read | ran <test> |
  mutant <change> caught by <test>>)

## Not reviewed
- <anything in scope that was skipped, and why>
