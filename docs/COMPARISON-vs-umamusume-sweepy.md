# Overseer vs. umamusume-sweepy

**umamusume-sweepy** is a third-party Python career-automation bot + web dashboard for *Umamusume: Pretty Derby*, created by **SweepTosher**. It is not affiliated with Heaven or Heaven's author.

This comparison covers Overseer's Python advisor module (`advisor/career_bot/`) and web UI stylesheet (`native/src/web/overseer.css`) — the parts of Overseer that Heaven doesn't have an equivalent for (Heaven has no career-automation bot and no web dashboard).

## `advisor/career_bot/` — scoped result

Looking only at the four files with real overlap (the fair, apples-to-apples comparison, since most of Overseer's other 113 files are Rust game-hooking code with nothing to do with a Python automation bot):

| File in Overseer | % identical | Lines |
|---|---|---|
| `advisor/career_bot/master_data.py` | **78.3%** | 403/515 |
| `advisor/career_bot/runner.py` | 35.1% | 755/2154 |
| `advisor/career_bot/races.py` | 35.4% | 120/339 |
| `advisor/career_bot/presets.py` | 28.8% | 64/222 |
| **Combined** | **41.5%** | **1342/3230** |

### `master_data.py`: near-total structural identity

Overseer's `master_data.py` defines 36 functions; umamusume-sweepy's defines 34. **All 34 of umamusume-sweepy's function names appear in Overseer, in the same relative order**, with Overseer adding 2 extra inserted into the sequence. Example of a full function body, byte-for-byte identical in both:

```python
def race_occurrence_id(program_id, year_offset):
    year_key = {
        0: 1,
        24: 2,
        48: 3,
    }.get(int(year_offset or 0), 9)
    return year_key * 100000 + int(program_id or 0)
```

### `runner.py`, `races.py`, `presets.py`: distinctive shared function names

Counting only function/method names (`def ...`, including class methods), not lines:

| File | Shared names | Total in umamusume-sweepy |
|---|---|---|
| `runner.py` | **39** | 40 (98%) |
| `races.py` | **10** | 10 (100%) |
| `presets.py` | **10** | 14 (71%) |

These aren't generic Python names — they're project-specific: `_blocked_playing_state`, `_shop_attempt_cost`, `_track_turn_scores`, `_recover_blocked_state`, `forced_program`, `check_aptitude`, `available_programs`. Independent reimplementations don't converge on the same 39 internal helper names.

### Broken imports — a sign of unedited copy-paste

`advisor/career_bot/runner.py`, `master_data.py`, `presets.py`, `races.py`, `events.py`, and `scenarios/base.py` in Overseer all import from **`career_bot.logging_utils`**:

```python
from career_bot.logging_utils import get_logger, runtime_output_root
```

`runner.py` additionally imports `career_bot.skills`, `career_bot.items`, and `career_bot.report`. **None of `logging_utils.py`, `skills.py`, `items.py`, or `report.py` exist anywhere in the Overseer repository.** These files, as shipped, cannot import successfully — the module tree they depend on was never fully brought over. umamusume-sweepy's `career_bot/` does have `skills.py`, `items.py`, and `report.py`. This is consistent with files being copied out of a source tree without carrying every dependency along.

## `native/src/web/overseer.css` vs. umamusume-sweepy's `public/styles.css`

Raw line-overlap is modest (17.5%, 246/1406 lines), but the interesting part isn't the property values — it's the **design-token naming scheme**. Both stylesheets define a CSS custom-property theming system (light/dark or multi-accent themes) using the same variable names, even though the actual color values differ:

| Shared variable name | Overseer | umamusume-sweepy |
|---|---|---|
| `--bg-start`, `--bg-end` | ✓ | ✓ |
| `--surface`, `--surface-2` | ✓ | ✓ |
| `--text-main`, `--text-muted` | ✓ | ✓ |
| `--accent-primary`, `--accent-primary-rgb`, `--accent-secondary` | ✓ | ✓ |
| `--accent-glow`, `--accent-dim` | ✓ | ✓ |
| `--border-soft`, `--border-strong` | ✓ | ✓ |
| `--radius-sm`, `--radius-md`, `--radius-lg` | ✓ | ✓ |
| `--font-family` | ✓ | ✓ |

Both also implement multiple selectable themes the same way. Seventeen matching, arbitrarily-named design tokens sharing the exact same semantic roles is not something two people converge on independently.

## Timeline and the "Icarus" name

| Date created | Repo | What it is |
|---|---|---|
| 2025-12-30 | [umamusume-sweepy](https://github.com/SweepTosher/umamusume-sweepy) | SweepTosher's career-automation bot — the codebase compared throughout this document |
| 2026-02-27 | [SweepTosher/Icarus](https://github.com/SweepTosher/Icarus) | SweepTosher's own "Icarus" — a small native-API speedrun proof of concept; per its own README, "the automated version shall remain private" |
| 2026-05-23 | [Remezzo/Umamusume-Icarus](https://github.com/Remezzo/Umamusume-Icarus) | Overseer's developer's "Icarus" — a full commercial-grade automation platform under the same name, created almost three months later |

SweepTosher is both umamusume-sweepy's creator and the original author of the "Icarus" name/concept for this kind of tool, predating Remezzo's product by months.

Remezzo's `Umamusume-Icarus` repo ships no public source (the bot is distributed as a compiled build; the repo itself is just docs) — but its git history starts with a commit titled **"Initial upload"** ([`ed80f43`](https://github.com/Remezzo/Umamusume-Icarus/commit/ed80f43fe853d370e742c7cbce5a547830ecc7e0), dated the same day the repo was created), authored by an account named **`EdenUmaBots`**. Its contents are the same `career_bot/` codebase covered in this document (`delay.py`, `events.py`, `items.py`, `master_data.py`, `presets.py`, `races.py`, `report.py`, `runner.py`, `scenarios/`, plus the same `data/` JSON layout) — we confirmed `master_data.py` from that commit matches.

### Direct evidence from Icarus's distributed build

The `Umamusume-Icarus` GitHub repo has no source, but the application itself does — every install bundles a `public/` folder alongside the compiled executable, the same way umamusume-sweepy does. Comparing that bundled `public/` folder directly against umamusume-sweepy's:

- **`broom.png` and `sweep.png` are byte-for-byte identical** (MD5 `dd53699642f937ef9e2e3ccf4f6110a6` and `b2f12b9ca825f8bb904abd256a1a9b3a` respectively, matching in both). These are thematic assets named after "sweepy" — not generic Umamusume art, branding tied to umamusume-sweepy specifically, shipped unchanged inside Icarus.
- **`styles.css` is 75.4% identical line-for-line** (1,887 of 2,501 lines).
- The `races/` image folder has the same 283 files; sampled files (e.g. `Arima Kinen.png`) are byte-identical.
- `app.js` and `ui.js` — unlike every other file in the bundle — are minified through a JavaScript obfuscator (hex-encoded string arrays, `a0_0x...`-style identifiers). `index.html`, `styles.css`, and every image ship as plain, readable files. Obfuscating only the two script files that would contain the actual application logic, while leaving the branding assets and styling untouched, is consistent with an attempt to make this specific comparison harder — the assets that gave it away are exactly the ones nobody thought to obscure.

This isn't a leak of source code — it's comparing what Icarus itself installs on a user's machine against umamusume-sweepy's own public repo, both directly inspectable.

Script: [`../evidence/scripts/icarus_build_vs_umamusume_sweepy.py`](../evidence/scripts/icarus_build_vs_umamusume_sweepy.py)
Raw output: [`../evidence/raw-output/icarus_build_vs_umamusume_sweepy.txt`](../evidence/raw-output/icarus_build_vs_umamusume_sweepy.txt)

### Two more repos checked — clean

For completeness: SweepTosher's own **[Icarus](https://github.com/SweepTosher/Icarus)** proof-of-concept (5 small files: `client.py`, `crypto.py`, `main.py`, `steam_auth.py`, `ticket_gen.js` — a different, much smaller codebase than the `career_bot` module or Remezzo's product) and **[SweepTosher/dumper](https://github.com/SweepTosher/dumper)** (a small packet-dumping tool) were also checked against Overseer directly. Neither shows any meaningful overlap (0.25% and 0.01%, both noise level) — makes sense, since Overseer's copying path runs through the `career_bot` module and the `EdenUmaBots` upload, not through these two smaller, separate tools. Raised here to record that they were checked, not because they turned up anything.

## Note on umamusume-sweepy

None of the above is a criticism of umamusume-sweepy or SweepTosher. This document exists to show where Overseer's code came from — umamusume-sweepy is the origin, not the subject, of the finding.
