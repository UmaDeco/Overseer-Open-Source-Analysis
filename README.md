# Overseer — Open Source Analysis

This repository does **not** document how "Overseer" (an overlay/mod for *Umamusume: Pretty Derby*) works. Its only purpose is to lay out, with verifiable evidence, **which projects its source code copies from**.

Overseer's full source code (version analyzed: `3.5.15`) is included as-is in this repository so anyone can reproduce the analysis. See [`ORIGINAL-OVERSEER-README.md`](ORIGINAL-OVERSEER-README.md) for the project's original README (features, install instructions, etc. — not the focus of this repo).

> **TL;DR:** 42–44% of Overseer's code is verbatim copied from Heaven and umamusume-sweepy (both third-party projects), with no attribution anywhere in Overseer's own documentation. 7 of the 12 features it advertises are majority-copied — and the other 5 are copied too, just to a lesser degree; none of the 12 comes back clean. Full breakdown below.

## Summary

| Comparison | Verbatim identical code lines | Files with the same name |
|---|---|---|
| Overseer vs. **Heaven** | **47.31%** (15,040 / 31,789 lines) | **62.5%** (60 / 96 files) |
| Overseer vs. **umamusume-sweepy** (`advisor/career_bot/`) | **41.5%** (1,342 / 3,230 lines) — up to **78.3%** on a single file | **71.4%** (5 / 7 files, within `career_bot/`) |
| Overseer vs. **Hachimi** | **3.52%** (1,120 / 31,789 lines) | **10.4%** (10 / 96 files) |

### Overall: how much of Overseer is copied

Counting each line once (a line copied from more than one source below still only counts once), across all of Overseer's Rust, Python, and web source, **excluding the vendored third-party `msgpack` Python package** that ships under `advisor/msgpack/` (it's an unmodified copy of the public PyPI library, not Overseer's own code, so it doesn't belong in either bucket):

| | Lines | % |
|---|---|---|
| Matches Heaven, umamusume-sweepy, or Hachimi verbatim | 17,398 | **42.00%** |
| No match in any of those three | 24,030 | 58.00% |

That's the raw count, and it still mixes in lines that carry no real signal either way — `use`/`import`/module-declaration lines and the raw HTML markup of the 3,270-line web dashboard page (`native/src/web/index.html`), which is structure, not logic. Stripping those out and looking only at substantive code:

| | Lines | % |
|---|---|---|
| Matches Heaven, umamusume-sweepy, or Hachimi verbatim | 16,449 | **44.44%** |
| No match in any of those three | 20,565 | 55.56% |

