# Overseer — Developer Handoff

Everything a new maintainer needs to build, deploy, and safely modify Overseer. Read the
**Conventions** section before touching any hook — most historical crashes came from breaking
one of those rules.

Current version: **3.5.15** (single source of truth: [`native/Cargo.toml`](native/Cargo.toml)).

---

## 1. What Overseer is

A single native **Rust DLL** that loads inside Umamusume: Pretty Derby (Steam, Global **and**
Japanese clients) and adds translation and quality-of-life features. It is **not** an automation
tool — it never plays the game unattended; it responds to what the player does.

It ships as **`cri_mana_vpx.dll`**, a *proxy loader*: the installer renames the game's real CRI
Sofdec DLL to `cri_mana_vpx_orig.dll`, and Overseer re-exports the same symbols and forwards
every call to the original. Because CriWare loads that DLL *after* `GameAssembly.dll`, IL2CPP is
already resolvable when Overseer initialises.

Control surfaces:
- **Web control panel** at `http://127.0.0.1:1620` (the primary UI).
- **In-game overlay** toggled with **Insert** (imgui, drawn on the D3D11 Present hook).

---

## 2. Repository layout

```
Overseer/
├─ native/                    the overlay DLL — the whole product
│  ├─ src/                    ~90 modules (see the feature map, §5)
│  │  ├─ skip/                event / training / result / shop / skill / rival / inspire
│  │  ├─ performance/         fps / graphics / display / cyspring
│  │  ├─ overseer_compat/     IL2CPP-API compatibility shim
│  │  └─ web/                 index.html + overseer.css (the web SPA, embedded in the DLL)
│  ├─ vendor/hudhook-0.6.5/   patched hudhook (mouse-button key-alias fix; see Cargo.toml patch)
│  ├─ Cargo.toml              version + feature flags
│  └─ target/                 build cache — NOT in git, ~8 GB
├─ advisor/                   Python sidecar bundle (Live Advisor); pyembed = bundled runtime
├─ data/                      optional local data files read next to the DLL
├─ deploy.ps1                 copy an already-built DLL into the game (no rebuild)
├─ build_nllb.bat            release build WITH the neural-MT feature
├─ install.ps1 / uninstall.ps1   end-user proxy install / restore
├─ dl_model.py / dl_1.3b.py       fetch the NLLB weights from HuggingFace
├─ pack_intro.py             build custom-intro media from a video
└─ *.md                      README / CHANGELOG / custom-intro / race-director / this file
```

**Not tracked in git** (see [`.gitignore`](.gitignore)): `native/target/`, the model folders
`nllb-model/` + `nllb-1.3b-model/` (multi-GB, exceed GitHub's 100 MB limit — re-fetch with the
`dl_*.py` scripts), built `*.dll`/`*.pdb`, and local custom-intro media.

---

## 3. Building

Requires **Rust stable (MSVC toolchain)** on Windows. `cargo` is often not on `PATH` — use
`~/.cargo/bin/cargo.exe`.

**Quick dev check** — fast, skips the vendored CTranslate2 C++ compile (no neural MT in the
resulting binary):

```
cd native
cargo check --release        # or cargo build --release
```

Produces `native/target/release/overseer_overlay.dll`.

**Full release with neural translation** (the shipped build — needs CMake + MSVC + `+crt-static`):

```
build_nllb.bat               # release + --features nllb, ~3–4 min
```

The `nllb` feature compiles vendored CTranslate2 and expects the model at `<dll_dir>/nllb/` at
runtime. Fetch the weights once:

```
python dl_model.py           # 600M distilled (default)
python dl_1.3b.py            # 1.3B high-quality mode
```

### Feature flags (`native/Cargo.toml`)

| Flag        | Default | Purpose |
|-------------|:---:|---------|
| `racenet`   | ✓ | Player-horse identity from the msgpack race response. |
| `raceread`  | ✓ | Live race reader (frames + finish placement); pulled in by `racenet`. |
| `races_on`  | ✓ | Race-result auto-skip defaults ON. |
| `freecam`   | ✓ | Retained for internal race telemetry reads (the player-facing camera UI is not shipped). |
| `banner`    | ✓ | Custom title-intro player (needs local `intro_full.bin` / `intro_song.ogg`). |
| `nllb`      | — | In-process NLLB-200 neural MT. **Not** in quick dev builds; ship builds enable it. |

