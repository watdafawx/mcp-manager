# Releasing MCP Manager

A release is a git tag. Pushing `v<version>` runs `.github/workflows/release.yml`, which builds
`mcpm.exe` on `windows-latest` and publishes the GitHub release. Installed copies find it
through the "latest release" API and update themselves (SPEC.md, "Self-update").

## Steps

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`. Every crate inherits it.
   Run a build so `Cargo.lock` follows.
2. Run the checks: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`.
3. Commit with the subject `MCP Manager x.y.z`. The commit body becomes the top of the release
   notes, so put what changed for users there. Subjects of the commits since the last tag are
   listed below it.
4. Tag and push:

   ```
   git tag vx.y.z
   git push origin main vx.y.z
   ```

The workflow refuses a tag that differs from the Cargo.toml version.

## What the workflow publishes

| Asset | Used for |
|---|---|
| `mcpm.exe` | the app; what installed copies download |
| `mcpm-<version>-win64.zip` | the same exe zipped, for people downloading by hand |
| `mcpm.exe.sha256` | `<sha256>  mcpm.exe`; installed copies verify their download against it |

An update is only offered when `mcpm.exe` is attached to the latest release and the tag is a
higher version than the running one. The download is checked against `mcpm.exe.sha256` (or the
`sha256:` digest GitHub shows for the asset when that file is missing); with neither, the
update is refused. Draft and pre-release entries are ignored. Re-running the workflow for a tag
re-uploads the assets (`--clobber`).

## Release notes

`scripts/release-notes.ps1 <tag>` builds them: the release commit's body, the commit subjects
since the previous `v*` tag, a compare link. To refill an old release:

```
pwsh scripts/release-notes.ps1 v0.2.0 | gh release edit v0.2.0 --notes-file -
```

## Testing an update without a release

The daemon reads `MCPM_UPDATE_URL`, a base replacing `https://api.github.com/repos/<owner>/<repo>`,
and asks `<base>/releases/latest`. Answer with the same JSON GitHub sends (`tag_name`, `assets`
with `name`, `size` and a `browser_download_url` the daemon can reach). Serve that file and a
higher-versioned `mcpm.exe` (plus its `mcpm.exe.sha256`) from any static server, copy the test
exe somewhere outside a cargo `target` folder, start its daemon with the variable set, and use
`mcpm update` or the tray's "Check for updates".

Builds run from a cargo `target\debug` or `target\release` folder never replace themselves,
and `MCPM_NO_UPDATE=1` switches updating off for any copy.
