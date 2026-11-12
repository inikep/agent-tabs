Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
REVIEW_FILES=<report files or glob> # reports to merge, e.g. /data/sh/LLM/reviews/PS-11934-8.4-H07/review-*.md
REVIEW_COMBINED=<report path>       # merged report, e.g. /data/sh/LLM/reviews/PS-11934-8.4-H07/review-combined.md

Default variables (used unless the caller sets them):
SOURCE_DIR=$PWD                     # git checkout with the reviewed target
PROMPT_DIR=<the directory of this file>  # where environment.md is


If a required variable is missing, stop and ask for it.

Merge the reviews in $REVIEW_FILES into $REVIEW_COMBINED. All reports must
review the same range: check that their target and base SHAs match and stop if
they don't. Below, TARGET means the target in the reports' front matter.
The reports follow the format in review.md, and so does this one where it applies.
Don't merge $REVIEW_COMBINED itself if $REVIEW_FILES matches it.

Environment: follow $PROMPT_DIR/environment.md. To check a finding by running
something, use your own worktree and build dir, with the worktree detached at
TARGET as review.md says. Reading code with `git -C $SOURCE_DIR show TARGET:path`
needs no worktree.

1. Group findings that have the same root cause: an overlapping location and the same
   mechanism. Different symptoms of one cause belong in one group; similar symptoms
   with different causes stay separate. If separate groups share code or a likely
   fix, keep them separate and cross-reference them under `related`.
2. For each group, write one finding in the same field format, with id C<n>, and add:
   - reported_by: <model ids with their finding ids, e.g. Opus-5.5-high-F2, GPT-5.4-high-F1>
   - status: confirmed | partly-confirmed | rejected | unverified, with a one-line
     reason (see step 3)
   - severity: the highest reported, plus a one-line reason if the reviewers disagree
   - merge_severity: your own assessment after checking, with a reason if it differs
     from severity
   - verification: the strongest reported (reproduced > code-read > suspected), or
     your own if you reproduced it
   - origin / commit / at_base: as in review.md; if reviewers disagree, or a reviewer
     says "introduced" without showing BASE's behaviour, settle it (run the scenario
     at BASE if you can) and say how
   - related: <other C ids, or "none">
   - disputes: any reviewer whose "Checked and correct" list contradicts the finding,
     or "none"
   Quote each reporter's evidence with its model id. Don't rewrite it as if you ran it.
3. Check every finding reported by only one model, and every finding whose origin or
   severity is disputed, against the code at TARGET. Run existing tests or a short
   scenario when the claim depends on behaviour (an existing test that covers the
   path can refute a code-read finding). Mark it confirmed, partly-confirmed (say
   which part holds), rejected (with the reason and evidence), or unverified. Don't
   drop anything silently.
4. Order findings by merge_severity, then by number of reporters. Put rejected
   findings last, under "## Rejected", keeping their C ids.
5. Add "## Fix plan": for each confirmed or partly-confirmed finding, where its fix
   belongs: fold it into the commit that introduced it, or a new commit for
   pre-existing defects, and the test that should catch it.
6. End with a coverage table: rows are the files or hunks changed in the range plus
   every area a finding names, and a last row with each model's test run and pass
   counts; columns are models; each cell says found (C<n>) / checked-ok /
   not-reviewed. Then give the overall verdict:
   - changes-requested: a confirmed or unverified introduced finding above nit
   - approve-with-nits: only introduced nits
   - approve: nothing introduced; list confirmed pre-existing findings as
     follow-ups either way.

Start the combined report with the same front matter as the reviews, with
`model: $MODEL_ID (merge)`, `built:` and `tests_run:` describing what you ran
for the checks (or "no" / "none"), and a `sources:` line listing the merged
report files.
