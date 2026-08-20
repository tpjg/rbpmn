#!/usr/bin/env bash
#
# Gate 0c: run the DMN TCK against dsntk twice — once as published on
# crates.io, once as rbpmn actually ships it — and compare the two verdicts
# case by case (docs/dmn.md).
#
# The question is not "what does dsntk score". It is "does what we ship change
# a single answer", and that is only answerable by running both builds against
# the same corpus with the same runner. The pass/fail *totals* are the weaker
# half of the check: two runs can agree on the count and disagree on which
# cases failed, so the result files are compared byte for byte.
#
# `patched` used to mean "the published tarball with one crate substituted by
# `[patch.crates-io]`". It now means "rbpmn's dsntk fork, built from source",
# because that is what an application gets. The substitution moved into the
# fork as a feature (`use-fastnum`), and a `[patch]` could no longer express it
# anyway: the fork's version is `0.3.1-dev`, which does not satisfy the
# `^0.3.0` its own siblings request, so the patch silently did not apply. The
# assertions below caught that, which is the only reason this comment is not a
# bug report.
#
# The cost of the change, stated plainly: the comparison now has two variables
# rather than one — the decimal backend *and* 0.3.0 → 0.3.1-dev. Byte-identical
# verdicts prove both harmless together; a difference needs bisecting. That is
# the right trade, because "what we ship versus what upstream publishes" is the
# question worth gating on.
#
# Nothing here is vendored. The TCK corpus is separately OMG-licensed and is
# not ours to redistribute; the dsntk source and the test runner are fetched
# at their pinned revisions and land in gitignored directories. Versions are
# pinned and the crate tarball's checksum is verified before anything is
# extracted — the `just tla` discipline, for the same reason: an unpinned
# fetch would silently change what the verdict was measured against.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

DSNTK_VERSION=0.3.0
DSNTK_SHA256=d825c4d25e6b10d0393a606b2444a56c640cd137a0c1d465b29eff2e5e382d32
# rbpmn's fork, and the rev must match `crates/rbpmn-dmn/Cargo.toml` — this
# gate is worthless if it measures a dsntk nobody ships. `just dsntk-rev`
# checks the two agree.
DSNTK_FORK_URL=https://github.com/tpjg/dsntk
DSNTK_FORK_REV=37156c7
TCK_COMMIT=20274cd2ba9cad805db6114f331c743f4b2603a1
RUNNER_COMMIT=3276110236af1b9f1a0947a67bf421b283615c8f
PATCHES_COMMIT=8dfc54e8ab137b6d3875d8dc649b77a2604cb8d2
PORT=22022

# `--patched-corpus` additionally applies dsntk's own opinionated TCK patches,
# which is the corpus their published 3374/3391 was measured against. It
# changes the absolute numbers and not the comparison.
apply_tck_patches=false
[ "${1:-}" = "--patched-corpus" ] && apply_tck_patches=true

say() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

# ------------------------------------------------------------------ fetching

say "fetching pinned sources"

if [ ! -d tck ]; then
  git clone -q https://github.com/dmn-tck/tck.git tck
fi
git -C tck fetch -q --depth 1 origin "$TCK_COMMIT" 2>/dev/null || true
git -C tck checkout -q "$TCK_COMMIT" -- . 2>/dev/null || git -C tck checkout -q "$TCK_COMMIT"
git -C tck restore . 2>/dev/null || true
echo "  tck        $TCK_COMMIT"

if [ ! -d runner ]; then
  git clone -q https://github.com/DecisionToolkit/dsntk-test-runner.git runner
fi
git -C runner checkout -q "$RUNNER_COMMIT" 2>/dev/null || true
cp config.yml runner/config-rbpmn.yml
mkdir -p runner/output
echo "  runner     $RUNNER_COMMIT"

if $apply_tck_patches; then
  if [ ! -d patches ]; then
    git clone -q https://github.com/DecisionToolkit/dsntk-tck-patches.git patches
  fi
  git -C patches checkout -q "$PATCHES_COMMIT" 2>/dev/null || true
  ( cd patches && bash scripts/apply-patches.sh ../tck >/dev/null 2>&1 )
  echo "  patches    $PATCHES_COMMIT (applied to the corpus)"
fi

tarball="dsntk-$DSNTK_VERSION.crate"
if [ ! -f "$tarball" ]; then
  curl -sSfL -o "$tarball" "https://static.crates.io/crates/dsntk/$tarball"
fi
got=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
if [ "$got" != "$DSNTK_SHA256" ]; then
  echo "  checksum mismatch for $tarball"
  echo "    expected $DSNTK_SHA256"
  echo "    got      $got"
  exit 1
