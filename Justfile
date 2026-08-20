default: test

test:
    cargo test

# `--workspace` rather than the default members, so the benchmark harness is
# linted too: it is kept out of `cargo test` deliberately, and a crate nobody
# lints is a crate that drifts.
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --check

# The build **without** DMN, which is the only thing keeping "optional" a fact.
#
# DMN is on by default: a definition plus its manifest is a fully executable
# flow, and a decision is part of that definition. But the seam has to stay
# real — it is what keeps dsntk out of `rbpmn-model` and `rbpmn-core`, which
# must compile to wasm32, and it is the supported way to build without a 1.94
# toolchain. A feature nobody builds is a feature that has already rotted; that
# is exactly how the *previous* arrangement failed, claiming opt-in while every
# default build compiled dsntk.
#
# The dependency assertions are the load-bearing half. Clippy and the tests
# would pass just as happily with dsntk linked in.
no-dmn:
    #!/usr/bin/env bash
    set -euo pipefail
    for crate in rbpmn-server rbpmn-wasm; do
      # The tree is captured in its own step so a `cargo tree` that *fails*
      # is fatal. Written as `$(cargo tree ... | grep -c dsntk || true)` it was
      # not: grep prints "0" for a broken tree exactly as it does for a clean
      # one, and `|| true` swallowed cargo's exit code — so a resolver error,
      # a bad manifest or an offline registry all reported the seam intact.
      # The same hole as "a differential that skipped is a differential that
      # passed", in the recipe written to stop that class of rot.
      off=$(cargo tree --manifest-path crates/$crate/Cargo.toml --no-default-features -e normal)
      n=$(printf '%s' "$off" | grep -c dsntk || true)
      [ "$n" = "0" ] || { echo "$crate --no-default-features still links $n dsntk crates"; exit 1; }
      # The mirror, and not a formality: both directions have been quietly
      # wrong here. Feature unification means one dependency that takes the
      # defaults switches `dmn` back on for the whole build — a self
      # dev-dependency did exactly that, and `--no-default-features` ran the
      # DMN tests it was meant to prove could be left out.
      on=$(cargo tree --manifest-path crates/$crate/Cargo.toml -e normal)
      m=$(printf '%s' "$on" | grep -c dsntk || true)
      [ "$m" != "0" ] || { echo "$crate no longer links dsntk by default"; exit 1; }
      # ...and the dsntk it links must be the fork's *defaults*, which is where
      # the two properties that used to be `[patch.crates-io]` now live.
      #
      # This replaces a build script in `rbpmn-dmn` that proved the same thing
      # from inside the build. The patch is gone, so that guard is gone with
      # it; the property is not, and unlike a patch a feature can be switched
      # back on by any crate in the graph. Cargo unifies features, so one
      # dependency asking for `java-bridge` would restore an HTTP client for
      # everyone — silently, and only here would it show.
      c=$(printf '%s' "$on" | grep -c dfp-number-sys || true)
      [ "$c" = "0" ] || { echo "$crate links Intel's decimal C library — use-fastnum is not in effect"; exit 1; }
      j=$(printf '%s' "$on" | grep -c 'reqwest v0.13' || true)
      [ "$j" = "0" ] || { echo "$crate links reqwest 0.13 — dsntk's java-bridge feature got switched on"; exit 1; }
    done
    cargo clippy -p rbpmn-server --no-default-features --all-targets -- -D warnings
    cargo clippy -p rbpmn-engine --no-default-features --all-targets -- -D warnings
    # `rbpmn-wasm` too, or its `#[cfg(not(feature = "dmn"))]` arms — the
    # refusal path for bundled decision artifacts — are the one part of the
    # seam nothing ever compiles. Asserting its dependency graph above while
    # never type-checking the code that graph selects is half a check.
    cargo clippy -p rbpmn-wasm --no-default-features --all-targets -- -D warnings
    # ...and for **wasm32**, which is the target that makes the seam matter at
    # all. Inherited from the MSRV recipe that used to do this incidentally on
    # an old toolchain; that recipe is gone (one floor now, docs/dmn.md D9) but
    # this check is not, because nothing else builds this crate without DMN for
    # the browser.
    rustup target add wasm32-unknown-unknown
    cargo check -p rbpmn-wasm --no-default-features --target wasm32-unknown-unknown
    cargo test -p rbpmn-engine --no-default-features


