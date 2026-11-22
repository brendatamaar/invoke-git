// Spec 1 of docs/spec/verification.md: refresh vs user action.
//
// Actors: the user (stage, commit), the file watcher, an outside git process
// and the shell's write queue. The state is the disk (index, head, lock), the
// app's snapshot of it and the queued commands with the values they expect.
//
// Invariants:
//   * `writesApplyOnExpectedValues`: a write never applies when what it
//     expects differs from disk (rule 5; the shell refuses it instead);
//   * `noWriteUnderForeignLock`: no write applies while another process holds
//     index.lock (law 12);
//   * `snapshotIsDisk`: whenever no watcher event and no read is in flight,
//     the app's snapshot equals disk (law 11).
//
// The Rust harness (crates/harness) mirrors the actions by name: `userStage`
// and `userCommit` are Transition::Stage and ::Commit, `outsideCommit` is
// ::OutsideCommitAll, `outsideTakesLock` is ::WhileLocked, `watcherEvent` and
// `finishRead` are the DiskChanged event and the ReadState command.
//
// Known gap, recorded in tasks/024.DONE.md: `shellRun` checks the expected
// values and runs git as one step. The real shell checks first and spawns git
// after, so an outside write in that window is not caught by the check.
module refresh_vs_user {
  type Disk = { index: int, head: int, locked: bool }
  type Snapshot = { index: int, head: int }
  // What a write expects to find, and whether it moves the head (a commit)
  // or only the index (a stage).
  type Command = { expectIndex: int, expectHead: int, commits: bool }
  // One applied write, as it found disk.
  type Applied = {
    expectIndex: int, foundIndex: int,
    expectHead: int, foundHead: int,
    lockedByOther: bool
  }

  var disk: Disk
  var snapshot: Snapshot
  var queue: List[Command]
  // Empty, or the one write the shell is running.
  var running: List[Command]
  // Watcher events not yet turned into a read, and a read in flight.
  var events: int
  var readPending: bool
  var log: List[Applied]

  action init = all {
    disk' = { index: 0, head: 0, locked: false },
    snapshot' = { index: 0, head: 0 },
    queue' = [],
    running' = [],
    events' = 0,
    readPending' = false,
    log' = [],
  }

  // The write passes the shell's checks: lock free, index and head as expected.
  pure def passes(cmd: Command, d: Disk): bool =
    not(d.locked) and d.index == cmd.expectIndex and d.head == cmd.expectHead

  // --- the user, through the app ---------------------------------------
  action userStage = all {
    length(queue) < 3,
    queue' = queue.append({ expectIndex: snapshot.index, expectHead: snapshot.head, commits: false }),
    disk' = disk, snapshot' = snapshot, running' = running,
    events' = events, readPending' = readPending, log' = log,
  }

  action userCommit = all {
    length(queue) < 3,
    queue' = queue.append({ expectIndex: snapshot.index, expectHead: snapshot.head, commits: true }),
    disk' = disk, snapshot' = snapshot, running' = running,
    events' = events, readPending' = readPending, log' = log,
  }

  // --- the core: dispatch needs a fresh snapshot and a free lock --------
  action dispatch = all {
    length(running) == 0,
    length(queue) > 0,
    not(readPending),
    not(disk.locked),
    running' = [queue[0]],
    queue' = queue.slice(1, length(queue)),
    disk' = disk, snapshot' = snapshot,
    events' = events, readPending' = readPending, log' = log,
  }

  // --- the shell: refuses on a moved value or a held lock, else writes --
  action shellRun = all {
    length(running) == 1,
    val cmd = running[0]
    all {
      running' = [],
      if (passes(cmd, disk)) all {
        disk' = {
          index: disk.index + 1,
          head: if (cmd.commits) disk.head + 1 else disk.head,
          locked: disk.locked
        },
        log' = log.append({
          expectIndex: cmd.expectIndex, foundIndex: disk.index,
          expectHead: cmd.expectHead, foundHead: disk.head,
          lockedByOther: disk.locked
        }),
        events' = events + 1,
      } else all {
        disk' = disk, log' = log, events' = events,
      },
      // Law 13 / the core: whatever happened, the next write starts from disk.
      readPending' = true,
      snapshot' = snapshot, queue' = queue,
    }
  }

  // --- an outside git process ---------------------------------------------
  action outsideCommit = all {
    not(disk.locked),
    disk' = { index: disk.index + 1, head: disk.head + 1, locked: false },
    events' = events + 1,
    snapshot' = snapshot, queue' = queue, running' = running,
    readPending' = readPending, log' = log,
  }

  action outsideTakesLock = all {
    not(disk.locked),
    disk' = { index: disk.index, head: disk.head, locked: true },
    events' = events + 1,
    snapshot' = snapshot, queue' = queue, running' = running,
    readPending' = readPending, log' = log,
  }

  action outsideReleasesLock = all {
    disk.locked,
    disk' = { index: disk.index, head: disk.head, locked: false },
    events' = events + 1,
    snapshot' = snapshot, queue' = queue, running' = running,
    readPending' = readPending, log' = log,
  }

  // --- the watcher and the read it causes -----------------------------------
  action watcherEvent = all {
    events > 0,
    events' = events - 1,
    readPending' = true,
    disk' = disk, snapshot' = snapshot, queue' = queue, running' = running, log' = log,
  }

  action finishRead = all {
    readPending,
    snapshot' = { index: disk.index, head: disk.head },
    readPending' = false,
    disk' = disk, queue' = queue, running' = running, events' = events, log' = log,
  }

  action step = any {
    userStage, userCommit, dispatch, shellRun,
    outsideCommit, outsideTakesLock, outsideReleasesLock,
    watcherEvent, finishRead,
  }

  // --- invariants -------------------------------------------------------------
  val writesApplyOnExpectedValues =
    log.indices().forall(i =>
      log[i].expectIndex == log[i].foundIndex and log[i].expectHead == log[i].foundHead)

  val noWriteUnderForeignLock = log.indices().forall(i => not(log[i].lockedByOther))

  val snapshotIsDisk =
    (events == 0 and not(readPending)) implies
      (snapshot.index == disk.index and snapshot.head == disk.head)

  val allInvariants = writesApplyOnExpectedValues and noWriteUnderForeignLock and snapshotIsDisk
}
