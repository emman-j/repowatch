# RepoWatch

Portable, read-only dashboard for all your git repositories. No accounts, no network.

## Run
- Needs `git` on PATH.
- Build: `cargo build --release` -> `target/release/repowatch` (`.exe` on Windows).
- Copy the exe anywhere (USB stick, Dropbox...). Its data lives in `.repowatch/config.json` next to it.

## Use
- **Add repo**: paste a repo folder path. **Scan folder**: paste a parent folder (e.g. `D:\projects`) and it finds repos up to 5 levels deep.
- Table shows branch, ahead/behind, staged/modified/untracked counts, stashes, last commit.
- Click a repo for changed files and recent commits. Dot colour: green clean, yellow local changes, blue unpushed/behind, red error/conflict.
- "Remove from list" only forgets the repo inside RepoWatch.

## Safety
- Every git call goes through one allowlist (`src/git.rs`): `status`, `log`, `rev-parse`, `stash list`. Nothing else can run: no fetch, pull, push, merge, reset, checkout, clean, commit...
- Runs with `GIT_OPTIONAL_LOCKS=0` so even `status` doesn't touch the index; `core.fsmonitor` is disabled.
- Never deletes or writes anything except its own `.repowatch/config.json`.
- "Behind" counts reflect your last fetch (RepoWatch never fetches).