---

## 4. Deploying to the game

`deploy.ps1` copies a built DLL straight into the game — no rebuild.

```
powershell -File deploy.ps1 -NoLaunch    # copy only
powershell -File deploy.ps1              # copy + relaunch via steam://
```

> **Deploy target is `…/UmamusumePrettyDerby_Data/Plugins/x86_64/` ONLY.** A second copy at the
> game root double-loads the DLL and crashes on boot. The deployed file keeps the name
> `cri_mana_vpx.dll` even though the crate builds `overseer_overlay.dll`. Game process name:
> `UmamusumePrettyDerby`. `deploy.ps1` also syncs the `advisor/` sidecar bundle.

---

## 5. Runtime architecture & feature map

| Layer | Modules | Notes |
|-------|---------|-------|
| Loader / proxy | `proxy.rs`, `boot.rs`, `lib.rs` | Re-exports CRI symbols, forwards to `*_orig.dll`, spins the overlay thread. |
| Render + input | `overlay.rs`, `ui_input.rs`, `menu_model.rs` | hudhook D3D11 Present hook + imgui. `menu_model.rs` is the single source of truth for the in-game menu. |
| IL2CPP hooking | `il2cpp.rs`, `htt_il2cpp.rs`, `tt_il2cpp.rs`, `hooks.rs`, `arbiter.rs` | Trampoline detours (`retour`). **One detour per method address** — `arbiter.rs` refuses to stack; fan out to multiple callbacks from a single owner. |
| Main-thread pump | `ui_tempo.rs`, `mainthread.rs` | A single `TweenManager.Update` detour is also the master main-thread marshalling pump. |
| Web panel | `webui.rs`, `http.rs`, `web/index.html`, `web/overseer.css` | Local HTTP server + embedded SPA. |
| Translation / MTL | `mtl.rs`, `nllb.rs`, `glossary.rs`, `names.rs`, `loc_*.rs`, `sql.rs`, `plurals.rs` | glossary → cache → neural tiers; master.mdb + texture localization. |
| Skip engine | `skip/*.rs` | Drives the game's *own* skip/fast-forward routines. |
| Career / reporting | `career.rs`, `webhook.rs`, `response_hook.rs` | Career tracking; additive webhook payloads. |
| Pre-click predictions | `event_reveal.rs`, `race_field.rs`, `race_reveal.rs`, plus `parse_spark_offer` / `parse_career_plan` / `parse_blocked` + the training tiles in `response_hook.rs` | Decode what the server sends the client *before* it acts on it: every event choice's rewards, each training tile's exact gains / failure rate / pre-rolled hint, the whole race field at the entry screen, the race result, the end-of-career spark pool, and the run's rolled race schedule. Pure parsing off the response hook — no IL2CPP, **no requests of our own**. Surfaced on the web Predictions page, and the event / field / training three on an in-game HUD (`overlay.rs::draw_predictions`, setting `predict_hud`) which shows whichever decision has the shortest clock. |

**The passive line.** Overseer *reads* responses the game already received; it never sends a request.
That is what keeps it a companion overlay rather than a bot (Icarus, the automation product, implements
~87 API methods and does send). Traffic the game did not originate is the thing that gets accounts
actioned, so adding a "just fetch X" call is a product decision, not a refactor — and rarely needed:
the client volunteers almost everything already (e.g. it fetches the event reward table itself for 35
of 37 choices). Keep new decoders on the `on_response` fan-out.
| Legacy / affinity | `affinity.rs`, `legacy` routes in `webui.rs` | Affinity from live play; loop planner. |
| Companion feeds | `friendlyplugins.rs`, `umas.rs`, `uma_bridge.rs`, `race_export.rs` | Native in-process exports; no external plugins. |
| Health / diag | `watchdog.rs`, `guard.rs`, `crashlog.rs`, `exctrace.rs`, `diag.rs` | Self-heal stuck flags; crash + exception breadcrumbs. |