fmt:
    cargo fmt

# Run the HTTP server with a fresh throwaway token (printed for curl use).
serve:
    #!/usr/bin/env bash
    set -euo pipefail
    psql -h localhost postgres -tc "select 1 from pg_database where datname = 'rbpmn_dev'" | grep -q 1 || psql -h localhost postgres -c "create database rbpmn_dev"
    export RBPMN_DATABASE_URL="postgres://$USER@localhost:5432/rbpmn_dev"
    export RBPMN_API_TOKEN=$(openssl rand -hex 32)
    echo "token: $RBPMN_API_TOKEN"
    cargo run -p rbpmn-server

# Build the WASM linter: web target (playground) + node target (bpmnlint plugin).
#
# `--features dmn` on both even though it is now the default, and the native
# dump in `parity` names it the same way. The redundancy is the point: both
# sides of the parity check must ask for the feature identically, so a build
# that ever loses it fails per fixture instead of quietly comparing two
# different validators.
wasm:
    wasm-pack build crates/rbpmn-wasm --target web --out-dir ../../playground/src/wasm --no-typescript --features dmn
    wasm-pack build crates/rbpmn-wasm --target nodejs --out-dir ../../bpmnlint-plugin-rbpmn/wasm --no-typescript --features dmn

# Linter playground: fixture browser + live lint at http://localhost:5173.
playground: wasm
    cd playground && npm install && npm run dev

# Build the two UI documents into crates/rbpmn-ui/assets/. Build output, so
# gitignored: run this once after cloning, and again after touching ui/ or the
# linter crates the editor embeds. rbpmn-ui's build.rs says so if you forget.
ui:
    cd ui && npm install && npm run build

# The UI's own unit tests: the pure modules (diagnosis, manifest parsing,
# model facts, the L3 subtraction), no browser and no build artifacts needed.
ui-test:
    cd ui && npm install && npm test

# Write both documents to disk so they can be opened directly — the editor is
# usable with no server at all, and this is how you check that.
#
# Depends on `ui` so what lands in ui/dist is built from the sources as they
# are now. Writing the documents without rebuilding is how you end up
# debugging a fix that was never in the file you opened.
ui-dist: ui
    cargo run -p rbpmn-ui --example write-documents

# Live demo: a real engine, a real instance frozen on an incident, and two
# clickable links. Stays up until Ctrl-C. Needs a local Postgres.
#
# It runs a tiny auth-injecting reverse proxy in front, because that is the
# documented posture: rbpmn's UI routes are behind the bearer, a browser
# cannot send that on a navigation, and supplying it is the embedding
# application's job.
demo: ui
    python3 e2e/demo.py

# Browser checks for the two documents, driven from file://. The Rust tests
# pin their structure and escaping; none of that notices a blank canvas, so
# this opens the real files and drives them. Also the only place the CSP is
# enforced for real. Needs python3 + playwright.
e2e-ui: ui-dist
    python3 e2e/ui.py

# The playground-never-lies check (Rust vs WASM byte parity) + the bpmnlint
# plugin's corpus test. Run before releasing anything WASM-facing.
#
# Both sides must carry the same features. `dmn` is a *default* feature of
# rbpmn-wasm, so `just wasm` and the native dump both get it and the only way
# to mismatch is an explicit `--no-default-features` on one side — which this
# check catches rather than skips: without the feature the validator refuses
# bundled decisions instead of staying silent, so all 17 DMN fixtures differ
# and parity fails per fixture. Verified by building one side each way.
parity: wasm
    cd playground && npm install && npm run parity
    cd bpmnlint-plugin-rbpmn && npm install && npm test

