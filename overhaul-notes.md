# Overhaul notes — stability, performance, footprint, reporting

Findings and changes from the pass covering: Legacy Loops integration, the soft-lock investigation,
the skip/training soft lock, the performance regression, the Overseer-vs-Translation toggle, the
memory footprint, and career webhooks.

Everything below is in `native/` unless stated otherwise. `cargo test` covers the new logic
(34 tests); `cargo build --release` is clean.

---

## 0. Field-log analysis (2026-07-23, `overseer-log.txt` + `overseer-log (1).txt`)

Two captures of the same append-only log — `(1)` is the newer superset. **36 sessions over ~4 days,
14 624 lines.** The two files are otherwise identical, so everything below is from `(1)`.

36 launches in four days is itself the headline: the user was repeatedly force-closing the game.
Four distinct problems account for it, all confirmed from the log rather than inferred.

### 0.1 The skill-learning auto-confirm HARD-FROZE the game — 2 of 4 attempts

`[skill] auto-clicked Learn` appears 4 times. `[skill] auto-closed result popup` appears **twice**.
The two that didn't complete both look like this:

```
[skill] confirm dialog matched (Learn/Cancel) -> auto-OK (ButtonRight) armed
[skill] auto-clicked Learn (ButtonRight) — spends the SP the player selected
[button]  seen: "SteamInputBlock(Clone)" press=false
[dialog-dump] Data :: {...}
                      ← 92 s / 1348 s of TOTAL log silence →
[proxy] cri_mana_vpx exports forwarded          (the user killed it and relaunched)
```

The silence is what makes this diagnostic rather than suggestive. Overseer normally emits translation
and button lines several times a second; **nothing at all** for 22 minutes means the game's own main
thread wedged — a hang, not a stall. The two successful runs, by contrast, went confirm → result
popup → `PlayOutView` → lobby in under a second.

**Cause.** Every synthetic click Overseer makes is issued from inside `ButtonCommon.Update`, i.e. we
call `OnPointerClick` *while Unity is iterating its own UI update*. For a self-contained advisory OK
that survives (46 such clicks in this log, no incident). The "Learn" handler tears down a dialog
stack **and** fires a network request, and re-entering the EventSystem there deadlocks
`DialogManager` — the same z-order deadlock this codebase already hit twice from `SkipStory`.

**Fix.** `ui_input::click_deferred` posts the click to the next main-thread pump: still the main
thread, but a clean stack outside the UI update (the pattern the friendship-splash callback already
used). The confirm and result-popup clicks now go through it and re-verify the dialog at click time.
Plus a `SKILL_PROGRESS` expectation with **no retry stage** — by the time this stalls the player's SP
is already spent, so both escalations just abandon the flow, lift the input block and hand control
back.

### 0.2 Every event choice was selected THREE times

`[autoskip] selected top event choice` fires 320 times, and **54 of those are runs of 3 within one
second** on the same controller. `IsWaitSelect` doesn't flip synchronously — the selection is queued
and the flag clears a few frames later — so the 250 ms throttle was the only thing between us and
repeat invocations, and it let three through every time.

Invoking a choice handler three times pushes duplicate story continuations. One session in the log
froze immediately after exactly this burst (27 minutes of silence, then a relaunch). Now deduped per
controller instance, reset when a new choice opens — so a genuine second choice is never blocked.

### 0.3 The re-entry guard leaked 15 times, every time across a race

`[watchdog] cleared a stuck in_overseer guard` × 15. Every single occurrence is preceded by the same
two lines:

```
[race] FINISH: ... -> place=N
[race-export] via SetupSimulateData (sim/skip path)
[watchdog] cleared a stuck in_overseer guard -> skips recovered
```

The crate builds with `panic = "abort"`, so Rust emits no landing pads and `Drop` runs **only** when
the guarded call returns to us. A managed C# exception raised inside game code we invoked doesn't
return — IL2CPP unwinds it with SEH straight past our frame — and the guard stays set.

