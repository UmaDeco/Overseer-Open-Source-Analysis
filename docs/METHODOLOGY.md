# Methodology

Everything below is reproducible: the scripts live in [`../evidence/scripts/`](../evidence/scripts/) and run directly against the source code included in this repository.

## 1. Line-level comparison (exact-match)

For each project pair (Overseer vs. Heaven, Overseer vs. Hachimi):

1. Every `*.rs` file from each project is collected, **excluding vendored third-party dependencies** (`native/vendor/hudhook-0.6.5/` in both Overseer and Heaven — a public third-party crate, not code belonging to either project).
2. Every line of every file is trimmed, and trivial lines are discarded: lines shorter than 12 characters, and lines that are only `{`, `}`, or `};` (to avoid inflating the percentage with generic Rust syntax).
3. An index of "normalized line → list of files where it appears" is built for the reference project (Heaven or Hachimi).
4. For each file in Overseer, we count what percentage of its non-trivial lines appear **exactly, word-for-word**, in some file of the reference project.
5. The overall percentage is: (Overseer lines found verbatim in the other project) / (total non-trivial lines in Overseer).

This is a **conservative** lower bound: if someone reformats code, renames variables, or reorders lines, this technique won't catch it — so the real degree of similarity (at the logic/structure level) could be even higher than what's reported here.

Script: [`../evidence/scripts/line_overlap_overseer_vs_both.py`](../evidence/scripts/line_overlap_overseer_vs_both.py)
Raw output: [`../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt`](../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt)

## 2. String literal comparison

All `"..."` literals of at least 8 characters are extracted from each project (via a simple regex over the source) and compared as sets, looking for exact matches. This is an independent signal: strings (log messages, file names, UI text) often survive even when someone reformats or renames variables while copying code.

Script: [`../evidence/scripts/string_literal_overlap_heaven_vs_hachimi.py`](../evidence/scripts/string_literal_overlap_heaven_vs_hachimi.py)

## 3. Filename comparison

The base names (`basename`) of every `.rs` file in Overseer are compared against those of each reference project. A single matching filename isn't evidence on its own (`lib.rs`, `mod.rs` are common in any Rust project), but the **proportion** of shared names is a signal of how closely the project layout matches.

## 4. Byte-by-byte verification

For files that step 1 flags with very high overlap (>95%), a direct `diff` is run between the Overseer file and the corresponding file in the reference project to confirm whether they're 100% identical or have some actual difference.

## 5. Overall copied-vs-unexplained total

The README's "Overall" figures are computed differently from the per-source percentages above, to avoid double-counting: a single combined index is built from every non-trivial line in Heaven (`.rs`), Hachimi (`.rs`), and umamusume-sweepy (all source files), then every non-trivial line across Overseer's full considered source (`native/src`, `advisor`, `launcher` — Rust, Python, HTML, CSS) is checked against that combined index once. A line copied from more than one source still only counts once toward "copied."

Two corrections on top of the base method (methods 1–4):

- **Vendored third-party code excluded.** `advisor/msgpack/` is an unmodified copy of the public PyPI `msgpack` package (its own docstring says so) bundled inside Overseer, not code Overseer wrote — same category as `native/vendor/hudhook-0.6.5`. Earlier passes of this script didn't exclude it, which understated the "copied" percentage by folding ~1,200 lines of unrelated third-party code into the "no match" bucket. Fixed by adding `msgpack` to the excluded directories, same as `vendor` and `pyembed`.
- **A "useful lines" variant.** The raw per-line count treats an `import` statement, a `#[derive(...)]` attribute, and a load-bearing function body as equally weighted — but a `use` line matching is weak evidence on its own, and 3,270 of the "no match" lines are raw markup from `native/src/web/index.html` (structure, not logic). The second table filters those out (regex `^(use |mod |pub mod |pub use |extern crate |import |from .* import |#!\[|#\[)`, plus excluding `.html` files entirely) from both the numerator and denominator, so the percentage reflects substantive code rather than being diluted by declarations and markup either direction.

Script: [`../evidence/scripts/total_copied_vs_unexplained.py`](../evidence/scripts/total_copied_vs_unexplained.py)
Raw output: [`../evidence/raw-output/total_copied_vs_unexplained.txt`](../evidence/raw-output/total_copied_vs_unexplained.txt)

## 6. Feature-level breakdown

Overseer's source files were grouped into the feature areas named in Overseer's own README (Translation, Skip & speed, Career tracking & guidance, Legacy & inheritance, Team Trials, Performance & visuals, Race telemetry/free camera, Custom title intro, Self-updater, Web dashboard, plus the underlying overlay UI framework and IL2CPP hooking core that everything else sits on). Each group's total non-trivial line count and how many of those lines match the combined Heaven/Hachimi/umamusume-sweepy index (method 5, above) is reported separately per source, so the dominant source per feature is visible. Grouping is a manual, one-time judgment call based on file purpose and the README's own section headers — reasonable people could draw the boundaries slightly differently, but the underlying per-file percentages (which don't depend on the grouping) are in the per-source comparison docs.

Script: [`../evidence/scripts/feature_level_breakdown.py`](../evidence/scripts/feature_level_breakdown.py)
Raw output: [`../evidence/raw-output/feature_level_breakdown.txt`](../evidence/raw-output/feature_level_breakdown.txt)

## What this analysis is NOT

- Not a legal determination of copyright infringement — that's up to whoever has jurisdiction (the platform, or eventually a court), not a script.
- Doesn't measure "inspiration" or similar architecture — it only counts exact textual matches. The numbers reported are a floor, not a ceiling.
- Explicitly excludes any third-party dependency both projects legitimately use under license — that doesn't count as copying between them.
