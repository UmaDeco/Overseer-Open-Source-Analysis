# Overseer vs. Heaven

**Heaven** is an overlay for *Umamusume: Pretty Derby (Global)* developed by **Night DC (nighty3333)**: [github.com/Nighty3333/Heaven-Internal-Public-Version-](https://github.com/Nighty3333/Heaven-Internal-Public-Version-). It's an active project with its own IL2CPP hooking engine, a D3D11 + imgui overlay, and its source openly documents where every piece comes from (including its compatibility layer with Hachimi's SDK — see below).

Analysis result (methodology in [`METHODOLOGY.md`](METHODOLOGY.md)):

- **47.31%** of Overseer's non-trivial code lines (15,040 of 31,789) appear **exactly, word-for-word**, in Heaven's source code.
- **62.5%** of Overseer's `.rs` files (60 of 96) have **the exact same name** as a file in Heaven.
- Overseer version analyzed: `3.5.15` — coincidentally (or not) the **same version number** declared in Heaven's `Cargo.toml`.

## Files identical byte for byte (empty diff)

Verified with a direct `diff`, not just line-count matching:

- `native/src/clipboard.rs`
- `native/src/reset.rs`
- `native/src/tools/time.rs`
- `native/src/overseer_compat/il2cpp_api.rs` ↔ Heaven's `native/src/hachimi_compat/il2cpp_api.rs`

## Full table (files with ≥40% identical lines)

| File in Overseer | % identical | Lines | Mostly matches (in Heaven) |
|---|---|---|---|
| `src/clipboard.rs` | 100.0% | 55/55 | src/clipboard.rs(79), src/htt_il2cpp.rs(35) |
| `src/reset.rs` | 100.0% | 83/83 | src/reset.rs(83), src/bgm.rs(6) |
| `src/overseer_compat/il2cpp_api.rs` | 100.0% | 67/67 | src/hachimi_compat/il2cpp_api.rs(67), src/il2cpp.rs(12) |
| `src/tools/time.rs` | 100.0% | 12/12 | src/tools/time.rs(12), src/affinity.rs(2) |
| `src/intro_player.rs` | 99.8% | 495/496 | src/intro_player.rs(547), src/htt_il2cpp.rs(25) |
| `src/race_director.rs` | 99.5% | 802/806 | src/race_director.rs(869), src/il2cpp.rs(45) |
| `src/race.rs` | 99.5% | 372/374 | src/race.rs(424), src/freecam.rs(34) |
| `src/audio.rs` | 99.0% | 99/100 | src/audio.rs(119), src/intro_player.rs(7) |
| `src/update.rs` | 98.8% | 79/80 | src/update.rs(107), src/selfupdate.rs(41) |
| `src/startup_probe.rs` | 98.7% | 77/78 | src/startup_probe.rs(77), src/boot.rs(11) |
| `src/overseer_compat/services.rs` | 98.4% | 190/193 | src/hachimi_compat/services.rs(440), src/il2cpp.rs(97) |
| `src/tt_il2cpp.rs` | 98.1% | 52/53 | src/tt_il2cpp.rs(114), src/il2cpp.rs(92) |
| `src/menu_model.rs` | 97.5% | 234/240 | src/menu_model.rs(361), src/overlay.rs(85) |
| `src/skip/rival.rs` | 97.4% | 76/78 | src/skip/rival.rs(112), src/freecam.rs(33) |
| `src/htt.rs` | 96.9% | 251/259 | src/htt.rs(281), src/il2cpp.rs(29) |
| `src/freecam.rs` | 96.4% | 1539/1596 | src/freecam.rs(2070), src/overlay.rs(298) |
| `src/il2cpp_json.rs` | 96.1% | 295/307 | src/il2cpp_json.rs(490), src/il2cpp.rs(54) |
| `src/performance/fps.rs` | 95.6% | 153/160 | src/performance/fps.rs(187), src/skip/mod.rs(22) |
| `src/race_export.rs` | 95.2% | 179/188 | src/race_export.rs(189), src/umas.rs(10) |
| `src/overseer_compat/interceptor.rs` | 95.1% | 58/61 | src/hachimi_compat/interceptor.rs(80), src/il2cpp.rs(45) |
| `src/uma_bridge.rs` | 95.1% | 154/162 | src/uma_bridge.rs(166), src/il2cpp.rs(11) |
| `src/umas.rs` | 94.3% | 133/141 | src/umas.rs(133), src/htt.rs(28) |
| `src/overlay.rs` | 93.3% | 2448/2625 | src/overlay.rs(11250), src/padder.rs(435) |
| `src/padder.rs` | 93.2% | 574/616 | src/padder.rs(895), src/overlay.rs(444) |
| `src/htt_il2cpp.rs` | 92.4% | 306/331 | src/htt_il2cpp.rs(407), src/il2cpp.rs(109) |
| `src/overseer_compat/vtable.rs` | 92.3% | 155/168 | src/hachimi_compat/vtable.rs(157) |
| `src/crashlog.rs` | 90.3% | 112/124 | src/crashlog.rs(118), src/intro_player.rs(4) |
| `src/performance/cyspring.rs` | 89.7% | 70/78 | src/performance/cyspring.rs(70), src/performance/graphics.rs(14) |
| `src/hunter.rs` | 89.2% | 437/490 | src/hunter.rs(497), src/overlay.rs(171) |
| `src/diag.rs` | 88.3% | 106/120 | src/diag.rs(105), src/overlay.rs(83) |
| `src/data.rs` | 88.2% | 82/93 | src/settings.rs(170), src/data.rs(156) |
| `src/skip/shop.rs` | 88.1% | 230/261 | src/skip/shop.rs(280), src/freecam.rs(37) |
| `src/arbiter.rs` | 87.0% | 40/46 | src/arbiter.rs(42), src/diag.rs(2) |
| `src/loadprof.rs` | 86.7% | 111/128 | src/loadprof.rs(122), src/freecam.rs(8) |
| `src/selfupdate.rs` | 86.0% | 412/479 | src/selfupdate.rs(509), src/overlay.rs(156) |
| `src/performance/display.rs` | 85.4% | 123/144 | src/performance/display.rs(126), src/padder.rs(9) |
| `src/followers.rs` | 83.6% | 92/110 | src/followers.rs(93), src/freecam.rs(21) |
| `src/bgm.rs` | 83.3% | 169/203 | src/bgm.rs(265), src/il2cpp.rs(52) |
| `src/tools/mod.rs` | 81.8% | 9/11 | src/tools/mod.rs(9) |
| `src/overseer_compat/init.rs` | 81.0% | 51/63 | src/hachimi_compat/init.rs(53), src/paths.rs(1) |
| `src/overseer_compat/mod.rs` | 80.6% | 54/67 | src/hachimi_compat/mod.rs(54), src/il2cpp.rs(4) |
| `src/http.rs` | 72.3% | 172/238 | src/http.rs(324), src/il2cpp.rs(9) |
| `src/il2cpp.rs` | 71.9% | 443/616 | src/il2cpp.rs(762), src/htt_il2cpp.rs(162) |
| `src/friendlyplugins.rs` | 69.6% | 16/23 | src/friendlyplugins.rs(16), src/freecam.rs(2) |
| `src/affinity.rs` | 68.2% | 296/434 | src/affinity.rs(326), src/overlay.rs(268) |
| `src/skip/train.rs` | 65.1% | 69/106 | src/skip/train.rs(83), src/skip/result.rs(11) |
| `src/settings.rs` | 61.9% | 598/966 | src/settings.rs(4963), src/data.rs(266) |
| `src/skip/event.rs` | 61.2% | 175/286 | src/skip/event.rs(191), src/skip/rival.rs(57) |
| `src/skip/mod.rs` | 60.5% | 356/588 | src/skip/mod.rs(384), src/il2cpp.rs(47) |
| `src/boot.rs` | 57.3% | 176/307 | src/boot.rs(183), src/overlay.rs(84) |
| `src/performance/mod.rs` | 57.1% | 24/42 | src/performance/mod.rs(24), src/settings.rs(1) |
| `src/ui_input.rs` | 55.6% | 168/302 | src/ui_input.rs(195), src/il2cpp.rs(33) |
| `src/lib.rs` | 46.3% | 68/147 | src/overlay.rs(354), src/lib.rs(93) |

Full unfiltered table (the remaining 96 files, including low/expected overlap): [`../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt`](../evidence/raw-output/overseer_vs_heaven_and_hachimi_lines.txt).

## `Cargo.toml`

| | Heaven | Overseer |
|---|---|---|
| `version` | `3.5.15` | `3.5.15` (identical) |
| `authors` | `["Night DC : nighty3333"]` | `[]` (**empty**) |
| `license` field in `Cargo.toml` | `MIT` (stale — see below) | `MIT` |
| Per-dependency comments | Original | **Copied word for word**, including the `[patch.crates-io]` comment for `hudhook` |

None of Overseer's documents (`README`, `HANDOFF.md`, `CHANGELOG.md`) mention Heaven or Night DC anywhere.

### License history

The `license = "MIT"` field in Heaven's `Cargo.toml` is leftover metadata — Heaven had **already moved to a more restrictive, no-copying license before this code was copied into Overseer**. The copying documented here happened in direct violation of that license from the start, not before it existed and not after some later change. That license covers the code either way: Overseer is still distributing it, so it's still bound by the terms Heaven's code is actually under. It wasn't respected — the copying was never stopped or brought into compliance. As of this writing, Heaven's repository is no longer publicly listed on GitHub, consistent with tightening access after the fact.

## The `overseer_compat/` module = Heaven's `hachimi_compat/`, renamed

Heaven has a `native/src/hachimi_compat/` module that **openly declares** itself to be a mirror of the ABI (vtable layout) of Hachimi's v3 plugin SDK, so it can load plugins built for Hachimi inside Heaven. Direct quote from Heaven's header comment:

> "Mirrored from the upstream SDK v3 plugin_api."

Overseer has the same module, same 6 files, renamed to `overseer_compat/`. Overseer's header comment is the same text, with `Heaven`→`Overseer` and `hachimi_init`→`overseer_init` swapped, but leaves the phrase **"Mirrored from the upstream SDK v3 plugin_api"** intact — i.e. it presents as Overseer's own SDK something that is, two steps removed, a copy of Hachimi's ABI made by Heaven.

## A note on Heaven

None of the above is a criticism of Heaven. Heaven doesn't hide its relationship with Hachimi — it documents it in its own code. The subject of this analysis is exclusively the **Overseer → Heaven** relationship.