The existing rescue lived in the `ButtonCommon.Update` pump, but **during a race there are no
ButtonCommon instances updating**, so a guard leaked on the way into a race stayed set for the entire
race with every skip silently dead, and was only cleared once buttons reappeared. That is precisely
the window the timestamps show. The deadline is now part of the hold itself (see §1), so it expires
on any thread with no pump involved; the pump keeps the tidy-up and counts the recovery.

### 0.4 The advisor sidecar was being killed, not crashing

`[advisor] sidecar exited (code Some(-1073741510))` × 10. `-1073741510` is `0xC000013A` =
`STATUS_CONTROL_C_EXIT`: the Python child inherited our console and died to a console control event,
after which the supervisor spawned a fresh interpreter. Now spawned with
`CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW`.

### 0.5 Two long-standing comments were wrong, and had cost time

The Scout/Gacha block asserts in several paragraphs that its 1× tempo pin "has **NEVER ONCE
ENGAGED**… 0 occurrences", and instructs the next reader to stop guessing and go hunt the entry
point. The log says otherwise: `[view] Scout/Gacha view begin` **19×** and
`Scout/Gacha InitializeView` **19×** — the pin fires on every scout-open. The claim came from a log in
which `InitializeView` simply had no log line of its own.

The `[gtrace]` scaffolding those investigations added — four extra permanent detours on
`GachaMainViewController` — logged `ENTERED` **zero** times across all 36 sessions. Question answered;
scaffolding removed, comments corrected.

### 0.6 What the log volume itself showed

| lines | source |
|---:|---|
| 4 007 | `[mtl]` per-string REJECTED / gave-up churn |
| 3 789 | `[button] seen:` |
| 1 460 | `[loc/settext]` boot probe (class method inventories) |
| ~420 | `[rrprobe]` + `[skill-probe]` + `[inspire]` boot dumps |

Over half the log is per-string or once-per-launch diagnostics — and before the buffered logger every
one of those lines was an open + write + close on the render thread. They are now at the verbose
level (off by default), and the boot inventory dumps only run when it's on. `[mtl] queue deep` also
appears 63 times, peaking at **600 waiting with 38 evicted** — genuine translation backlog during
skip floods, which the smaller batches and the model/worker gating in §3 and §4 address.

### 0.7 Ruled out

- **`SkipStory suppressed (dialog open)` × 323** looks alarming but is healthy: 75 bursts, longest 7
  in ~1 s, each followed immediately by normal progress. The dialog gate is doing its job.
- **Most multi-hour silences** before a relaunch (78 276 s, 48 505 s, 44 229 s…) are the user going to
  bed, not freezes. Only the 45 s–45 min windows are diagnostic, and those are the cases above.

---

## 1. Soft locks — what was actually wrong

Every soft lock reproduced in this codebase has **the same shape**: a boolean says "an Overseer
action is in progress" or "this screen is armed", one hook sets it and a *different* hook clears it.
When the clearing hook doesn't run — the view was torn down another way, a network error bounced to
title, a coroutine was collapsed, the game was alt-tabbed mid-transition — the flag stays set for the
rest of the session and everything gated on it dies silently.

| Flag | If it sticks | Symptom |
|---|---|---|
| `ui_input::BUSY` | `auto_press` / `click_now` / `auto_close` all bail on line 1 | "the skip just stopped working" |
| `loc_settext::ENTRY_DEPTH` | every string forwards untranslated | "translation stopped mid-session" |
| `skip::result::WINDOW_OPEN` | the result skip stays armed outside its race | auto-presses in unrelated menus |
| `hooks::IN_OVERSEER` | every skip that gates on it dies | (already had an ad-hoc rescue) |

`BUSY` was the worst of these: it is set around `ButtonCommon.OnPointerClick`, i.e. **arbitrary game
code**, which can tear a view down or raise a managed exception that never returns through our frame.

`ENTRY_DEPTH` had a second bug on top: it was a *process-global* `AtomicUsize` for what is a
*call-scope* property, so any other thread reaching a hooked setter while a composite entry was
in flight saw `in_entry()` true and silently forwarded its text untranslated.

### The fix — make "stuck" unrepresentable

New module **`guard.rs`**:

