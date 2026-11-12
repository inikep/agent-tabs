Environment rules shared by the prompts in this directory. Your instructions file says
which git worktree and branch to work on, and may add rules of its own; where they
differ from these, it wins.

Default variables (used unless the caller or your instructions file sets them):
SOURCE_DIR=$PWD                     # the shared git checkout
WORKTREE_DIR=$SOURCE_DIR-$MODEL_ID  # your own git worktree
BUILD_DIR=$SOURCE_DIR-$MODEL_ID-deb-gcc16-rocks  # your own build dir

- Before anything else, record the start time with `date '+%F %T %Z'`; it goes
  under `started:` in your report or summary.
- Don't modify $SOURCE_DIR; it is shared with other agents. Make all source
  changes in your own git worktree $WORKTREE_DIR, set up as your instructions file
  says. Never push.
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
  *.cmake file changed. Record any extra flags under `built:` in your report or
  summary.
- Run MTR from $WORKTREE_DIR with the build's binaries: `cd $WORKTREE_DIR/mysql-test
  && MTR_BINDIR=$BUILD_DIR perl ./mysql-test-run.pl ...`.
- MTR on this machine: put --vardir under your build dir (not /tmp); pass
  `--mysqld=--innodb_use_native_aio=0` (the AIO limit is shared); Debug builds need
  `--debug-server`; never run two MTR instances on one build dir at once (keyring
  tests write config into the build tree); `export DEBUGINFOD_URLS=` so MTR's gdb
  backtrace after a crash does not hang on downloads, and if gdb hangs anyway kill
  it by PID (a `pkill -f` pattern can match your own shell). Re-run a failing test
  alone before deciding whether it is a real failure or a flaky test, and report
  both runs if the failure does not repeat.
- Work in parallel where safe: start `cmake` + `ninja -j32` in the background and
  read code, compare with the base, and write tests while it builds; batch read/git
  tool calls; pass several test names to one `mysql-test-run.pl --parallel` call to
  avoid repeated MTR startup overhead.
