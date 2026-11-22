// Spec 3 of docs/spec/verification.md: cancel.
//
// Actors: a running write and a cancel that can arrive at any step.
//
// A write is a few steps of git's own: objects are written (nothing is
// referenced yet), the ref is locked (`<ref>.lock`), and the lock is renamed
// over the ref (one atomic step that both publishes the new value and
// releases the lock). A cancel kills the process wherever it is.
//
// Invariants (law 13):
//   * `neverHalfWritten`: once nothing is running, the ref holds either its
//     old value or the new one, never anything else;
//   * `leftoverLockIsOurs`: a lock that is left behind is the one our own
//     killed process held, so the app can clear it (it is "HeldByUs", not a
//     foreign lock) and the repo then is exactly the unchanged repo.
//
// The Rust side is crates/shell/tests/runner.rs (cancel kills the hook and
// leaves HEAD alone); `cancel` here is Event::OpCancelRequested.
module cancel {
  // Where the write is. `Idle` is before it starts and after it ends.
  type Stage = Idle | ObjectsWritten | RefLocked | Published
  type Ended = NotEnded | Finished | Cancelled

  var stage: Stage
  var ended: Ended
  // The ref's value: 0 is the old one, 1 the new one.
  var ref: int
  var lockHeldByUs: bool
  var cleaned: bool

  action init = all {
    stage' = Idle, ended' = NotEnded, ref' = 0, lockHeldByUs' = false, cleaned' = false,
  }

  action writeObjects = all {
    stage == Idle, ended == NotEnded,
    stage' = ObjectsWritten,
    ended' = ended, ref' = ref, lockHeldByUs' = lockHeldByUs, cleaned' = cleaned,
  }

  action lockRef = all {
    stage == ObjectsWritten,
    stage' = RefLocked, lockHeldByUs' = true,
    ended' = ended, ref' = ref, cleaned' = cleaned,
  }

  // The atomic step: rename the lock over the ref.
  action publish = all {
    stage == RefLocked,
    stage' = Published, ref' = 1, lockHeldByUs' = false,
    ended' = Finished, cleaned' = cleaned,
  }

  // The user cancels: the process is killed wherever it is. Nothing else
  // runs afterwards, so the stage stays where it was.
  action cancel = all {
    ended == NotEnded,
    stage != Idle,
    ended' = Cancelled,
    stage' = stage, ref' = ref, lockHeldByUs' = lockHeldByUs, cleaned' = cleaned,
  }

  // After a cancel the app removes the lock its own process left behind.
  action cleanUp = all {
    ended == Cancelled, lockHeldByUs,
    lockHeldByUs' = false, cleaned' = true,
    stage' = stage, ended' = ended, ref' = ref,
  }

  action step = any { writeObjects, lockRef, publish, cancel, cleanUp }

  val neverHalfWritten = ended != NotEnded implies (ref == 0 or ref == 1)

  // A cancelled write that did not publish left the old value.
  val cancelledMeansUnchangedOrPublished =
    (ended == Cancelled) implies ((stage == Published and ref == 1) or ref == 0)

  val leftoverLockIsOurs = (ended != NotEnded and lockHeldByUs) implies (ended == Cancelled and stage == RefLocked)

  val allInvariants = neverHalfWritten and cancelledMeansUnchangedOrPublished and leftoverLockIsOurs
}