# TLA+ model checking of the concurrency protocol (spec/README.md). Needs
# java; fetches tla2tools.jar into spec/ on first use (gitignored). The
# version is pinned and the download is checksum-verified before java ever
# runs it: an unpinned `releases/latest` would silently change what executes
# on a developer's machine and would make past hold/fail verdicts
# irreproducible. To move to a new TLA+ release, bump both constants — the
# recipe refuses to run on a mismatch rather than trusting the download.
# Eight of the thirteen configs are EXPECTED to fail — they are the
# counterexamples that show the checks have teeth, and three of them
# reproduce bugs that were real (the AB/BA timer-claim sketch, the phase-6
# scope teardown that left a timer row behind, and release_task guarded by
# owner alone, which shipped and which a review caught before the model
# could — the model had no notion of a retried request to catch it with).
tla:
    #!/usr/bin/env bash
    set -euo pipefail
    cd spec
    version=v1.7.4
    sha=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
    jar=.tla2tools-$version.jar
    verify() { # jar -> 0 when it matches the pinned checksum
        local got
        got=$(shasum -a 256 "$1" | cut -d' ' -f1)
        [ "$got" = "$sha" ] || { echo "  checksum mismatch for $1"; echo "    expected $sha"; echo "    got      $got"; return 1; }
    }
    if [ ! -f "$jar" ]; then
        echo "fetching tla2tools.jar $version ..."
        curl -fsSL -o "$jar.part" \
            "https://github.com/tlaplus/tlaplus/releases/download/$version/tla2tools.jar"
        # Verify BEFORE the file is usable: never leave an unverified jar
        # where the next run would find it and skip the check.
        verify "$jar.part" || { rm -f "$jar.part"; exit 1; }
        mv "$jar.part" "$jar"
    fi
    verify "$jar" || exit 1
    # name, config, module, hold|fail, expected-error regex (fail only), [TLC flags]
    #
    # A non-zero exit is NOT enough to call a counterexample: TLC also exits
    # non-zero when a spec or config fails to parse (151 on a semantic error).
    # Treating that as "fails as expected" is how an expected-fail config rots
    # silently while `just tla` stays green — so the failure must match the
    # specific violation it is there to demonstrate.
    check() {
        local out
        if out=$(java -cp "$jar" tlc2.TLC ${6:-} -config "$2" "$3" 2>&1); then
            if [ "$4" = "hold" ]; then
                echo "  ok       $1"
            else
                echo "  MISSING  $1 — expected a counterexample, TLC found none"; return 1
            fi
        elif [ "$4" = "fail" ]; then
            if grep -qE "$5" <<<"$out"; then
                echo "  ok       $1 (counterexample found, as expected)"
            else
                echo "  WRONG    $1 — TLC failed, but not with /$5/:"
                echo "$out" | grep -E "^Error" | head -5
                return 1
            fi
        else
            echo "  BROKEN   $1"; echo "$out" | tail -40; return 1
        fi
    }
    check "lock order: shipped protocol"      LockOrder.cfg           LockOrder.tla hold ""
    check "lock order: rejected AB/BA sketch" LockOrderHistorical.cfg LockOrder.tla fail "Error: Deadlock reached"
    check "lock order: all five per-instance rows"  LockOrderAllRows.cfg           LockOrder.tla hold ""
    check "lock order: five rows, wrong order"      LockOrderAllRowsHistorical.cfg LockOrder.tla fail "Error: Deadlock reached"
    # -deadlock on the lease configs: a closed item — completed, cancelled by
    # the process, or failed with its instance frozen for a repair the model
    # does not include — is a legitimate terminal state once the clock runs
    # out, the same kind as a torn-down scope, not a livelock. Deadlock
    # freedom is a property under test only for LockOrder.
    check "lease: safety"                     Lease.cfg               Lease.tla     hold "" -deadlock
    check "lease: double belief is reachable" Lease_DoubleBelief.cfg  Lease.tla     fail "Invariant DoubleBeliefIsReachable is violated" -deadlock
    check "lease: release without its owner check" Lease_UncheckedRelease.cfg Lease.tla fail "Action property LiveLeaseEndsOnlyByItsHolderOrTheProcess is violated" -deadlock
    check "lease: release without its lease epoch"  Lease_EpochlessRelease.cfg Lease.tla fail "Action property ReleaseFreesOnlyTheLeaseItNamed is violated" -deadlock
    # A cancelled item (interrupting boundary, terminate, teardown) is the
    # second terminal state the lease configs reach; same -deadlock reason.
    check "lease: completing a cancelled item"      Lease_CancelIgnoresGuard.cfg Lease.tla fail "Action property NoCompletionAfterCancel is violated" -deadlock
    # -deadlock: a terminal state is legitimate here (everything torn down,
    # nothing armed). Deadlock freedom is a property under test only for
    # LockOrder, where the flag is deliberately absent.
    check "timer claim vs scope teardown"     TimerTeardown.cfg       TimerTeardown.tla hold "" -deadlock
    check "teardown leaving a timer behind"   TimerTeardown_Buggy.cfg TimerTeardown.tla fail "Invariant NeverFiredADanglingTimer is violated" -deadlock
    # The same module over subscription rows: correlate claims a boundary
    # subscription the way try_fire claims a timer (unlocked pick, instance
    # row, re-check of the ROW), and teardown must withdraw both kinds of arm
    # with the token. Nothing in the module is timer-specific; this run is
    # what lets the README say so about subscriptions.
    check "correlate vs scope teardown"       SubscriptionTeardown.cfg TimerTeardown.tla hold "" -deadlock
    # -deadlock: the two exits (host completed, boundary taken) are the
    # model's legitimate terminal states.
    check "boundary exit: complete vs correlate"    BoundaryExit.cfg               BoundaryExit.tla hold "" -deadlock
    check "boundary exit: no re-check under the lock" BoundaryExit_NoRecheck.cfg   BoundaryExit.tla fail "Invariant ExactlyOneExit is violated" -deadlock
    check "boundary exit: completion keeps the arm"   BoundaryExit_NoWithdraw.cfg  BoundaryExit.tla fail "Invariant ArmDiesWithTheWait is violated" -deadlock
    check "boundary exit: re-check of any row"        BoundaryExit_AnyRowRecheck.cfg BoundaryExit.tla fail "Invariant LateCallsAreTyped is violated" -deadlock
    check "retention: floor and the archive gap" Retention.cfg              Retention.tla hold "" -deadlock
    check "retention: floor from the plan"       Retention_FloorFromPlan.cfg Retention.tla fail "Invariant FloorIsSomethingDeleted is violated" -deadlock
    check "retention: no DUE re-check"           Retention_NoRecheck.cfg     Retention.tla fail "Invariant OnlyDueRecordsDeleted is violated" -deadlock