**In-game menu tabs** (from `menu_model.rs` — the authoritative list): Gameplay (Superskip:
events/training/races-won-only/shop, Game speed, Auto-unfollow), Team Trials (deck profiles,
opponent hunter, result capture), Visuals (max 3D quality, cloth-physics uncap), Performance
(Low Resource mode, frame rate), Interface (window, layout, intro video), Plugins (race/veterans/
response exports), About (updates, diagnostics).

**Web-panel-only features** (routes in `webui.rs`): translation (`/api/translation/*`), legacy
(`/api/legacy/*`), career + webhooks (`/api/career`, `/api/webhook`), advisor (`/api/advisor`),
AI insights (`/api/ai/learned`), predictions/reveal, veterans, logs (`/api/logs`), CVD/accessibility,
memory + health (`/api/mod/memory`, `/api/mod/health`), Catalogue (static page; the only non-text
route, `/assets/promo/*.png`).

**Binary assets.** `webui.rs` serves exactly one class of binary: the Catalogue banners, baked in
with `include_bytes!` and dispatched *before* the string router (whose payload type is `String`).
Names are matched by equality against the `PROMO` table and never joined onto a filesystem path, so
the route cannot be traversed — a test asserts that, and asserts every embedded banner is actually
referenced by a card. Adding another binary asset means extending that table, not adding file I/O.

---

## 6. Conventions & gotchas (read before editing hooks)

These rules exist because breaking them has caused real, shipped crashes.

- **IL2CPP GC threading.** `il2cpp::GCHandle::new / .target() / free` may run **only on an
  IL2CPP-attached thread** (game/hook/pump). Web- and render-thread code must never touch a
  `GCHandle`; instead set an `AtomicBool` request flag that the main-thread pump honours, and use
  an `AtomicBool` mirror for cross-thread "is this active?" reads. Rust statics are **not** GC
  roots, so any raw managed pointer cached across frames is a use-after-free — always wrap live
  managed objects in a `GCHandle`. This is the root cause of the whole "Collecting from unknown
  thread" / `read at 0x…` crash family.
- **Dialog fields are typed.** When auto-confirming a `DialogCommon.Data`, only read genuine
  `System.String` fields (Title / *ButtonText). **Never** read enum fields (`FormType`,
  `DialogType`, colours) as strings — dereferencing an enum value as a pointer is an instant
  access violation.
- **One detour per method address.** `arbiter.rs` will cede rather than stack two detours on the
  same address. If more than one feature needs a method, hook it once in an owner module and fan
  out to callbacks.
- **JP vs Global.** `loc_ui::is_jp_client()` keys on the executable name; it flips the MT source
  language (`jpn_Jpan` vs `eng_Latn`) and enables the English target on the JP client.
- **Never translate date/number tokens.** A translated month string fed back into the game's
  `DateTime.Parse` has soft-locked views before — name protection and the unsafe-translation
  reject exist to prevent this.
- **Crash + diagnostic logs** are written to
  `…/UmamusumePrettyDerby_Data/Plugins/x86_64/overseer-logs/`
  (`overseer-native.log`, `overseer-crash.log` with a breadcrumb hook/step/faulting address,
  and `overseer-diag.txt` from the Diagnostics button). A logs folder left by older builds at
  the game root is stale — ignore it.

---

## 7. Current status (as of this handoff)

Recently completed and deployed:
- Fixed the skill-purchase crash (a typed enum field was being read as a string on the result
  popup) and a broader class of use-after-free crashes (raw managed pointers cached across frames,
  now `GCHandle`-pinned).
- Hardened native skipping: consecutive-race warning auto-confirm, race-result skip (win-only),
  skill confirm-and-return.
- English translation target on the Japanese client.
- Renamed the weak-PC performance switch to **Low Resource mode**.
- Softened the colour-vision-deficiency filters.
- Repaired the Live Advisor.
- Fixed the "recently translated" feed and settings that were resetting on each launch.