- **`Latch`** — a boolean whose *deadline is part of the stored value*. A reader past the deadline
  sees `false`, so a lost `clear()` self-heals with no watchdog involvement. `BUSY` is now a Latch
  behind an RAII guard.
- **`Gate`** — an armed window with the same guarantee plus an age. `WINDOW_OPEN` is now a Gate, so
  the auto-press engine can never keep acting on a screen it lost track of.
- **`Progress`** — see §2.
- **`tick()`** — a per-frame watchdog driven from the single main-thread pump. It only ever *reads
  and clears atomics* — it never calls into the game — so it cannot itself become the thing that
  hangs. Each rescue is counted and surfaced on the web panel's **System → Health** page, so a soft
  lock now leaves evidence instead of being invisible.

`ENTRY_DEPTH` became a **thread-local with a deadline**, plus a cross-thread mirror so the watchdog
can still see and report a leak.

### Other indefinite waits found and closed

- **`http::get`** (the self-updater) had **no timeouts at all**. A black-holed connection — a captive
  portal, a firewall that drops rather than rejects — parked its thread forever and left the updater
  status on "checking…". Now 15 s connect / 30 s receive.
- **`webui`** had a 15 s read timeout and no write timeout, and spawned an unbounded thread per
  connection. Now 5 s both ways behind a fixed 4-thread pool, so an abandoned socket can't
  accumulate.
- **Window focus** is now a first-class input to the watchdog. Alt-tabbing throttles the game's own
  update loop while wall-clock deadlines keep running, so a *working* skip looked stalled purely
  because the player checked their browser. The progression watchdog only escalates while the game
  window is in the foreground, with a 3 s grace after regaining focus.

### One latent bug the new tests caught

`Gate::age_ms` used `since_ms == 0` as a "never armed" sentinel — but the process clock genuinely
reads 0 for its first millisecond, so a gate armed in that window reported an infinite age and read
*closed the instant it opened*. Keyed off the open flag instead.

---

## 2. Skip & training transitions

The skip legs were **fire-and-forget**: call the game's own `SkipRuntime` / `SkipStory` /
`OnPushSkip`, then assume the flow advances. Nothing retried and nothing gave up, so when a skip was
swallowed — a coroutine collapsed before its continuation registered, an input block that never
lifted, a dialog stacked on top — the player sat on a dead screen with no indication a mod was
involved.

Each leg now stamps a **`Progress` expectation** when it acts, and clears it when the game
*demonstrably* moved on. Resolution comes only from evidence the **game** produced — a new timeline
starting, `ChangeMainView`, reaching Home, or the server advancing the turn — never from a timer.

A stale expectation escalates twice:

1. **Warn** (trace only) — the window has passed; note it and keep watching.
2. **Stand down** — suppress that leg for a 6 s cool-down, release anything we might be holding, and
   lift any stuck `SteamInputBlock` so the player's own taps land.

The worst case degrades from *soft lock* to *tap through this one screen yourself*, and it says so in
the log and on the Health page.

### The retry stage was removed — it caused soft locks rather than fixing them

Stage 1 originally **re-fired the skip**: re-invoke `SkipRuntime` / `SkipStory` on the captured
instance. That was wrong, and it took three incidents to see why. The premise was "these calls are
idempotent, so a false retry costs nothing". The premise is false — they are idempotent *in context*.
The game only ever calls them from inside its own hook for that cut-in, i.e. at the moment it is in
that state. A watchdog fires on an arbitrary later frame, by which point the player is somewhere
else entirely.

What it actually produced, all three visible in the field logs:

- **A use-after-free crash.** `train::LAST_CUTIN` was a raw pointer; the retry ran game code on a
  destroyed helper. (`GameAssembly.dll+0x41ac092`, `step: tween:guard-tick`.) Fixed separately with a
  strong `GCHandle` + `unity_object_destroyed` — but the retry is what dereferenced it.
- **Managed exceptions.** Every retry line in the log is followed within seconds by
  `[guard] recovered: re-entry guard leaked (managed exception unwound past us)`.
- **A soft lock of its own.** `[skip] training cut-in did not clear — retrying the skip once` fired
  while the player was on the Inspiration screen; the succession view was left with an inert button.
  That is the "can't click the Inspiration button mid-career" report.