# Differential the FEEL subset against dsntk (the DMN-TCK-verified reference):
# every condition we accept must evaluate identically there. Outside the
# workspace and not part of `test` — dsntk pulls ~170 crates and a C library.
feel-parity:
    cd feel-parity && cargo test

# ---------------------------------------------------------------------- DMN
#
# The dsntk route to DMN and full FEEL (docs/dmn.md). dsntk cannot reach
# wasm32 as published: `dsntk-feel-number` binds Intel's decimal C library and
# `dsntk-feel-evaluator` carries an unconditional `reqwest` for FEEL's
# external-Java bridge. The first is replaced by `crates/rbpmn-feel-number`
# and substituted through `[patch.crates-io]`; the second is refused outright.

# Gate 0a: our decimal against the C library it replaces, plus upstream's own
# 1166-line test corpus vendored as the acceptance suite.
#
# The vendored corpus runs under `cargo test` (`rbpmn-feel-number` is a default
# member). The *differential* is outside the workspace and cannot be reached
# from it, for the same reason as `feel-parity`: this is the only place the C
# library is allowed to exist. That half is what this recipe is for, and it
# stays owed by any change to the number crate.
#
# The run is green with divergences, not despite them — three classes are
# named and counted (docs/dmn.md, "Measured deviations"), and anything outside
# them fails. The transcendental tolerance applies at the `exp`/`ln`/`sqrt`/
# `pow` call sites only; exact arithmetic must match digit for digit.
number-parity: dsntk-rev
    cd feel-number-parity && cargo test -- --nocapture