---

## 8. Known issues & pending work

- **In-game choice/branch highlighting was REMOVED** (`choice_marks.rs`, deleted). It drew a box on
  the live choice buttons and on the game's own branch cards. Worth recording because the geometry
  was mostly solved and the remaining blocker is small:
  - The choice buttons are `StoryChoiceController._choiceButtonList`, a **pool** — entries past the
    live ones survive with a zero-width rect, so a two-option event reads as five buttons.
  - The UI canvas is **not** Screen-Space-Overlay. A transform's world position is in canvas units
    (buttons sit ~1.0 apart), while `rect` is in the canvas reference resolution (1440x1920). The
    canvas is **3:4 portrait inside a 16:9 window**, so mapping canvas fractions onto the backbuffer
    stretches x by ~2.37. Correct mapping: offset from the canvas centre / canvas world size
    (`rect * lossyScale`), then onto **`Canvas.pixelRect`** — which carries the pillarbox scale *and*
    its offset. The offset matters: the portrait view is not centred and the side menu is on a
    different canvas.
  - **What blocked it:** `Component.GetComponentInParent(Type)` resolves to null on this build, so
    the Canvas could never be reached and every pass bailed. Any retry needs a different route to
    the Canvas (the 2-arg `GetComponentInParent(Type, bool)`, `Canvas.rootCanvas`, or hooking a
    canvas-owning controller). Everything else resolved fine, including
    `UnityEngine.Canvas::get_pixelRect_Injected` and `Transform::get_lossyScale_Injected`.
  - The game's branch cards are `PartsSingleModeChoiceRewardBranchElement`, held per choice in
    `PartsSingleModeChoiceRewardElement._branchElementList` (names from `global-metadata.dat`, not
    guessed). Locating them via `Object.FindObjectsOfType` and sorting by screen Y works; the card
    count is a free cross-check on the decoder's grouping.
  - `_selectedIndex` capture (which button the player took) is **unrelated and still live**, in
    `skip/result.rs::note_selected_button` — the outcome recorder depends on it.
- **Non-Latin font/overflow for translated text.** A global font+overflow swap at component
  `Awake` was reverted because it also restyled Latin/numeric UI. The helper functions are kept
  in `loc_font.rs` as dead code for a proper **per-translation** swap (apply the OS font + wrap
  only to text that was actually translated into a non-Latin script). Not yet implemented.
- **Inspiration native-skip** is armed (`skip/inspire.rs`, passthrough trace detours) but needs
  one captured live inspiration turn to wire the real skip from the `ENTERED` log lines. Only
  `armed`/`miss` have been observed so far.
- **Rival-intro skip** (`skip/rival.rs`) offsets need re-deriving for the current game build; the
  feature is not exposed as a menu toggle and stays off until verified.
- **Player-facing race camera / capture** is not shipped; `freecam.rs` is retained only for the
  internal telemetry reads it still provides.
- **Live in-game testing is blocked** when other injected tooling is present — co-injection makes
  the game's GC abort on boot (not an Overseer bug). Verification is therefore build-level (clean
  `cargo check`/`build_nllb.bat`), log-reading, and player testing. Keep this in mind: a change
  that compiles is not the same as a change that was exercised in-game.
- **Self-updater URL** points at `https://github.com/REDACTED/REDACTED/releases`
  (`update.rs` / `webui.rs`), which has no releases yet — the in-game update check and Changelog
  link resolve there but find nothing until releases are published.

---

## 9. Where to start

- To change what's in the in-game menu: edit `menu_model.rs` (one list, both renderers consume it).
- To change a web page or endpoint: `webui.rs` (routes) + `web/index.html` (SPA).
- To add or fix a skip: the relevant `skip/*.rs`, following the GCHandle rules in §6.
- To adjust translation behaviour: `mtl.rs` (pipeline/tiers), `glossary.rs`, `nllb.rs`,
  `loc_*.rs`, `names.rs`.
- After any change: `cargo check --release`, then `build_nllb.bat`, then `deploy.ps1`.