The asymmetry the original design missed: a false **stand-down** merely pauses us, while a false
**retry** injects a call into a state that did not ask for it. Recovery here means getting out of the
way, not trying harder. Everything that remains is passive — notice, log, release, stand down.

---

## 3. Performance

Ordered by measured impact.

### 3.1 Logging was doing filesystem I/O on the render thread

`tools::log()` opened the file, wrote one line and closed it **per call** — and `paths::log_file()`
ran `GetModuleFileNameW` + `create_dir_all` on **every one of those calls**. The skip pumps, the text
hooks and the response hook all log, all on the game's main/render thread. A skip flood therefore
became a per-frame syscall storm with antivirus filter drivers in the path. **This is the single
largest contributor to "FPS drops during Skip / Fast Forward".**

- Callers now push a formatted line into a queue (one lock, one `String` move); a background thread
  drains it in batches, one open+write per file per flush.
- Per-frame diagnostics moved to a `trace()` level that is **off by default** (one relaxed atomic
  load when nobody is debugging), toggleable from **System → Diagnostics**.
- Identical repeated lines collapse to a single `(+N repeats suppressed)` entry.
- The log rotates at 8 MB, which also bounds what the Logs page re-reads.
- `dll_dir` / `log_dir` resolve once and are cached.

### 3.1b The tween sub-step budget, and two wrong ways to size it

Sub-stepping the UI costs N x the tween work every frame, so the pump needs a per-frame time budget.
Getting that budget wrong is its own performance bug, and it was got wrong twice before it was right.

1. **Fixed 3 ms.** Sized against a 120 fps target (8.3 ms/frame). This game runs far faster than
   that; live telemetry measured a **2.64 ms** frame. Spending 3 ms of a 2.64 ms frame is more than
   the whole frame.
2. **A fixed 20% share of the measured frame.** Backwards: at 522 fps that is **0.53 ms**, six times
   *less* than the fixed budget it replaced. The faster the machine, the harder the pump was
   throttled. This is what "now the skips feel slower" was.

The mistake both times was budgeting against the frame we are *in*. What governs skip speed is
sub-steps per second: with budget `B`, per-step cost `c` and game cost `G`, the pump lands `B/c`
steps in a `G+B` frame, i.e. `(B/c)/(G+B)` steps/sec -- monotonically increasing in `B`. There is no
frame rate at which withholding budget makes a skip finish sooner.

