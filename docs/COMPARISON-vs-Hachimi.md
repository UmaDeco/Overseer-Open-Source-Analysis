# Overseer vs. Hachimi

**Hachimi** is the mod-loading / IL2CPP-injection framework/SDK that much of this game's modding ecosystem builds on — including Heaven, which discloses it openly (see [`COMPARISON-vs-Heaven.md`](COMPARISON-vs-Heaven.md)). We don't include a repo link here because the source code we analyzed doesn't declare its canonical URL, and we'd rather not risk a wrong attribution.

Analysis result (methodology in [`METHODOLOGY.md`](METHODOLOGY.md)):

- **3.52%** of Overseer's lines (1,120 of 31,789) appear exactly identical in Hachimi's code.
- **10.4%** of Overseer's filenames match Hachimi's.

This overall number is low and mostly **expected noise**: any Rust project doing IL2CPP hooking is going to share generic code patterns (`unsafe` handling, IL2CPP API calls, `use std::...`, etc.) with any other project in the same domain, without that implying copying.

## The exception: the localization subsystem

Heaven has **no** translation/localization feature at all — it's a game-utility overlay (freecam, skip, race telemetry). Overseer does have one (the `loc_*.rs` files, `mtl.rs`, `nllb.rs`, `template.rs`, `plurals.rs`, `sql.rs`, `wrap.rs`), and that's exactly where the highest overlap against Hachimi shows up — which does ship a mature localization engine (`src/core/template.rs`, `src/core/plurals.rs`, `src/il2cpp/sql.rs`, etc.):

| File in Overseer | % identical | Lines | Mostly matches (in Hachimi) |
|---|---|---|---|
| `src/wrap.rs` | 69.9% | 130/186 | src/core/utils.rs(191), src/il2cpp/symbols.rs(90) |
| `src/plurals.rs` | 58.4% | 111/190 | src/core/plurals.rs(130), src/core/hachimi.rs(3) |
| `src/sql.rs` | 57.0% | 106/186 | src/il2cpp/sql.rs(203), src/il2cpp/symbols.rs(63) |
| `src/template.rs` | 52.0% | 78/150 | src/core/template.rs(100), src/il2cpp/symbols.rs(26) |
| `src/localize.rs` | 32.2% | 68/211 | src/core/hachimi.rs(443), src/windows/hachimi_impl.rs(98) |
| `src/loc_story.rs` | 20.3% | 60/296 | src/il2cpp/hook/umamusume/StoryTimelineData.rs(70), src/core/hachimi.rs(37) |
| `src/proxy.rs` | 15.4% | 10/65 | src/windows/proxy/mod.rs(5), src/windows/proxy/cri_mana_vpx.rs(4) |
| `src/loc_db.rs` | 14.6% | 21/144 | src/il2cpp/hook/LibNative_Runtime/Sqlite3/Connection.rs(23), src/il2cpp/symbols.rs(2) |

Interpretation: the part of Overseer's code that does **not** come from Heaven (localization) shows signs of coming, at least partly, from Hachimi — with `plurals.rs`, `template.rs`, and `sql.rs` as the clearest cases (matching filename AND more than half the content matching one specific Hachimi file).

Full unfiltered table: [`../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt`](../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt).

## A note on Hachimi

None of the above is a criticism of Hachimi. It's the shared infrastructure piece of the ecosystem, used (with varying degrees of transparency) by several projects — Heaven discloses it openly. The subject of this analysis is exclusively which parts of **Overseer** appear to derive from Hachimi without attribution.
