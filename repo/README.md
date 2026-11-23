# Takt repository for the Homebrew Channel

The Homebrew Channel reads a repository as a base URL plus `apps.json`. This directory is that base:
add

```
https://raw.githubusercontent.com/acuntex/takt/main/repo/apps.json
```

under **Settings → Repositories → Add repository** in the Channel.

`apps.json` and `dist/<id>.manifest.json` are generated from the built IPK and
`packaging/appinfo.json` (`packaging/gen-repo.sh <tag>`), so the hash and URLs always match the
published release. The manifest is an output and a release asset, never an input. The repository
must be public and the release assets must exist; a private repository answers 404 to the Channel.
