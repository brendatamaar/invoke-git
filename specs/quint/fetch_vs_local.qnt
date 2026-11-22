// Spec 2 of docs/spec/verification.md: background fetch vs local writes.
//
// Actors: a background fetch, the user committing and pushing, and someone
// else pushing to the remote. Values are commit numbers: a branch tip is the
// number of commits on it.
//
// Invariants:
//   * `fetchNeverMovesALocalBranch`: only the user's commits and pushes move
//     `local`; a fetch changes the remote-tracking ref alone;
//   * `forcePushLeasesOnTheReviewedValue`: every force push ran with the
//     lease set to the remote value the user had reviewed, and the remote
//     still held that value when it ran (law 10).
//
// The Rust side is task 17 (push); the harness will mirror `userCommit`,
// `fetch` and `userForcePush` by name when it lands.
module fetch_vs_local {
  // One force push, as it ran.
  type Push = { lease: int, remoteAtPush: int, reviewedAtPush: int, accepted: bool }

  var remote: int
  // The remote-tracking ref (`refs/remotes/origin/main`): the last fetched
  // value of `remote`.
  var tracking: int
  // The local branch.
  var local: int
  // The remote value the user reviewed before pushing.
  var reviewed: int
  // Commits made by the user and pushed so far: `local` must equal this.
  var madeLocally: int
  var pushes: List[Push]

  action init = all {
    remote' = 0, tracking' = 0, local' = 0, reviewed' = 0,
    madeLocally' = 0, pushes' = [],
  }

  // Someone else moves the remote.
  action remoteMoves = all {
    remote < 5,
    remote' = remote + 1,
    tracking' = tracking, local' = local, reviewed' = reviewed,
    madeLocally' = madeLocally, pushes' = pushes,
  }

  // The background fetch: it updates the tracking ref and nothing else.
  action fetch = all {
    tracking' = remote,
    remote' = remote, local' = local, reviewed' = reviewed,
    madeLocally' = madeLocally, pushes' = pushes,
  }

  action userCommit = all {
    local < 5,
    local' = local + 1,
    madeLocally' = madeLocally + 1,
    remote' = remote, tracking' = tracking, reviewed' = reviewed, pushes' = pushes,
  }

  // The review step before a push shows the tracking ref as it is now.
  action userReviews = all {
    reviewed' = tracking,
    remote' = remote, tracking' = tracking, local' = local,
    madeLocally' = madeLocally, pushes' = pushes,
  }

  // A force push always carries --force-with-lease=<branch>:<reviewed>. The
  // remote accepts it only when it still holds the reviewed value.
  action userForcePush = all {
    val accepted = remote == reviewed
    all {
      pushes' = pushes.append({ lease: reviewed, remoteAtPush: remote, reviewedAtPush: reviewed, accepted: accepted }),
      if (accepted) all { remote' = local, tracking' = local } else all { remote' = remote, tracking' = tracking },
      local' = local, reviewed' = reviewed, madeLocally' = madeLocally,
    }
  }

  action step = any { remoteMoves, fetch, userCommit, userReviews, userForcePush }

  val fetchNeverMovesALocalBranch = local == madeLocally

  // A rejected push is fine (the remote moved); a push that the remote
  // accepted must have found exactly the reviewed value there.
  val forcePushLeasesOnTheReviewedValue =
    pushes.indices().forall(i =>
      pushes[i].lease == pushes[i].reviewedAtPush and
      (pushes[i].accepted implies pushes[i].remoteAtPush == pushes[i].lease))

  val allInvariants = fetchNeverMovesALocalBranch and forcePushLeasesOnTheReviewedValue
}
