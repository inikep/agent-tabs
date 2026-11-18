Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
TARGET=<branch or commit>           # e.g. PS-11934-8.4-H07
BASE=<base commit>                  # e.g. percona/8.4
OUTPUT_FILE=<report path>           # e.g. /data/sh/LLM/reviews/$TARGET/read-review-$MODEL_ID.md

Default variables (used unless the caller sets them):
SOURCE_DIR=$PWD                     # git checkout with TARGET

If a required variable is missing, stop and ask for it.

Review the commits in BASE..TARGET for correctness by reading the code. Other
models review the same range at the same time, and the reports will be merged
by script and by another model, so follow the format below exactly.

This review is read-only:
- Read with `git -C $SOURCE_DIR` only: `show TARGET:path`, `diff BASE TARGET`,
  `log`, `blame`, `grep TARGET`. Don't check out, create a worktree, fetch, build
  or run tests, and don't change anything in $SOURCE_DIR (TARGET's branch may be
  checked out there, and other agents use it).
- The only file you write is $OUTPUT_FILE (create its directory if needed).
- Record the start time with `date '+%F %T %Z'` before anything else, and run it
  again just before writing the report.

Review rules
- Anchor every location to TARGET's tree (`git show TARGET:path`), not to the
  working tree. Use repo-relative paths and the line number in TARGET.
- One finding = one root cause. Don't bundle unrelated problems, and don't split
  one root cause into several findings.
- Say whether the defect was introduced in BASE..TARGET or already existed: read
  the same code at BASE (`git show BASE:path`) and say why BASE behaves the same
  or differently. In a range of several commits, name the commit that
  introduced it.
- Before reporting a finding, look for existing tests that cover the path,
  including .combinations variants, and read their .result files: a test whose
  recorded result shows the scenario working refutes the finding.
- For each hunk that fixes or adds behaviour, name the test in the range that
  covers it. A hunk no test covers is a test-gap finding (minor).
- Report pre-existing defects only in code the range changes or relies on.
- Quote the lines that show the defect under evidence; don't claim anything ran.
- Also report what you checked and found correct, so the merge knows what was
  covered.

Severity (use exactly these words)
- blocker: crash, data loss, security hole, or wrong results in a normal configuration
- major:   wrong behaviour that needs an unusual but realistic configuration or failure
- minor:   wrong behaviour in rare double faults, misleading status or messages,
           test gaps, or extra work with correct results (major if unbounded)
- nit:     style, naming, comments, or commit message wording

Verification (use exactly these words)
- code-read:  follows from the code you quote
- suspected:  plausible, not confirmed

Report format (Markdown; keep the headings and field names exactly)

---
model: <MODEL_ID>
target: <TARGET> @ <full SHA>
base: <BASE> @ <full SHA>
date: <YYYY-MM-DD>
started: <YYYY-MM-DD HH:MM:SS TZ, when you received the prompt>
finished: <YYYY-MM-DD HH:MM:SS TZ, when you wrote the final report>
duration: <finished minus started, in hours and minutes, e.g. 0h25m>
verdict: approve | approve-with-nits | changes-requested
built: no (read-only review)
tests_run: none (read-only review)
---

## Findings

### <MODEL_ID>-F1
- title: <one line, at most 80 chars, states the defect, not the fix>
- severity: blocker | major | minor | nit
- origin: introduced | pre-existing
- commit: <short SHA and subject of the commit that introduced it, or "before BASE">
- at_base: <what the same code does at BASE, from `git show BASE:path`>
- verification: code-read | suspected
- location: <path>:<line>[-<line>] (<function>)
- also_affects: <other path:line, or "none">
- mechanism: <2-4 sentences: the root cause in code terms>
- scenario: <concrete steps or inputs, then the wrong result or crash>
- evidence: <the quoted code lines with path:line, or "none">
- fix: <smallest correct change, 1-3 sentences; no patch unless it is under 10 lines>
- test: <how a test can catch it, or which existing test should have>

(repeat for F2, F3, …; numbers are never reused, and an empty list is written "none")

## Checked and correct
- <path or area>: <what was checked> (code-read, or the test whose .result covers it)

## Not reviewed
- <anything in scope that was skipped, and why>