The "no match" row isn't a certificate of originality — it's "not traced to a source we've checked yet" (see [What's still unexplained](#whats-still-unexplained)).

### By feature: how many of Overseer's advertised features are copied

Line-level percentages are accurate but hard to map to "what the user actually gets." So we grouped Overseer's source files by the feature sections in **Overseer's own README** (`ORIGINAL-OVERSEER-README.md`) and ran the same verbatim-match check per group:

| Feature | Lines | Copied | % | Dominant source |
|---|---|---|---|---|
| Race telemetry / free camera | 3,267 | 3,179 | **97.3%** | Heaven |
| Custom title intro | 799 | 763 | **95.5%** | Heaven |
| Core overlay UI framework | 3,783 | 3,424 | **90.5%** | Heaven |
| Team Trials (opponent finder) | 490 | 437 | **89.2%** | Heaven |
| Self-updater | 559 | 491 | **87.8%** | Heaven |
| Performance & visuals | 879 | 468 | **53.2%** | Heaven |
| IL2CPP hooking / plugin-SDK core | 6,642 | 3,428 | **51.6%** | Heaven |
| Skip & speed | 3,835 | 1,413 | 36.8% | Heaven |
| Legacy & inheritance (affinity) | 1,276 | 336 | 26.3% | Heaven |
| Career tracking & guidance (advisor bot) | 7,181 | 1,547 | 21.5% | umamusume-sweepy |
| Translation / localization engine | 4,567 | 788 | 17.3% | Hachimi |
| Web dashboard (control panel) | 7,679 | 1,085 | 14.1% | none dominant |

**7 of these 12 feature areas are majority copied** (over half their code verbatim from one known source): free camera/race telemetry, the custom intro, the core overlay UI, Team Trials, the self-updater, performance/visuals, and the IL2CPP hooking core — all from Heaven. **4 are partially copied** (15–50%): Skip & speed and Legacy/affinity (Heaven), the advisor bot (umamusume-sweepy), and the translation engine (Hachimi, for its formatting/template plumbing specifically). **1 has no dominant copied source**, the web dashboard's own HTML/backend — though even that one reuses umamusume-sweepy's CSS design-token naming (see [Overseer vs. umamusume-sweepy](#overseer-vs-umamusume-sweepy)).

These are group averages: a "partially copied" feature can still contain individual files that are almost entirely copied (`skip/rival.rs` at 97.4%, `master_data.py` at 78.3%) alongside other files in the same feature that are mostly original — the group number blends them. Per-file detail is in each comparison doc.

Full methodology in [`docs/METHODOLOGY.md`](docs/METHODOLOGY.md). Scripts and raw output in [`evidence/`](evidence/) so anyone can rerun the analysis on the exact source included here.

> Note: this is an objective technical comparison (exact-match line and filename counts), not a legal determination. Percentages exclude vendored third-party dependencies on both sides (e.g. `hudhook`).

## About the projects being compared (so it's clear who's who)

- **[Heaven](https://github.com/Nighty3333/Heaven-Internal-Public-Version-)** — the original overlay for *Umamusume: Pretty Derby (Global)*, made by **Night DC (nighty3333)**. An active project with its own hooking engine, imgui overlay, and a compatibility layer (`hachimi_compat/`) that it openly documents as a mirror of Hachimi's plugin SDK ABI — disclosed, not hidden. Heaven has since gone private, no longer publicly hosted, following the copying documented here.
- **[umamusume-sweepy](https://github.com/SweepTosher/umamusume-sweepy)** — a separate, third-party Python career-automation bot + web dashboard created by **SweepTosher**. Covers the parts of Overseer that Heaven doesn't (the Python `advisor/` and the web UI).
- **Hachimi** — the mod-loading / IL2CPP-injection framework/SDK for this ecosystem, used as a base or reference by multiple community overlays (Heaven included, transparently). It's the most "upstream" piece of infrastructure of the group.
- **Overseer** — the project under analysis in this repository.

Heaven explicitly documents that it mirrors Hachimi's SDK — that's disclosed, not hidden. The issue documented here is specific to Overseer: a very large amount of Heaven's code shows up in Overseer word-for-word, and a very large amount of umamusume-sweepy's `career_bot` module shows up in Overseer's own advisor bot, in both cases with no attribution — Heaven's original author (`Night DC : nighty3333`) is stripped from Overseer's `Cargo.toml`, and neither Heaven, umamusume-sweepy, nor Hachimi are mentioned anywhere in Overseer's documentation.

## Overseer vs. Heaven

Full detail: [`docs/COMPARISON-vs-Heaven.md`](docs/COMPARISON-vs-Heaven.md)

Highlights:

- **4 files 100% byte-identical** (empty diff): `native/src/clipboard.rs`, `native/src/reset.rs`, `native/src/tools/time.rs`, and `native/src/overseer_compat/il2cpp_api.rs` (an exact copy of Heaven's `hachimi_compat/il2cpp_api.rs`).
- More than **20 files with 90–99.8% identical lines**, including large modules: `overlay.rs` (2,625 lines, 93.3% identical), `freecam.rs` (1,596 lines, 96.4%), `race_director.rs`, `race.rs`, `intro_player.rs`, `padder.rs`, and the entire `overseer_compat/*` module (which is Heaven's `hachimi_compat/*`, renamed).
- Overseer's `Cargo.toml` keeps the **same version number** (`3.5.15`) and **the same comments, word for word**, as Heaven's, changing only the package name and **emptying the `authors` field** (Heaven declares `authors = ["Night DC : nighty3333"]`; Overseer, `authors = []`).
- Heaven had already moved to a more restrictive, no-copying license **before** this code was copied into Overseer — the copying happened in direct violation of that license, not before it existed. None of Overseer's documents (`README`, `HANDOFF.md`, `CHANGELOG.md`) mention Heaven or Night DC, and the copying was never stopped or brought into compliance.

## Overseer vs. umamusume-sweepy

Full detail: [`docs/COMPARISON-vs-umamusume-sweepy.md`](docs/COMPARISON-vs-umamusume-sweepy.md)

Highlights:

- `advisor/career_bot/master_data.py` is **78.3%** identical to umamusume-sweepy's `career_bot/master_data.py`, line for line — including a 34-function set that appears in Overseer in the same order, and function bodies that are byte-for-byte identical.
- `runner.py`, `races.py`, and `presets.py` share **98%, 100%, and 71%** of their internal function/method names respectively with umamusume-sweepy's versions — names like `_blocked_playing_state`, `_shop_attempt_cost`, and `forced_program` that aren't generic Python, they're project-specific.
- Six of Overseer's Python files import a module, `career_bot.logging_utils`, that **doesn't exist anywhere in the Overseer repository** — along with `career_bot.skills`, `career_bot.items`, and `career_bot.report`, none of which exist either. The code as shipped can't actually import successfully, consistent with files being lifted out of a source tree without carrying every dependency.
- Overseer's web UI stylesheet (`overseer.css`) reuses the same 17-token CSS custom-property naming scheme as umamusume-sweepy's `public/styles.css` (`--surface`, `--text-muted`, `--accent-primary`, `--radius-sm`, etc.) — an arbitrary naming convention that two people don't independently converge on.
- SweepTosher is also the original author of the **"Icarus"** name/concept (Feb 2026) — months before Overseer's developer's own, much larger **[Umamusume-Icarus](https://github.com/Remezzo/Umamusume-Icarus)** product (May 2026), whose git history starts with a commit uploading this exact `career_bot` codebase, authored by an account named `EdenUmaBots`. That product's own distributed build carries the receipts directly: **two of umamusume-sweepy's branding images ship inside it byte-for-byte identical (MD5-matched)**, its stylesheet is 75.4% identical line-for-line, and — unlike every other file in the bundle — its two script files are run through a JS obfuscator, while the assets that gave it away were left untouched. Timeline and detail in [`docs/COMPARISON-vs-umamusume-sweepy.md`](docs/COMPARISON-vs-umamusume-sweepy.md#timeline-and-the-icarus-name).

## Overseer vs. Hachimi

Full detail: [`docs/COMPARISON-vs-Hachimi.md`](docs/COMPARISON-vs-Hachimi.md)

Direct overlap is low (3.52%) and mostly the kind of coincidental matches expected between any Rust project in this domain (IL2CPP hooking, D3D11). That said, a handful of files in **Overseer's translation/localization subsystem** (a feature Heaven doesn't have) show a noticeably higher overlap specifically against Hachimi:

| File in Overseer | % identical | Main source in Hachimi |
|---|---|---|
| `native/src/wrap.rs` | 69.9% | `src/core/utils.rs`, `src/il2cpp/symbols.rs` |
| `native/src/plurals.rs` | 58.4% | `src/core/plurals.rs` |
| `native/src/sql.rs` | 57.0% | `src/il2cpp/sql.rs` |
| `native/src/template.rs` | 52.0% | `src/core/template.rs` |

In other words: the part of Overseer that does **not** come from Heaven (localization) shows signs of coming, at least in part, from Hachimi.

## What's still unexplained

Not everything in Overseer traces back to a known source. After accounting for Heaven, umamusume-sweepy, and Hachimi, a number of files show no meaningful overlap against any of them — most notably the 4,073-line web UI markup (`native/src/web/index.html`) and several mid-size Rust modules (`mtl.rs`, `webui.rs`, `career.rs`, `event_reveal.rs`, `legacy.rs`, `webhook.rs`, and a few smaller ones). We're not claiming these are copied from somewhere — we simply haven't found a source for them yet, and we're saying so rather than leaving it implied. If a source turns up, it'll get the same treatment as everything else here.

## What's next

This repository will grow with more sources to compare against. Each new source gets its own document at `docs/COMPARISON-vs-<name>.md`, following the same reproducible methodology.

---

This analysis was performed with [Claude](https://claude.com).