# Every place the dsntk fork's revision is written down must name the same one.
#
# There are four, and they are not near each other: the crate that ships it,
# the differential that verifies its decimal, the wasm gate, and the TCK gate.
# A differential run against a rev nobody ships is green and meaningless — the
# exact failure `number-parity` exists to prevent, one level up. So this is a
# dependency of the gates rather than a separate chore.
dsntk-rev:
    #!/usr/bin/env bash
    set -euo pipefail
    revs=$(grep -ho 'tpjg/dsntk[^"]*", rev = "[0-9a-f]*"' \
             crates/rbpmn-dmn/Cargo.toml feel-number-parity/Cargo.toml dmn-wasm-probe/Cargo.toml \
           | grep -o 'rev = "[0-9a-f]*"' | sort -u)
    tck=$(grep -o '^DSNTK_FORK_REV=[0-9a-f]*' dmn-tck/run.sh | cut -d= -f2)
    n=$(printf '%s\n' "$revs" | wc -l | tr -d ' ')
    if [ "$n" != "1" ]; then
      echo "dsntk fork revs disagree across manifests:"; printf '%s\n' "$revs"; exit 1
    fi
    manifest_rev=$(printf '%s' "$revs" | grep -o '[0-9a-f]\{7,\}')
    if [ "$tck" != "$manifest_rev" ]; then
      echo "dmn-tck/run.sh pins $tck but the manifests pin $manifest_rev"; exit 1
    fi
    echo "dsntk fork pinned at $manifest_rev everywhere"

# Gate 0b: the DMN stack parsing, compiling and *evaluating* inside a real
# WebAssembly VM, built through wasm-pack exactly as `rbpmn-wasm` is. Needs
# node + wasm-pack and the wasm32-unknown-unknown target.
#
# Throwaway scaffolding, superseded by crates/rbpmn-dmn at P1 — it exists so
# the gate's verdict is reproducible rather than remembered.
dmn-wasm-probe:
    rustup target add wasm32-unknown-unknown
    cd dmn-wasm-probe && wasm-pack build --target nodejs --out-dir pkg --no-typescript && node run.mjs

# The DMN crate's own tests: the fixture corpus, the value bridge's hostile
# inputs, and the outcome pinning.
#
# `cargo test` runs these too, now that `rbpmn-dmn` is a default member
# (docs/dmn.md, D9). Kept as a recipe because it is the fast loop while
# working on the crate, not because it is the only way to reach it.
dmn-test:
    cargo test -p rbpmn-dmn

# Gate 0c: the DMN TCK, run against dsntk twice — as published, and with our
# decimal substituted — comparing the two verdicts case by case. The totals
# are the weak half; the result files are compared byte for byte, because two
# runs can agree on the count and disagree on which cases failed.
#
# Nothing is vendored: the corpus is separately OMG-licensed, and the dsntk
# source and runner are pinned fetches with the crate tarball's checksum
# verified before extraction (the `just tla` discipline). Needs git, curl and
# a few minutes; `--patched-corpus` additionally applies dsntk's own
# opinionated TCK patches, which is the corpus their published 3374/3391 was
# measured against.
dmn-tck *ARGS:
    ./dmn-tck/run.sh {{ARGS}}

# Bake DI into any fixture that lacks a BPMNDiagram section (idempotent).
fixtures-di:
    cd playground && npm install && node scripts/add-di.mjs

# Browser end-to-end tests with automatic screenshots (e2e/screenshots/,
# gitignored): every fixture rendered, plus the full inspection stack when a
# local Postgres is reachable. Needs python3 + playwright
# (pip install playwright && playwright install chromium).
e2e:
    cd playground && npm install
    python3 e2e/run.py

# ---------------------------------------------------------------- benchmarks
#
# A separate track from the correctness tests (benchmarks/README.md). Nothing
# here runs in `cargo test` — `rbpmn-bench` is a workspace member but not a
# default one — and nothing here gates CI on an absolute number. The single
# exception is `bench-micro`, which compares the pure-core suite against a
# baseline recorded on *this* machine.
#
# Needs the local Postgres, the same one `just serve` and the integration
# tests use. No Docker: `bench-compose` is the opt-in variant for a pinned,
# explicitly tuned server.

# Provision the benchmark database (idempotent). Separate from rbpmn_dev on
# purpose: a benchmark leaves hundreds of thousands of rows behind, and it
# rewrites per-table autovacuum settings.

