# WO-10 — Release + deploy discipline

**Repo:** `~/code/plurx` (+ private `pjunod/ansible` at `~/code/ansible/media/`) · **Baseline:** `origin/main` @ `e8a910f` · **Priority: P0 (task 1), P1 (rest)**

## Context

The deploy tooling itself reviewed well (clean ship/ansible interface split after #46→#49, strong compose/Dockerfile with contract tests, honest clustering deferral). The structural gap is temporal: ~11 merges/day onto a fleet that does `reset --hard @{u}` + rebuild, with auto-applied forward-only migrations (v14 offline + v15 scan shipped this week), zero tags, zero backups, and no defined last-known-good. `make release-check` is red today (`CHANGELOG.md has no '## [0.2.7]' section` — reproduced) and the tag-triggered publish pipeline has never fired.

## Tasks

1. **P0 — DB backup before every deploy + a rollback runbook.**
   An older binary refuses a newer DB (`crates/plurx-core/src/store/sqlite/mod.rs:786-792`), so any revert after a migration deploy = both plurx nodes crash-looping (`restart: unless-stopped`) with no way back and nothing to restore. Zero hits for backup/rollback in OPERATIONS.md, RELEASING.md, deploy/README.md.
   Fix: (a) in the private ansible deploy play, before `up`: snapshot `plurx.db` (single SQLite file — `sqlite3 plurx.db ".backup ..."` live, or plain copy while the stack is briefly down), keep N=3 rotations per node; (b) write "Rolling back a deploy" in OPERATIONS.md: stop → restore snapshot → check out last-good sha → rebuild; (c) **drill it once** on one node with a scratch DB.
   Acceptance: the drill — revert a migration commit, redeploy, restore, service healthy.

2. **P1 — decide the release model, then make `release-check` green and true.**
   Two coherent options; pick one:
   - **Cut releases:** move `[Unreleased]` under `## [0.2.7] — 2026-08-09` (WO-09 adds the missing entries first), tag `v0.2.7`, let the tag-triggered publish job run for the first time (it has a version-match guard waiting to be used). Then: version-bump PRs must move the CHANGELOG section, tag same-day, `release-check` on a weekly schedule so red is visible.
   - **Subordinate to continuous-main:** document in RELEASING.md that the fleet tracks main, keep CHANGELOG as a rolling log, delete or repurpose the tag machinery.
   Either way, fix the self-inconsistencies: `deploy/unraid-plurx.xml:7-8` pulls `plurx/plurxd` from Docker Hub while RELEASING.md promises `ghcr.io/pjunod/plurx` and **neither image exists** — align the template with whatever CI would actually publish, or mark it "build locally until first tag"; CHANGELOG's `[Unreleased]` compare link points at nonexistent `v0.2.0`.
   Acceptance: `make release-check` exits 0 (option A) or is retired from the docs (option B); unraid template names a real registry path.

3. **P1 — verify the fleet, refresh the ledger.**
   The deployment ledger (`docs/APPLE-NATIVE-SUBTITLES-HANDOFF.md:340-342`, echoed by STATUS.html:269) says every node runs `787eaa6` — 2026-08-01, ~47 merges behind main. Either the week's work is deployed nowhere, or the ledger lies. On the control host: `curl -s http://<node>:32400/api/v1/server` on nynuc + nuc4 → record actual build stamp + schema version; if it reports "(unstamped build)", the private deploy play is using raw `docker compose up -d --build` instead of `make docker-up` — switch it so stamps work. Update the ledger + STATUS.html fleet line with reality.
   Acceptance: both nodes report a stamped build; ledger matches.

4. **P1 — re-pin the mobile-deploy contracts #49 deleted.**
   #46 added in-repo ansible mobile playbooks with CI contract tests (required-device serial gating, test-before-upload); #49 moved the playbooks to the private repo and **deleted the tests** — those behaviors are now ungated everywhere plurx can see. Add minimal CI in the private ansible repo (`ansible-playbook --syntax-check` + grep-level asserts that the required-serial gate and test-before-archive tasks exist), or re-pin from plurx via a documented contract file.
   Acceptance: a playbook edit that drops the required-device assert fails something.

5. **P2 — small ship/docs fixes.** `scripts/ship:131-134` tells the operator to set DV-rung env in "the environment: block in deploy/docker-compose.yml" — a tracked file the next `reset --hard` wipes, silently reverting the experiment; point it at the untracked override/.env instead. Note in deploy/README.md that TestFlight is the Apple delivery path (`--apple` uploads; devices pull via TestFlight — no direct install exists), so "deploy to all Apple devices" = upload + install-on-device step.

## Don't

- Don't move the media mounts into the tracked compose file — the override-file split is deliberate and correct for the `reset --hard` fleet.
- Don't build a 3-node cluster deploy yet — CLUSTERING-PLAN M6 owns it; the current `[cluster]`-tolerant config groundwork is verified correct (rollback-safe, `#[serde(default)]`, example stanza commented).
- Don't re-review the Dockerfile/compose internals — contract-tested and clean (two-stage cache-mounted build, dual ffmpeg with capability assertion, healthcheck, non-root).
