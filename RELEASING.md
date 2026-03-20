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

Set a GitHub token for PR metadata lookups (avoids rate limiting):

```bash
export GITHUB_TOKEN=$(gh auth token)
```

## Release Steps

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
   git-cliff --github-repo fotoetienne/gru --tag v0.2.0 -o CHANGELOG.md
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

## Future Automation

GitHub Actions will automate binary builds on tag push (see #524).
