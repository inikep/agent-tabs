Required variables (the caller must set these):
MODEL_ID=<Model-X.Y-level>          # your own model, version and reasoning level, e.g. Opus-5.5-high
BASE_BRANCH=<branch>                # the branch to build on, e.g. PS-11299-trunk
PLAN=<plan file>                    # what to implement, e.g. /work/LLM-plans/PS-11299-port-vector-to-26.07/plan5-Opus-5.5-high.md

Default variables (used unless the caller sets them):
BRANCH=$BASE_BRANCH-$MODEL_ID       # your own branch, created on top of BASE_BRANCH
SOURCE_DIR=$PWD                     # git checkout with BASE_BRANCH
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree on BRANCH; always used
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # your build dir

If a required variable is missing, stop and ask for it.

On top of BASE_BRANCH create BRANCH and implement PLAN there. Other models implement
the same plan at the same time, each in its own worktree and branch, and the branches
will be compared, so keep to your own worktree, branch and build dir.

Environment rules
- Before anything else, record the start time with `date '+%F %T %Z'`; it goes
  under `started:` in the summary.
- Don't modify $SOURCE_DIR; it is shared with other agents. Always work in your
  own git worktree $WORKTREE_DIR, on BRANCH:
  - if BRANCH already exists (`git -C $SOURCE_DIR rev-parse --verify BRANCH`),
    stop and ask; it may hold an earlier run's work.
  - if $WORKTREE_DIR doesn't exist, create it:
    `git -C $SOURCE_DIR worktree add -b BRANCH $WORKTREE_DIR BASE_BRANCH`
  - if it exists, reuse it: check that it is a worktree of $SOURCE_DIR's repository
    (`git -C $SOURCE_DIR worktree list`), then `git -C $WORKTREE_DIR checkout -b
    BRANCH BASE_BRANCH`. If it has uncommitted changes from an earlier run, stop and
    ask before discarding them.
  Make all source changes there. Never push, and never change BASE_BRANCH or other
  models' branches.
- Build in $BUILD_DIR, configured out of tree from $WORKTREE_DIR with:

    CC=gcc-16 CXX=g++-16 cmake -GNinja $WORKTREE_DIR \
      -DCMAKE_BUILD_TYPE=Debug \
      -DCMAKE_C_COMPILER_LAUNCHER=ccache \
      -DCMAKE_CXX_COMPILER_LAUNCHER=ccache \
      -DMYSQL_MAINTAINER_MODE=OFF \
      -DDOWNLOAD_BOOST=1 \
      -DWITH_BOOST=/work/mysql-server/_deps \
      -DWITH_ROCKSDB=ON \
      -DWITH_PAM=ON \
      -DENABLE_DOWNLOADS=1 \
      -DWITH_READLINE=system \
      -DWITH_CURL=system \
      -DCMAKE_EXE_LINKER_FLAGS=-fuse-ld=gold \
      -DCMAKE_SHARED_LINKER_FLAGS=-fuse-ld=gold \
      -DCMAKE_MODULE_LINKER_FLAGS=-fuse-ld=gold
    ninja -j32

  Never build or run MTR with more than 32 jobs (always pass -j32 or lower, and
  --parallel <= 32); the machine is shared. Re-run cmake when a CMakeLists.txt or
  *.cmake file changed. Record any extra flags under `built:` in the summary.
- Run MTR from $WORKTREE_DIR with the build's binaries: `cd $WORKTREE_DIR/mysql-test
  && MTR_BINDIR=$BUILD_DIR perl ./mysql-test-run.pl ...`.
- MTR on this machine: put --vardir under your build dir (not /tmp); pass
  `--mysqld=--innodb_use_native_aio=0` (the AIO limit is shared); Debug builds need
  `--debug-server`; never run two MTR instances on one build dir at once (keyring
  tests write config into the build tree); `export DEBUGINFOD_URLS=` so MTR's gdb
  backtrace after a crash does not hang on downloads, and if gdb hangs anyway kill
  it by PID (a `pkill -f` pattern can match your own shell). Re-run a failing test
  alone before deciding it is your change's fault or a flaky test.
- Work in parallel where safe: start `cmake` + `ninja -j32` in the background and
  read code, compare with BASE_BRANCH, and write tests while it builds; batch
  read/git tool calls; pass several test names to one `mysql-test-run.pl --parallel`
  call to avoid repeated MTR startup overhead.

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

Summary (print it as your final answer; keep the field names exactly)

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