# Create the rbpmn_bench database if it does not exist (idempotent).
bench-db:
    #!/usr/bin/env bash
    set -euo pipefail
    psql -h localhost postgres -tc "select 1 from pg_database where datname = 'rbpmn_bench'" | grep -q 1 \
        || psql -h localhost postgres -c "create database rbpmn_bench"

# The full lifecycle suite (or one scenario: `just bench linear-5-service`).
# Writes benchmarks/results/<scenario>-<date>-<host-id>.json, one per
# scenario, each carrying the git sha, the seed, every Postgres setting, the
# hardware and the scenario's own statement of what it does not measure.
#
# Release mode is not optional: a debug-build benchmark measures the debug
# build, and someone will quote the number anyway.

# The lifecycle suite, or one scenario: `just bench linear-5-service`.
bench SCENARIO='': bench-db
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{SCENARIO}}" ]; then
        cargo run --release -p rbpmn-bench -- run --all
    else
        cargo run --release -p rbpmn-bench -- run "{{SCENARIO}}"
    fi

# Latency under a fixed arrival rate rather than drain-the-backlog
# throughput. Records whether the arrival tap fell behind — an open loop that
# quietly slowed down would be reporting a rate it never ran at.

# The same scenarios at a fixed arrival rate: latency under load.
bench-steady SCENARIO='': bench-db
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{SCENARIO}}" ]; then
        cargo run --release -p rbpmn-bench -- run --all --mode steady
    else
        cargo run --release -p rbpmn-bench -- run "{{SCENARIO}}" --mode steady
    fi

# The pure-core criterion suite (no database, no IO, no clock) plus the
# regression gate against this machine's recorded baseline. Fast, and the
# one benchmark that may fail a build — see benchmarks/src/gate.rs for the
# fences on that. A machine with no baseline yet reports and passes.

# Pure-core micro-benchmarks + the regression gate (fast, no database).
bench-micro:
    cargo bench -p rbpmn-bench --bench core_constructs
    cargo run --release -p rbpmn-bench -- gate --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion"

# Re-record this machine's micro baseline, into the gitignored
# benchmarks/.baselines/. Never committed — a baseline describes one machine,
# and the gate folds that machine's noise into its threshold. Explicit and
# manual: a baseline that re-recorded itself would ratchet regressions in one
# accepted percent at a time.

# Re-record this machine's (gitignored, local) micro baseline.
bench-baseline:
    cargo bench -p rbpmn-bench --bench core_constructs
    cargo run --release -p rbpmn-bench -- record-baseline --criterion-dir "${CARGO_TARGET_DIR:-target}/criterion"

# The persisted half of the pattern micro-benchmarks: per-construct cost
# including the rows it writes. Reported, never gated.

# Per-construct cost including its row writes. Reported, never gated.
bench-micro-persisted: bench-db
    cargo run --release -p rbpmn-bench -- micro-persisted

# Render this machine's results into a markdown comparison table, grouped by
# host — two machines' numbers are not comparable and a table that put them
# adjacent would imply they were. results/ is gitignored, so this renders what
# you have measured locally; there is no baseline set in the repository.

# Render benchmarks/results/*.json into a markdown table, grouped by host.
bench-report:
    cargo run --release -p rbpmn-bench -- report

# Lint and compile every benchmark model against its manifest. No database,
# no Docker: the fast check that a benchmark model is still a model this
# engine would deploy.

# Lint and compile every benchmark model against its manifest (no database).
bench-check:
    cargo run -p rbpmn-bench -- check

# The same suite against the pinned, explicitly tuned Postgres in
# benchmarks/compose.yml instead of the machine's own server. Optional, and
# the only recipe here that needs Docker. `down -v` at the end: the volume is
# anonymous, so every compose run starts from an empty database — which is
# the point of using it.

