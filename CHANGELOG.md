# Changelog

This file starts at 0.7.0. Earlier releases are described by their tags and
commit history.

## 0.7.0

Three concurrency bugs found by reading `spec/` against the code, each fixed
with regression tests and a model-checked spec (`just tla`). Two of the fixes
change public API, which is why this is 0.7.0 and not 0.6.4.

### Upgrading — read this before updating

**HTTP workers: `POST /v1/tasks/{id}/fail` now requires `leaseNo`.**
Send the `leaseNo` the claim (`POST /v1/tasks/get`) returned, exactly as
`/v1/tasks/{id}/release` already requires. A request without it is a **422**.

Update workers *before* or *together with* the server. A worker that is not
updated cannot record a failure at all. Its item keeps its lease until the
lease expires, is claimed again and run again, and fails again with another
422. No retry is spent, no error boundary is taken and no incident is raised,
so a handler that always fails is re-run for ever, side effects included.

New response: **409 `{"outcome": "lockLost", "state": …}`** when the claim the
request names is gone. That covers two cases:

- a retry whose first copy already landed (before: `200 retrying`, spending a
  second retry);
- a failure naming a claim that was released, expired and re-claimed, or
  replaced by a newer claim of the same owner (before: it landed on that
  claim).

Treat it like the heartbeat's `lockLost`: stop working on the item. Nothing
changed.

`POST /v1/work-items/{id}/fail`, the ownerless operator path, is unchanged.

**Rust API (`rbpmn-engine`).** These are compile-time breaks:

- `Engine::fail_task(task, owner, lease, error_code, detail)` takes the claim's
  `LockedTask::lease_no` as a new third argument.
- `FailOptions` has a new public field `lease: Option<i64>`. Code that builds
  it with every field listed needs `lease: …`; code using
  `..FailOptions::default()` does not.
- `FailOutcome::Lost { state }` is new. It is a failure that **changed
  nothing**; a `_ =>` arm that treats every `Ok` as recorded is now wrong.
- `EngineError::InstanceChanged(Uuid)` is new (see `delete_instance` below).
  The HTTP server maps it to 409.
- `EngineError` and `FailOutcome` are now `#[non_exhaustive]`, so every
  `match` on them outside `rbpmn-engine` needs a wildcard arm. This is a
  one-time break that makes future variants non-breaking. Map an unknown
  `FailOutcome` to "not recorded", never to success. The HTTP server answers
  an unmapped one with a logged 500.

**If you call `fail_work_item` / `fail_work_item_in_tx` yourself with an
`owner`,** set `lease: Some(lease_no)` as well. With `lease: None` you keep
the old owner-only guard and the retry hazard that comes with it. The built-in
push worker already passes its claim's epoch.

**Archive sinks (`RetentionArchive`).** The contract is now spelled out, and a
sink has to satisfy all of it:

- the same `InstanceRecord::id` can arrive more than once, **with different
  content**, and the later delivery supersedes the earlier one;
- an archived record is **not** a promise of deletion.

A sink that upserts by id is correct. A sink with an insert-only unique key on
the id is not: it was already fragile under overlapping sweeps, and
`delete_instance` can now deliver one id twice.

### Fixed

- **`delete_instance` could delete a running instance.** `failed` is not
  terminal: a repair could thaw the instance while its record was at the
  archive sink, and the deletion then removed a live instance along with
  history the archive never saw.
  - The deletion now re-checks under the row lock that the archived record is
    still the whole record: status still deletable, and the same event count.
  - The record is read in one consistent snapshot.
  - A changed instance is archived again once and deleted. If it changed
    across both attempts, the call returns `InstanceChanged`. If it is active
    again, the call returns `InstanceStillActive`.
  - Model: `spec/DeleteInstance.tla`.
- **Two concurrent messages to a non-interrupting boundary: the second got a
  false 404.** The first delivery consumes the subscription row and opens a
  fresh one for the same key. The second delivery's row-specific re-check
  failed and answered `NoSubscription` although the new row was waiting for
  exactly that message.
  - A failed re-check now looks the key up once more under the instance lock,
    before answering 404.
  - A late message behind a host completion is still a typed 404.
  - Model: `spec/BoundaryExit.tla` (`NoFalseNotFound`).
- **`fail_task` was not safe to retry.** A failure leaves the item open, and
  the owner-only guard let a retried or stale request land again. It could
  spend a second retry, so two lost responses froze an instance on one real
  failure. It could also end the same owner's next claim while that claim's
  handler was still running. Failures are now scoped to the claim's lease
  epoch. Model: `spec/Lease.tla` (`FailSpendsOnlyTheLeaseItNamed`).