So the budget is now `target_frame - G`, where `G` is recovered by subtracting the pump's own
measured cost from the frame interval (two EMAs; without the subtraction the budget chases its own
tail, since every microsecond the pump spends lands in the next frame's measurement).

`target_frame` slides with the requested tempo -- a 144 fps floor at 1-2x, down to a 60 fps floor at
20x. One fixed floor cannot serve both ends of a 1x-20x slider: at 2x the user wants smoothness, at
20x they have asked for "as instant as it can get" and would rather spend the frame. On this machine
(game cost ~5.5 ms) that is 2.9 ms of pump at the 120 fps floor versus 11 ms at 60 fps -- about
**twice** the sub-steps per second.

A frame-rate dip during a fast skip is therefore expected: it is the pump doing the work. It is not
the same fault as the 200 -> 18 fps collapse, which was 3.1.

### 3.2 Engine-wide hooks armed for a disabled feature

`freecam::install()` detoured `Transform::set_position_Injected`,
`set_localPosition_Injected`, `set_rotation_Injected`, `Internal_LookAt_Injected`,
`Behaviour.set_enabled`, `Camera.get_fieldOfView` and `GameObject.SetActive` — some of the hottest
paths in the entire engine, firing for every object the game moves, shows or hides, in every scene —
**and the free camera has been force-disabled at boot for several builds**. `GameObject.SetActive`
was armed purely as a diagnostic for an investigation that has since concluded.

This is invisible in any per-feature profile because the cost is spread across every transform write
the game makes. They are now armed only when the persisted free-camera preference is on, and only at
boot (patching a 5-byte prologue mid-race is not safe, so "enable it and restart" is the right
trade). The 100 Hz free-camera input poller follows the same gate.

### 3.3 Per-object work that should have been per-frame

`ButtonCommon.Update` fires **once per button per frame** — dozens of invocations on a menu screen —
but most of what Overseer did there is per-*frame*: the deferred-callback pump, the MTL fallback
pump, the result-skip driver, the auto-choice driver, the fast-forward driver. None of them look at
the button. They now run once per frame, keyed on the main-thread frame id.

### 3.4 The response hook scanned every payload eleven times

`on_response` asked eleven independent `contains()` questions of every decrypted API response, each
a full pass — so a 5 MB career response was scanned ~55 MB worth, on the game's network thread, per
response. Replaced with `msgpack::contains_any`, a single pass with a 256-entry first-byte table
(needles retire as they're found). Same answers, one pass.

### 3.5 The web panel polled everything, forever

The SPA polled every page's endpoints whether or not you were looking at them: the Logs tab re-read a
**400 KB tail every 2 s**, the dashboard re-serialised the entire run history every 2.5 s, plus
Performance, Translation, Gameplay and AI on their own timers — each request spawning a fresh OS
thread inside the game process.

- `ovPoll(fn, ms, pages)` runs a job only while its page is visible **and** the tab is foregrounded,
  and refreshes immediately when either becomes true again.
- The log tail is 128 KB and served from a size-keyed cache, so an unchanged log costs a `metadata()`
  call.
- `/api/career` is cached against a version counter every mutator bumps.
- Fixed 4-worker pool instead of thread-per-connection.

### 3.6 Per-`set_text` costs

`set_text` is the hottest hook Overseer owns — every label, every screen, every frame a counter ticks.

- `mtl::translation_active()` called `settings::tl_lang()`, which **clones an `Option<String>` out of
  an ArcSwap** — a heap allocation per on-screen string per frame. Now a non-allocating mirror.
- `user_content_component()` ran **six** IL2CPP virtual calls plus two managed-string decodes per
  call. Now memoized per component and verified against the component's current GameObject pointer
  on each hit, so a recycled slot re-resolves instead of inheriting the previous occupant's
  classification. One virtual call on a hit instead of six, with no loss of accuracy.

### 3.7 Always-on diagnostics

The load/stall profiler sampled every frame and every response, taking a mutex and formatting a
detail string, writing CSV rows — permanently, for everyone. It is a diagnostic, so it is now opt-in
(**System → Diagnostics**).

### 3.8 Legacy-Select field lookups

The new affinity capture reads ids by field *name*; `field_offset` walks every field of the class and
its parents through `CStr`, and the game calls `CalcRelationPoint` once per candidate when it paints
the list. Offsets are resolved once per class and cached. The store is written by a background
thread, never inside the detour.

---

## 4. Memory

The reported ~8 GB is the **whole game process**; the honest question is which part Overseer owns.
**System → Memory** now answers it live (process working set + every Overseer cache), so this stops
being guesswork.

What was found and changed:

| Item | Before | After |
|---|---|---|
| NLLB model | loaded at boot, held for the session **whether or not a language was ever selected** | follows the translation switch, plus a configurable idle unload (default 5 min) |
| `mtl::TRACK_CAP` | 900 **pinned `GCHandle`s** — managed text components (and their meshes/materials/subtrees) the game cannot collect | 250 |
| `mtl::CACHE_CAP` | 60 000 entries × two heap strings | 25 000 (rest reloads from `mtl.json` on demand) |
| `loc_settext::EMITTED_CAP` | 40 000 × two generations = up to 80 000 owned `String`s | 8 000 (the permanent hash set already covers the long tail) |
| `mtl::ATTEMPTED_CAP` | 20 000 | 6 000 |
| `mtl::HOLD_CAP` | 128 pinned handles | 48 |
| `ui_input` name / press / dialog caches | **unbounded** — keyed by raw pointers to objects the game constantly recreates, only cleared on race-result arm/disarm | bounded working sets |
| `loc_settext` user-field cache | (new) | bounded at 4096 |

The model is the big one: ~0.7 GB resident for NLLB-600M int8, ~2 GB for the 1.3B build. Releasing it
when translation is off or idle is most of the answer. It reloads automatically on the next
translation.

**Free cached memory now** (System → Memory) drops every reclaimable cache and hands the pages back
to the OS. Nothing persisted is touched.

---

## 5. Overseer disabled vs Translation disabled

The report was accurate: `bot_enabled` gated **four** things (skips, UI tempo, free camera,
translation dispatch). The response-hook parsers, career tracking, the companion feed, Team Trials
capture, the opponent hunter, the race reader, the exporters, the MTL worker + resident model, the
advisor sidecar and the profiler all kept running. Turning Overseer "off" therefore only ever stopped
translation.

New module **`runtime.rs`** models **two independent switches** plus per-subsystem exceptions:

- **Overseer** (master) — off suspends *everything*. Hooks stay installed (uninstalling a detour
  mid-session is a crash risk) but every one short-circuits in its first branch, which is what
  "suspended" can safely mean inside a live process. Worker threads park, timers stop, the resident
  model is released.
- **Translation** — independent. Run Overseer without translation, or (via an exception) translation
  without Overseer.
- **`keep_*_when_disabled`** — the spec's "unless explicitly configured otherwise", one per
  subsystem, all defaulting off.

Gates were added at the *first* branch of each hot path, not at its consumers — the response hook
returns before the byte scan, `ButtonCommon.Update` returns before any pump, the overlay skips its
post-process, the pack/`master.mdb` translation surfaces follow the translation switch (they were
substituting text even with translation off).

The header now has two pills; **System → What is running** shows each subsystem's live verdict and
its exception checkbox, and the OFF banner names whatever the user chose to keep alive rather than
claiming everything stopped.

---

## 6. Legacy Loops integration

[Umamusume-Legacy-Loops](https://dadudeian.github.io/Umamusume-Legacy-Loops/) contributes one sharp
idea: inheritance is not a per-career decision, it is a **rotation**. Rotate which of a small cast is
the trainee and the others always supply a high-affinity legacy, so one career feeds the next
indefinitely. Its own tool is a fixed four-slot chart over a community compatibility table, and it
explicitly excludes the legacy-side contribution because the author didn't know how it was computed.

Overseer is better placed on both counts, so **`legacy.rs`** adapts the *idea*, not the
implementation:

- **Exact numbers.** Overseer already hooks `SingleModeUtils.CalcRelationPoint`, so every pairing the
  player looks at yields the value the game itself uses — grandparent chain and win-saddle bonus
  included. The matrix builds itself from play; nothing ships and goes stale.
- **Any loop size, scored not asserted.** 3–6 characters, every rotation enumerated, each scored on
  its **weakest** session (a loop is only as good as its worst career), with the average as
  tie-breaker. `affinity_score` is banded, not linear, so the worst ◎ always outranks the best ○ —
  a linear score would happily recommend a rotation that drops out of ◎.
- **Honest coverage.** Every result reports which pairings are measured, which came from the optional
  community prior, and which are unknown — plus exactly which Legacy Select screens would fill the
  gaps. Unknown is reported, never guessed.

Also: ranked parent recommendations (affinity band first, since affinity gates how much of *any*
spark transfers; sparks second), spark decoding with a documented structural fallback that always
preserves the raw id, and colour-coded spark chips in the web panel and the career report.

Optional drop-in data, both under `data/` next to the DLL: `legacy_affinity.json`
(`{"1001-1002": 63}` — the slot for a community compatibility export) and `factor_data.json`
(`{"<id>": {"kind","name","stars"}}` — exact spark names).

---

## 7. Career webhooks

**`webhook.rs`** + a much richer `career::CareerSummary`.

The wire format is a versioned envelope — `{schema, version, event, sent_at, meta, career}` — that is
**additive-only**: new statistics arrive as new keys inside `career`, never by re-shaping what is
already there, and every enabled section is always present (empty/zero rather than absent) so a
parser never has to branch on existence. A Discord URL is auto-detected and rendered as an embed;
anything else gets the raw JSON.

Contents: character, scenario, difficulty, running style, distance, start/end time and duration;
final stats and remaining SP; character/stat/skill/race sparks plus the inheritance affinity the run
started from; skills purchased with SP cost and total spend; races entered, wins, losses, G1 wins,
fans gained, final fan count; training distribution, friendships completed, support bonds, observed
training failures; final rank, evaluation score, achievements, scenario-specific results.

Two robustness notes:

- Server field names change between game updates, so **every optional lookup tries the spellings we
  have seen** and a miss degrades exactly one field. Which sections resolved travels in the payload
  as `sections_resolved`, and every unmodelled integer on the trained-chara block is forwarded under
  `career.raw` — so a statistic we haven't modelled still reaches the consumer.
- Delivery is a bounded queue drained by one background thread, with per-attempt WinHTTP timeouts and
  capped exponential backoff. The game thread never blocks, and a dead endpoint cannot wedge
  anything. Webhook URLs are treated as credentials — never logged in full.

---

## 8. Low Resource Mode was mostly theatre — verified, then made real

Asked to make the mode "more meaningful", the first pass added four tiers, seven more
`QualitySettings` writes and a render-scale slider. Then the resolve report was actually read:

```
[graphics] QualitySettings setters: aa=ok pixellights=ok shadowdist=ok lodcrossfade=ok
  skinweights=MISS particleraycast=MISS softparticles=MISS reflectionprobes=MISS
  maxlod=MISS shadowcascades=MISS asyncupload=MISS
```

**All seven MISSed** — IL2CPP stripped them, so `set_i32`/`set_bool_qs` returned immediately. And the
slider drove `display::set_render_scale`, which has been a deliberate no-op since 2026-07-17 (it
corrupted the display and persisted in the Unity registry). Tiers 2, 3 and 4 were doing *nothing*
beyond tier 1. All of it is deleted; this module's own rule is that a knob nobody can prove works
does not ship as a switch, and that rule was broken by the code enforcing it.

What replaced it is the lever that was sitting in the file the whole time.
`GraphicSettings.ApplyGraphicsQuality(quality, force)` is the game's own device-tier preset — worth
far more than any single flag — but the detour on it has fired **zero** times in every log
(`"gfx_applies": 0`), so the tier chosen inside the hook was unreachable. A class probe settled the
rest: `static=false` (it needs an instance) and none of its 140 methods is an `Instance` accessor.

So the instance is found directly, via
`UnityEngine.Object.FindObjectOfType(typeof(GraphicSettings))`, held with a strong `GCHandle`
(re-acquired when the scene destroys it), and the method driven from the main-thread pump — through
the **trampoline**, so our own detour cannot re-apply the tier on top of a restore. `GetQuality()`
snapshots the game's own tier first, so switching the mode off restores rather than guesses.

Confirmed live:

```
[graphics] game-quality driver: ApplyGraphicsQuality=ok GetQuality=ok FindObjectOfType=ok typeof=ok
[graphics] found the live GraphicSettings — the game's own quality tier is now drivable
[graphics] game quality tier -> 0 (Low Resources tier 3)   # Extreme on
[graphics] game quality tier -> 2 (Low Resources tier 3)   # mode off — restored to the game's own
[graphics] game quality tier -> 0 (Low Resources tier 3)   # back on
```

A tier-driven **frame-rate cap** was also written and removed before shipping. It is a large, real
saving — and the wrong lever for this mode, which its own panel describes as "the fastest option for
Super Skip". Skipping is frame-driven, so capping frames slows down the thing the mode exists to
speed up. A cap belongs on the Frame Rate panel where the user picks it deliberately.

---

## Things deliberately *not* done

- **OCR.** The brief lists "OCR waits" and "OCR frequency", but Overseer has no OCR and never has —
  it reads the game's own decrypted network responses through an IL2CPP hook. The equivalent hot
  path is the response scan, which is covered in §3.4 and §5.
- **Runtime install/uninstall of detours.** Patching a function prologue that another thread may be
  executing is a genuine crash risk. Suspension is done by short-circuiting inside the hook, and the
  free-camera engine hooks are decided at boot.
- **Spark id names without master data.** The structural decoder gets kind and star level right for
  the common families and is honest ("") about names it cannot know, rather than inventing them. Drop
  in `factor_data.json` for exact names.