# The suite against the pinned Postgres in compose.yml. Optional; needs Docker.
bench-compose SCENARIO='':
    #!/usr/bin/env bash
    set -euo pipefail
    cd benchmarks
    trap 'docker compose -f compose.yml down -v' EXIT
    docker compose -f compose.yml up -d --wait
    export RBPMN_BENCH_DATABASE_URL="postgres://rbpmn:rbpmn@localhost:55432/rbpmn_bench"
    cd ..
    if [ -z "{{SCENARIO}}" ]; then
        cargo run --release -p rbpmn-bench -- run --all --provisioned-by compose
    else
        cargo run --release -p rbpmn-bench -- run "{{SCENARIO}}" --provisioned-by compose
    fi

# Population-scale measurement: park a large cohort, then probe what
# everything still costs at rest. Standing cost, not throughput — the
# question a long-running deployment actually has, and the one every other
# scenario here is blind to.
#
# Slow by nature: building a million parked instances takes on the order of
# fifteen minutes and a few GB. Pass a smaller ladder for a quick look:
#   just bench-population population-timer '--sizes 10000,100000'

# Park a large cohort and probe standing cost (slow; builds to 1M by default).
bench-population SCENARIO='population-timer' *ARGS='': bench-db
    cargo run --release -p rbpmn-bench -- population "{{SCENARIO}}" {{ARGS}}

# Remove every build artifact and every rbpmn database, for disk space or to
# force a genuinely clean build.
#
# Destructive and deliberately loud about it: it prints each thing before
# removing it. Scope is strictly `rbpmn_`-prefixed databases — including the
# `rbpmn_test_*` throwaways an integration test leaves behind when it panics
# (deliberately, for inspection) — plus this repository's own build output.
# Nothing outside those is touched.
#
# NOT removed: benchmarks/.baselines/ or benchmarks/results/. Both are
# gitignored, so both are the only copy there is. The baseline costs ten
# minutes of criterion to re-record and deleting it would silently disarm the
# micro gate; the results are measurements you cannot re-take at the commit
# they describe. Remove either by hand.
#
# After this, `just ui` is required before the next `cargo build` (the UI
# bundles are compile output — rbpmn-ui's build.rs will say so).

# Remove all build artifacts and all rbpmn_* databases (destructive).
cleanup:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "== databases =="
    dbs=$(psql -h localhost postgres -tAc "select datname from pg_database where datname like 'rbpmn\_%'" 2>/dev/null || true)
    if [ -z "$dbs" ]; then
        echo "  (none, or no local postgres)"
    else
        for db in $dbs; do
            echo "  dropping $db"
            psql -h localhost postgres -c "drop database $db (force)" >/dev/null
        done
    fi
    echo "== cargo =="
    for dir in . feel-parity feel-number-parity dmn-wasm-probe dmn-tck/stock dmn-tck/patched; do
        if [ -d "$dir/target" ]; then
            echo "  cargo clean in $dir ($(du -sh "$dir/target" 2>/dev/null | cut -f1))"
            (cd "$dir" && cargo clean)
        fi
    done
    echo "== node and generated assets =="
    for path in \
        playground/node_modules playground/dist playground/src/wasm playground/src/fixtures.generated.js \
        ui/node_modules ui/dist ui/wasm ui/src/generated \
        bpmnlint-plugin-rbpmn/node_modules bpmnlint-plugin-rbpmn/wasm \
        crates/rbpmn-ui/assets e2e/screenshots spec/states \
        dmn-wasm-probe/pkg \
        dmn-tck/tck dmn-tck/runner dmn-tck/patches dmn-tck/stock/dsntk-0.3.0 dmn-tck/patched/dsntk-0.3.0 \
        dmn-tck/dsntk-0.3.0.crate; do
        if [ -e "$path" ]; then
            echo "  removing $path ($(du -sh "$path" 2>/dev/null | cut -f1))"
            rm -rf "$path"
        fi
    done
    for jar in spec/.tla2tools-*.jar; do
        [ -e "$jar" ] && { echo "  removing $jar"; rm -f "$jar"; }
    done
    find . -name __pycache__ -type d -not -path './target/*' -exec rm -rf {} + 2>/dev/null || true
    echo
    echo "clean. Note: benchmarks/.baselines/ kept (machine-local; 10 min to re-record)."
    echo "Run 'just ui' before the next cargo build — the UI bundles are compile output."
