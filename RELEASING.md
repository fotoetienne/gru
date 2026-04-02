# Release Process

## Version Policy

Gru uses [Semantic Versioning](https://semver.org/) with a `0.x.y` scheme until API stabilization:

- **Minor** (`0.2.0`, `0.3.0`) — milestone batches, new features, breaking changes
- **Patch** (`0.1.1`, `0.1.2`) — bugfixes between milestones

## Prerequisites

Install git-cliff for changelog generation:

```bash
cargo install git-cliff
```

## Quick Release

Bump the version in `Cargo.toml`, then:

```bash
just release 0.2.0
```

This runs `cargo check`, generates the changelog, commits, tags, and pushes. The CI workflow builds binaries and creates the GitHub Release automatically.

## Manual Steps (if you prefer)

1. **Bump version** in `Cargo.toml`:
   ```toml
   version = "0.2.0"
   ```

2. **Run a quick check**:
   ```bash
   cargo check
   ```

3. **Generate the changelog** (use `--tag` so git-cliff knows the version before tagging):
   ```bash
   GITHUB_TOKEN=$(gh auth token) git-cliff --github-repo fotoetienne/gru --tag v0.2.0 -o CHANGELOG.md
   ```
   Review the output in `CHANGELOG.md` and edit if needed.

4. **Commit the release**:
   ```bash
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "Release v0.2.0"
   ```

5. **Tag the release**:
   ```bash
   git tag v0.2.0
   ```

6. **Push**:
   ```bash
   git push origin main --tags
   ```

## Verifying

After release, confirm:

```bash
cargo install --path .
gru --version
# => gru 0.2.0 (abc1234)
```

## What Happens on Push

The `release.yml` workflow automatically builds binaries for macOS (ARM, x86) and Linux (x86), generates checksums, and creates a GitHub Release with all artifacts attached.
