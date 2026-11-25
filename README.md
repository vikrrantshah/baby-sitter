# baby-sitter

`baby-sitter` takes a GitHub pull request from open to merged. It answers review threads, keeps the branch up to date with its base, fixes failing CI, and then merges the PR or adds it to the merge queue. When something needs a person, it stops and says why.

Codex writes the code changes and the reply text. The binary does every git and GitHub write itself and checks Codex's edits before it commits them.

There are two ways to run it:

- **Baby Sitter**, a macOS app. Watch your repositories, press play on a pull request, and get a notification when it merges or needs you.
- **`baby-sitter`**, the command-line tool the app runs. It babysits one PR from that PR's checkout.

## Requirements

- `gh` 2.97.0 or later, signed in as the account that should reply and merge.
- `git`.
- `codex`, signed in.
- For the app: macOS, and Xcode's command line tools to build the bundle.

## Install

Install the command-line tool:

```sh
cargo install --path .
```

Build the app, then drag it to `/Applications`:

```sh
app/bundle.sh
open "target/release/Baby Sitter.app"
```

The bundle contains both binaries. For development, `cargo build --workspace && target/debug/baby-sitter-app` runs the app without a bundle.

## Babysit pull requests with the app

1. Open **Settings** with Cmd+, and select **Status**. Every required check should be green. A red check names the command that fixes it, such as `gh auth login` or `codex login`. While a required check fails, the Pull requests page shows a banner that links to **Status**. Under **Permissions**, allow notifications so you hear when a babysit merges or needs you. Baby Sitter needs no other macOS permission.
2. Select **Repositories** and turn on the repositories you want to babysit.
3. On **Pull requests**, press play on a PR. Its row shows the four phases and what the babysitter is doing now.
4. Wait for the notification. A babysit ends when the PR merges, when its timeout passes, or when it needs you, and the row then shows the reason.
5. Dismiss a finished row to delete its worktree.

The search box finds PRs by number or title. Digits, with or without a leading `#`, match PR numbers that start with them. Every search also matches titles, ignoring case. While you search, the two filter groups dim and stop applying, so a match shows whatever its babysit or state. The repository picker next to the refresh button limits the list to one repository, during a search too. Two filter groups narrow the list further. The first shows All, Watched, or Unwatched PRs, each with a count of the PRs in the chosen repository. Neither the search nor the second group changes those counts. Watched covers running babysits, PRs whose babysit ended with a merge, and every other PR in the same stack as one of those. The second shows All, Open, Drafts, or Merged PRs. Open means ready for review or queued.

The pull request list reloads every minute, so new PRs appear without pressing refresh.

Each row also has a **More actions** menu to mark a draft ready for review, convert a PR to draft, close it, or copy its link.

**Settings > App** holds the defaults for every repository. The timeout and draft timeout are both `2h`, and the interval between checks is `30s`. To override a value for one repository, use the gear next to that repository. **Auto-dismiss merged PRs** clears merged rows and their worktrees after the **Clear after** delay.

A repository's gear also sets its **Setup command**, such as `pnpm install`. Baby Sitter runs it once in each PR worktree before the babysit starts. A finished setup does not run again unless you change the command. It is the only package install, so set it for any repository whose checks need dependencies. If the command fails, the babysit stops with its error and the app removes the worktree. The next start runs the setup again.

**Settings > Prompts** lets you replace the instructions Codex gets for review threads, failing CI, and merge conflicts. Turn on **Custom review prompt**, **Custom CI fix prompt**, or **Custom conflict prompt**, edit the text, and select **Save**. Until you save, the default prompt applies. Turning a toggle off restores the default and keeps your text for later. Running babysits use the change the next time they call Codex. Your text replaces only the instructions. Baby Sitter still adds the threads, checks, or conflicted files, and its safety checks apply regardless of the text.

Quitting the app stops every babysit. The next launch restarts them with their original deadlines.

## Babysit one pull request from the command line

Run the binary inside the PR's checkout. Without a PR number, it uses the PR for the current branch.