fi
echo "  dsntk      $DSNTK_VERSION (checksum verified)"

if [ ! -d "stock/dsntk-$DSNTK_VERSION" ]; then
  tar xzf "$tarball" -C stock
fi

if [ ! -d patched/dsntk-fork ]; then
  git clone -q "$DSNTK_FORK_URL" patched/dsntk-fork
fi
git -C patched/dsntk-fork fetch -q origin
git -C patched/dsntk-fork checkout -q "$DSNTK_FORK_REV"
echo "  dsntk fork $DSNTK_FORK_REV ($DSNTK_FORK_URL)"

# Which directory each build's workspace root lives in. `stock` is the
# extracted tarball's parent (it carries a `[workspace]` wrapper); `patched` is
# the fork's own checkout, which is already a workspace.
dir_for() {
  case "$1" in
  stock) echo "stock" ;;
  patched) echo "patched/dsntk-fork" ;;
  esac
}

# ------------------------------------------------------------------ building

for build in stock patched; do
  say "building dsntk ($build)"
  ( cd "$(dir_for "$build")" && cargo build --release --features tck -p dsntk 2>&1 | tail -1 )
done

# The substitution is only real if the C library is gone. Assert it rather
# than trust it: a `[patch]` that silently failed to apply would otherwise
# produce two identical builds and a triumphantly green comparison.
if grep -q 'name = "dfp-number-sys"' patched/dsntk-fork/Cargo.lock; then
  echo "patched build still links dfp-number-sys — the fork's use-fastnum default did not take"
  exit 1
fi
if ! grep -q 'name = "dfp-number-sys"' stock/Cargo.lock; then
  echo "stock build does NOT link dfp-number-sys — it is not the baseline it claims to be"
  exit 1
fi
# The other half of what rbpmn ships: no HTTP client, because `java-bridge` is
# off by default in the fork. Asserted here rather than assumed, for the same
# reason as the line above — a default that quietly flipped would leave this
# gate measuring a build nobody runs.
if grep -q 'name = "reqwest"' patched/dsntk-fork/Cargo.lock; then
  echo "patched build links reqwest — the fork's java-bridge feature is enabled"
  exit 1
fi
echo "  patched: dfp-number-sys absent, reqwest absent (fork defaults)"
echo "  stock:   dfp-number-sys present"

say "building the test runner"
( cd runner && cargo build --release 2>&1 | tail -1 )

# ------------------------------------------------------------------- running

run_tck() {
  local build=$1
  pkill -f "dsntk srv" 2>/dev/null || true
  sleep 1
  "$here/$(dir_for "$build")/target/release/dsntk" srv -H 127.0.0.1 -P "$PORT" \
    -D "$here/tck/TestCases" > "$here/$build.server.log" 2>&1 &
  local pid=$!
  for _ in $(seq 1 120); do
    curl -s -m 1 -o /dev/null "http://127.0.0.1:$PORT/tck" && break
    sleep 1
  done
  sleep 2
  ( cd runner && ./target/release/dsntk-test-runner config-rbpmn.yml > "$here/$build.runner.log" 2>&1 ) || true
  cp runner/output/compliance.csv "$here/$build.compliance.csv"
  cp runner/output/tck_results.csv "$here/$build.tck.csv"
  kill "$pid" 2>/dev/null || true
  pkill -f "dsntk srv" 2>/dev/null || true
  sleep 1
  grep -A 6 "Test cases:" "$here/$build.runner.log" | tail -5
}

for build in stock patched; do
  say "running the TCK ($build)"
  run_tck "$build"
done

# ------------------------------------------------------------------ verdict

say "verdict"
status=0
for f in compliance tck; do
  if cmp -s "stock.$f.csv" "patched.$f.csv"; then
    echo "  $f.csv: IDENTICAL — every case got the same verdict from both builds"
  else
    echo "  $f.csv: DIFFERS"
    diff <(sort "stock.$f.csv") <(sort "patched.$f.csv") | head -40
    status=1
  fi
done

# A comparison of two empty files is also "identical". Refuse to call that a
# pass.
cases=$(grep -c . patched.compliance.csv || echo 0)
if [ "$cases" -lt 3000 ]; then
  echo "  only $cases results — the corpus did not run; not a verdict"
  status=1
fi

if [ "$status" = 0 ]; then
  echo
  echo "  Gate 0c: PASS ($cases test results, byte-identical)"
else
  echo
  echo "  Gate 0c: FAIL"
fi
exit $status
