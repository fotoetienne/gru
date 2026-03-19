# Open Source Readiness Plan

**Goal:** Get Gru ready for internal Netflix sharing (phase 1) and public open source (phase 2).

**Status:** Gru is ~85-90% ready. Code is clean, no internal dependencies, strong docs. Gaps are release infrastructure.

## Phase 1: Internal Netflix Sharing

### P0 — Blockers (must ship)

- [ ] #518 — **Add LICENSE file** (Apache-2.0)
- [ ] #519 — **Cargo.toml metadata** (`license`, `authors`, `repository`, `description`) — blocked by #518
- [ ] #520 — **Example config file** (`docs/config.example.toml` with GHES setup)
- [ ] #521 — **CONTRIBUTING.md** (dev setup, PR workflow, code style)
- [ ] #522 — **SECURITY.md** (threat model for `--dangerously-skip-permissions`)
- [ ] #530 — **README overhaul** (user-focused, move dev stuff to CONTRIBUTING/CLAUDE.md, multi-agent emphasis) — blocked by #518, #521

### P1 — Should-haves (high value for adoption)

- [ ] #523 — **GHES setup guide** — blocked by #520
- [ ] #524 — **Prebuilt binary releases** (GitHub Releases, macOS arm64 + x86_64 + Linux) — blocked by #519
- [ ] #525 — **CHANGELOG.md** (v0.1.0 summary)
- [ ] #526 — **Fix test flake** (`test_reap_children_removes_exited_process`)
- [ ] #527 — **`gru init` polish** (check deps, guide config, verify auth) — blocked by #520

### P2 — Nice-to-haves (accelerates adoption)

- [ ] #528 — **cargo-audit in CI**
- [ ] #529 — **Demo GIF in README**

## Phase 2: Public Open Source

- [ ] #531 — **Project website** (GitHub Pages, mdBook or static) — blocked by #530, #529
- [ ] Release automation (cargo-dist, Homebrew formula, crates.io)
- [ ] Issue templates (bug report, feature request)
- [ ] Code of conduct
- [ ] Competitive positioning in README

## Dependency Graph

```
#518 LICENSE
 └─► #519 Cargo.toml metadata
      └─► #524 Binary releases
 └─► #530 README overhaul ◄── #521 CONTRIBUTING.md

#520 Example config
 └─► #523 GHES guide
 └─► #527 gru init polish

#529 Demo GIF ──► #531 Website ◄── #530 README overhaul

Independent: #522 SECURITY.md, #525 CHANGELOG, #526 Test flake, #528 cargo-audit
```

## Critical Path

**Fastest path to "shareable internally":**
1. #518 LICENSE (10 min)
2. #519 Cargo.toml metadata (10 min) — needs #518
3. #520 Example config (30 min) — parallel with #518/#519
4. #521 CONTRIBUTING.md (1 hr) — parallel
5. #522 SECURITY.md (2 hr) — parallel
6. #530 README overhaul (2-3 hr) — needs #518, #521
7. Tag v0.1.0

**Total estimated effort:** ~8-10 hours across P0 items.

## Strategy

**Recommendation: Internal first, then open source.**

1. Ship Phase 1 internally, get 2-3 Netflix teams using it
2. Collect feedback on setup pain, missing docs, sharp edges
3. Fix what surfaces from real adoption
4. Open source with confidence (Phase 2)

## Open Questions

1. **License:** Apache-2.0 (Netflix standard) vs. dual Apache-2.0/MIT (Rust ecosystem norm)?
2. **Repo location:** Stay on personal GitHub? Move to a Netflix org? New org?
3. **Naming:** Is "Gru" final? Any trademark concerns?
4. **Supervised mode:** Should Gru support a mode without `--dangerously-skip-permissions` for cautious adopters?
5. **CLAUDE.md naming:** Symlink AGENTS.md → CLAUDE.md? Rename? Keep both?