```sh
gh pr checkout 123
baby-sitter --dry-run    # print what every rule decides, change nothing
baby-sitter              # drive the PR to merged
baby-sitter 123 --merge-method rebase --timeout 4h --interval 30s
```

| Flag | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Print every rule's verdict for the current state of the PR, then exit. |
| `--merge-method` | `squash` | `squash`, `merge`, or `rebase` for direct merges. A merge queue uses its own method. |
| `--timeout` | `2h` | Deadline for the whole run, including every `gh`, `git`, and `codex` call. |
| `--interval` | `10s` | Time between checks while the run waits. |
| `--gates` | off | Print each phase's state before every decision, for example `[gates] reviews=met checks=waiting branch=met merge=pending`. `reviews` is the Comments phase. |
| `--prompts` | none | A JSON file that replaces the instructions for some prompts, for example `{"ci": "…"}`. Keys are `review`, `ci`, and `conflicts`. A missing file means the defaults. The binary reads the file again before each Codex call. |
| `--print-default-prompts` | off | Print the default instructions as JSON, then exit. |

| Exit code | Meaning |
|---|---|
| 0 | The PR is merged. A dry run exits 0 unless its first verdict is a stop. |
| 1 | The run stopped. The last line gives the reason. |
| 2 | The deadline passed. |

## How it works

Each pass reads the PR from GitHub and works on the first of four phases that is not done:

1. **Comments.** Codex answers unresolved review threads and fixes the code they ask about. The binary replies in each thread and resolves the fixed ones.
2. **Branch.** The binary updates the branch from its base. When the two conflict, Codex resolves the conflicted files.
3. **Checks.** The binary waits for CI. It reruns cancelled runs and has Codex fix the code behind real failures.
4. **Merge.** The binary waits for approvals, then merges or enqueues the exact commit it checked.

A phase starts only when every earlier phase is done. If an earlier phase slips, for example a new review comment arrives while the PR sits in the merge queue, the run goes back to it. The binary keeps no state file, so a crashed run restarts where it left off.

[How baby-sitter works](docs/how-it-works.md) lists every rule, the reply markers, the conflict procedure, and the crash recovery table. [`design.html`](design.html) draws the flow.

## Stacked pull requests

A PR is stacked when its base branch is another PR's branch, the PR below it. A stack lands in one of two ways.

- **Bottom up**, the default. Baby Sitter never merges a stacked PR into the PR below. Its babysit handles comments, branch updates, and CI as usual, then waits with a message such as "Waiting for #12 below to merge". When the PR below merges, Baby Sitter moves the stacked PR onto that PR's base if GitHub has not already done so. It then merges the new base in, waits for CI, and merges. Each PR lands on its own, one at a time.
- **Top down.** Each PR waits with a message such as "Waiting for #12 above to merge" while a PR is stacked on it. The top PR merges into the PR below it, then that PR merges into the one below it, and so on. The bottom PR then carries the whole stack and lands once, through the default branch's usual checks and merge queue. The PRs above the bottom merge into branches, so the default branch's review and CI rules apply only to the bottom PR.

In the app, a stacked row sits under the PR below it, and a vertical line joins the play buttons of PRs in the same stack. **Babysit stack bottom up** and **Babysit stack top down** in the row's More actions menu start every open PR in the stack in that mode. Top down also starts PRs on side branches of the stack, because each PR waits for every PR stacked on it. A stack runs in one mode at a time. Play on a PR whose stack is already running top down joins it top down.

## What it never does

- Edit CI configuration, or delete, skip, or weaken tests.
- Revert or reshape what the PR sets out to do just to make CI pass.
- Force-push. Every push is a fast-forward of the PR branch.
- Post a top-level PR comment. It replies only inside review threads.
- Merge a commit it did not check. The merge names the exact head SHA, and GitHub rejects it if someone pushed in between.

## Known limits

- CI failures caused by secrets, infrastructure, or flaky tests need a person. The binary pushes any code fix it can, then stops and names the remaining cause.
- A PR with more than 100 review threads, comments in one thread, reviews, review requests, or checks stops the run. The binary does not act on a partial view.
- Baby Sitter does not resend a re-review request that a crash lost.
- Baby Sitter does not support PRs from forks.
