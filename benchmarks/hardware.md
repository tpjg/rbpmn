# Hardware spec

A benchmark number belongs to a machine. This file is the machine — a
**filled-in template**, not prose, so that the harness can read it and copy
it into every result file.

The harness detects what it can (CPU model, physical cores, RAM, OS, arch,
Postgres version and every setting) and records that itself. What it cannot
detect is the block below, and the field that matters most is `disk`: NVMe,
SATA SSD and network-attached storage differ by more than any tuning in
`compose.yml` does, and a process engine is a transactional workload that
feels all of it.

**Fill this in before publishing numbers.** A run against the shipped
template still works — nothing here blocks a benchmark — but it records a
warning in the result file and prints it, because "we do not know what disk
this ran on" has to travel with the number rather than be quietly dropped.

```toml
# These three are detected anyway; declare them only if you want the
# cross-check, and a mismatch is recorded as a warning. Left commented out
# they cost nothing — the machine's own answer is what the result file
# carries either way.
# cpu_model = "Apple M3 Max"
# physical_cores = 14
# ram_gb = 36

# Not detectable, and the one that matters. nvme | ssd | network | unknown
disk = "ssd"

# local  — harness and Postgres on one machine. This is the default and the
#          shape the engine is actually deployed in: it is a library inside
#          the application, and the application's database is its database.
# remote — a separate database host. Valid, and recorded, but never mix the
#          two in one comparison: the network round trip per transaction is
#          the dominant term.
postgres_location = "local"

# Where that Postgres came from, for the reader's benefit — the harness
# records its own answer in `postgres.provisioned_by` regardless.
#   local   — the machine's own server (the default; no Docker involved)
#   compose — benchmarks/compose.yml, pinned by digest and explicitly tuned
postgres_provisioning = "local"

notes = """
Anything a reader needs in order to interpret the numbers: virtualization,
a shared host, thermal limits, a laptop on battery, an unusual filesystem,
a Docker storage driver that is not overlay2.
"""
```

## What the harness records on top of this

Every result file carries:

- `hardware.detected` — CPU model, physical and logical cores, RAM, OS, arch
- `hardware.host_id` — a stable hash of hostname + CPU + arch + cores.
  Results and micro baselines are keyed by it, and the report renderer
  groups by it, because two rows from different machines are not comparable
  and printing them adjacent implies they are.
- `hardware.declaration_mismatches` — where this file and the machine
  disagree
- `postgres.settings`, `postgres.non_default_settings`, `postgres.table_options`
- `postgres.local`, `postgres.connection_host`, `postgres.provisioned_by`

## Same-host default

`just bench` runs the harness and Postgres on one machine. That is not a
simplification — it is how this engine is deployed. It is a Rust library
inside an application, stepping tokens inside the application's own
transactions, against the application's own database. A benchmark that put
the database on another host would be measuring a topology this design
deliberately does not have.

To measure a remote database anyway, point the harness at it:

```
RBPMN_BENCH_DATABASE_URL=postgres://user:pass@db.internal:5432/rbpmn_bench \
  cargo run --release -p rbpmn-bench -- run --all
```

The result records `postgres.local = false` and the host it connected to.
Compare remote results only with other remote results.
