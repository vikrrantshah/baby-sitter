# baby-sitter

`baby-sitter` drives one GitHub pull request to merged. The binary does all routine work and every git and GitHub write. It calls Codex only to edit code and draft replies to review comments.

Visual version of the flow: [`design.html`](design.html).

## Requirements

- `gh`, authenticated as the account that replies and merges.
- `git`, with the PR's head branch checked out and tracking its remote branch.
- `codex`, logged in.

## Build

```sh
cargo install --path .
```

## Usage

Run the binary from the PR's checkout. Without an argument, it uses the PR for the current branch.

```sh
gh pr checkout 123
baby-sitter --dry-run    # print every rule's verdict, change nothing
baby-sitter              # drive the PR to merged
baby-sitter 123 --merge-method rebase --timeout 4h --interval 30s
```

| Flag | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Print the verdict of every rule for the current snapshot, then exit. |
| `--merge-method` | `squash` | `squash`, `merge`, or `rebase` for direct merges. Merge queues use their configured method. |
| `--timeout` | `2h` | Deadline for the whole run, including every `gh`, `git`, and `codex` call. |
| `--interval` | `10s` | Time between snapshots while the run waits. |

| Exit code | Meaning |
|---|---|
| 0 | The PR is merged. A dry run exits 0 unless its first verdict is a stop. |
| 1 | The run stopped. The last line gives the reason. |
| 2 | The deadline passed. |

## Flow

Each pass takes a fresh snapshot of the PR. For an open PR, it also observes the local checkout. The first rule that matches decides the pass. After any action, the next pass takes a new snapshot, so no step acts on stale data.

```mermaid
flowchart TD
  START([Start]) --> SNAP["1 · Take a snapshot<br/>PR and open PR checkout"]
  SNAP --> T{{"2 · Merged, closed, draft,<br/>conflicting, or CI failed?"}}
  T -- merged --> DONE([Exit 0])
  T -- other --> STOP([Stop, exit 1])
  T -- no --> Q{{"3 · Queue status?"}}
  Q -- queued --> WAIT["Wait --interval"] --> NEXT
  Q -- "removed at current head" --> STOP
  Q -- otherwise --> L{{"4 · Local checkout in sync?"}}
  L -- behind --> FF["Fast-forward"] --> NEXT
  L -- "dirty, ahead, or diverged" --> STOP
  L -- yes --> RF{{"5 · Fixed threads still open?"}}
  RF -- yes --> RES["Resolve them"] --> NEXT
  RF -- no --> UH{{"6 · Unanswered comments?"}}
  UH -- yes --> HC
  subgraph CODEX ["Codex"]
    HC["Edit code, draft replies<br/>fixed, declined, or needs_human"]
  end
  HC --> PUSH["Commit and push"] --> REPLY["Post replies with markers"] --> RR["Request re-review"] --> NEXT
  UH -- no --> BE{{"7 · Direct merge behind base?"}}
  BE -- yes --> UP["Update branch"] --> NEXT
  BE -- no --> G{{"8 · Anything blocks the merge?"}}
  G -- yes --> WAIT
  G -- no --> MERGE["Merge or enqueue the snapshot's head SHA"] --> NEXT
  NEXT(("Next pass")) -.-> SNAP
```

The rules live in one ordered table, `RULES` in `src/decide.rs`. The table order is the precedence. A direct merge or enqueue action runs only when no rule matches.

| Step | Rules | Result when a rule matches |
|---|---|---|
| 2 | `merged`, `closed`, `draft`, `conflict`, `ci-failed` | Exit 0 for `merged`. Stop for the others. |
| 3 | `in-queue`, `dequeued` | Wait while queued. Stop if the current head was removed for a reason other than `merged`. |
| 4 | `local-upstream`, `local-dirty`, `local-diverged`, `local-behind` | Fast-forward when behind. Stop for the others. |
| 5 | `resolve-fixed` | Resolve threads whose newest marker says `fixed`. |
| 6 | `unhandled-items` | Send the items to Codex, push, reply, and request re-review. |
| 7 | `behind-base` | Run `gh pr update-branch` for direct merges. Skip this rule for merge queues. |
| 8 | `ci-pending`, `merge-state`, `unresolved-threads`, `review-requests`, `review-decision` | Wait, then take a new snapshot. |

## Comments and replies

The binary handles three kinds of items:

- Unresolved review threads from people and bots.
- Top-level PR comments from people.
- Non-empty review bodies from people.

Codex gets every unanswered item in one batch. It edits files only and returns one result for each item: `fixed`, `declined`, or `needs_human`, with reply text. If Codex fails, times out, or returns a result that does not match the batch, the run stops before it writes anything.

Every reply ends with a hidden marker:

```html
<!-- baby-sitter:v1 item=thread:PRRT_abc outcome=fixed answers=4628647074 -->
```

`answers` is the ID of the newest comment that Codex saw. A thread is unanswered when it has a comment from someone else that is newer than every comment you answered. The binary reads markers only from your own comments. A plain reply that you write by hand also answers the comments before it.

The binary resolves only `fixed` threads. Declined threads and threads that need a human stay open, and the merge waits until a person replies or resolves them.

## Restart after a crash

The binary keeps no local state file. Each pass reads progress from GitHub and from the checkout.

| The crash happened | The next run sees | The next run does |
|---|---|---|
| While Codex ran | A dirty tree, or the same unanswered items | Stops so you can inspect the tree, or calls Codex again |
| After the commit, before the push | The local branch is ahead | Stops and tells you to push or reset |
| After some replies | Markers on the answered items | Sends only the remaining items to Codex |
| Before the threads were resolved | Open threads with a `fixed` marker | Resolves them |
| Before the re-review request | Nothing | Nothing. The request is lost, and the merge still waits on the review decision. |

A lock file at `$(git rev-parse --git-dir)/baby-sitter/pr-<n>.lock` stops a second run on the same PR.

## Merge gate

The binary merges only when all of these are true:

- CI passed or the PR has no checks.
- `mergeStateStatus` is `CLEAN` or `HAS_HOOKS`, or `BEHIND` when the base uses a merge queue.
- No review threads are unresolved.
- No review requests are pending.
- `reviewDecision` is not `CHANGES_REQUESTED` or `REVIEW_REQUIRED`.

The merge passes `--match-head-commit` with the SHA from the snapshot. If someone pushes in between, GitHub rejects the merge and the run stops.

For a merge queue, the binary enqueues the snapshot's head SHA and uses the queue's configured merge method. It waits without handling new review feedback while the PR is queued because pushing would remove it from the queue. If the queue removes the current head without merging it, the run stops with the removal reason. A new head can be enqueued after the cause is fixed.

## Known limits

- The binary does not fix failing CI or merge conflicts. It stops and names the cause.
- A PR with more than 100 review threads, top-level comments, reviews, or review requests stops the run. The binary does not act on a partial view.
- A lost re-review request is not retried.
