Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
REVIEW_FILES=<report files or glob> # reports to merge, e.g. /data/sh/LLM/reviews/PS-11934-8.4-H07/review-*.md
REVIEW_COMBINED=<report path>       # merged report, e.g. /data/sh/LLM/reviews/PS-11934-8.4-H07/review2-combined-opus-5.5.md

Default variables (used unless the caller sets them):
SOURCE_DIR=$PWD                     # git checkout with the reviewed target
PROMPT_DIR=<the directory of this file>  # where environment.md is
CVSS_SKILL_DIR=~/.claude/skills/oracle-cvss-scoring  # the oracle-cvss-scoring skill


If a required variable is missing, stop and ask for it.

Merge the reviews in $REVIEW_FILES into $REVIEW_COMBINED. All reports must
review the same range: check that their target and base SHAs match and stop if
they don't. Below, TARGET means the target in the reports' front matter.
The reports follow the format in review.md, and so does this one where it applies.

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
   - cvss: the CVSS v3.1 base vector, score and rating of the defect as you
     assess it after checking, e.g. `CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:U/C:N/I:N/A:H
     6.5 (Medium)`, and a one-line reason for each metric that isn't obvious.
     Compute the score with the v3.1 base formula (roundup to one decimal) and
     check it against the published examples for that vector if you are unsure.
     Score what an attacker or a faulty client can actually do at TARGET:
     - AV:N when an SQL client or a replication/router peer can trigger it,
       AV:L when it needs a shell on the host, a config file or a crafted data
       file.
     - PR:N before authentication, PR:L for any authenticated user, PR:H when
       it needs SUPER, SYSTEM_VARIABLES_ADMIN, INSTALL COMPONENT or a similar
       admin privilege; UI:R only when someone else must run something.
     - A:H for a server crash or hang, A:L for a failed statement or session;
       I:H for data loss or wrong results that are stored, I:L for wrong
       results or status shown to the client; C when data the user may not
       read is exposed.
     - S:C only when the impact reaches beyond the server process, e.g. the OS
       or another server.
     A finding with no confidentiality, integrity or availability impact (test
     gaps, messages, extra work, nits) is `CVSS:3.1/…/C:N/I:N/A:N 0.0 (None)`.
     A rejected finding gets `cvss: n/a`.
     Score `cvss` for every finding before you read the skill below, so this
     score is your own and not anchored on it.
   - cvss_oracle: a second CVSS v3.1 base score, made with the
     oracle-cvss-scoring skill: the way Oracle scores MySQL in its Critical
     Patch Updates. Claude can load the skill by name; any other model reads
     $CVSS_SKILL_DIR/SKILL.md (the rules table) and
     $CVSS_SKILL_DIR/scoring_brief.md (the rules as applied). Use only their
     scoring rules: score from the finding and your checks, not from a Jira
     ticket, and don't post comments, change priorities or look anything up
     on the web. Compute the score with
     `python3 $CVSS_SKILL_DIR/cvss_tool.py calc <vector>` instead of by hand.
     Give the vector, score and rating, the Oracle-style component (InnoDB,
     Server: Optimizer, Server: Replication, Client: mysqldump, …) and, for
     each metric that differs from `cvss`, the skill's rule that changes it,
     e.g. `PR:H (needs a table of its own)`. A rejected finding gets
     `cvss_oracle: n/a`.
   - verification: the strongest reported (reproduced > code-read > suspected), or
     your own if you reproduced it
   - origin / commit / at_base: as in review.md; if reviewers disagree, or a reviewer
     says "introduced" without showing BASE's behaviour, settle it (run the scenario
     at BASE if you can) and say how
   - author: oracle | percona | both — who wrote the defective code, decided
     from git, not from the file's name or copyright header:
     - Find the upstream MySQL release TARGET is based on:
       `git -C $SOURCE_DIR describe --tags --abbrev=0 --match 'mysql-[0-9]*' TARGET`
       (Oracle's release tags, e.g. mysql-8.4.9).
     - Blame the defective lines at TARGET
       (`git -C $SOURCE_DIR blame -L <lines> TARGET -- <path>`) and, for each
       commit it names, check `git -C $SOURCE_DIR merge-base --is-ancestor
       <commit> <tag>`: an ancestor of the tag is oracle, any other commit is
       percona. Compare `git show <tag>:<path>` with TARGET to confirm.
     - oracle: the defect is in that release unchanged; both: Percona changed
       Oracle's code and the defect needs both parts; percona: Percona's code
       alone.
     For oracle and both, also say whether a later upstream release already
     changed that code (`git -C $SOURCE_DIR log --oneline <tag>..<latest
     mysql-X.Y.Z tag of the same series> -- <path>`; fetch the `mysql` remote's
     tags if they are missing), and name the release and commit that fixed it.
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
4. Order findings by merge_severity, then by the `cvss` score, then by
   number of reporters. Put rejected
   findings last, under "## Rejected", keeping their C ids.
5. Add "## Fix plan": for each confirmed or partly-confirmed finding, where its fix
   belongs: fold it into the commit that introduced it, or a new commit for
   pre-existing defects, and the test that should catch it. For an oracle
   defect, say whether to take upstream's fix (when a later release has one),
   patch it in Percona, or report it to Oracle (bugs.mysql.com) as well.
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
