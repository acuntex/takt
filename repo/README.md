# Takt repository for the Homebrew Channel

The Homebrew Channel reads a repository as a base URL plus `apps.json`. This directory is that base:
add

```
https://raw.githubusercontent.com/acuntex/takt/main/repo/apps.json
```

under **Settings → Repositories → Add repository** in the Channel.

`apps.json` is generated at release time from the built IPK and its manifest
(`packaging/gen-repo.sh`), so the hash and URLs always match the published release. The repository
must be public and the release assets must exist; a private repository answers 404 to the Channel.
