# Sources

Local copies of the pages that docs, history entries and next-prompts here rely on: one folder per domain,
one slug per item, a .md that starts with its URL and says when and how it was fetched, beside the record
as fetched.

## Index

| Local copy | Source | Fetched | Cited in |
|---|---|---|---|
| [github.com/actions-runner-images-issue-14748.md](github.com/actions-runner-images-issue-14748.md) (+ .json) | https://github.com/actions/runner-images/issues/14748 (GitHub's notice: `ubuntu-latest` moves to Ubuntu 26.04 from 2026-10-19, done by 2026-11-19; `ubuntu-24.04` to go back, `ubuntu-26.04` to choose it) | 2026-11-07, gh api | next-prompts/, history/ |
| [github.com/actions-runner-images-ubuntu-configure-environment.md](github.com/actions-runner-images-ubuntu-configure-environment.md) (+ .sh) | https://github.com/actions/runner-images/blob/fad015e47059f40cf0786dcb4593384f6a0720d3/images/ubuntu/scripts/build/configure-environment.sh (the Ubuntu image's sysctl settings: inotify watches and instances raised, `max_queued_events` left at 16384) | 2026-11-07, gh api | history/ |
