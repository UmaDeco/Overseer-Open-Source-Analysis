# Overseer 3.5.15

## New: English on the Japanese client

Overseer now translates the **Japanese** Steam client into English end-to-end, not just localizing the Global build. English is selectable as its own target language, with the neural engine pointed at Japanese source text automatically the moment it detects the JP client. Play the version that gets content first, in a language you can read.

## Native skip, on everything

Skipping now leans on the game's *own* Skip and Fast-Forward routines wherever one exists, instead of faking taps — it's faster, and it can't get out of sync with the UI.

- **Consecutive-race warnings** are confirmed for you reliably now. The pop-up that used to sit there waiting is caught, verified against the front-most dialog, and dismissed inside a proper window so it never slips through.
- **Race results** are advanced by the game's native result-skip the instant a win is detected — and only on a win, never during Team Trials.
- **Skill purchases** confirm the spend and skip straight back to the lobby in one motion.
- **Inspiration and training** fold into the same native-first path.

## Live Advisor, rebuilt

The Live Advisor is working again, with a sharper read on the turn in front of you — which facility looks best, why, and what to expect. It only ever suggests; every call stays yours.

## Cleaner visuals and readable translations

- **Colour-vision filters** were too strong and washed the game out, Tritanopia worst of all. They're softened across the board to a natural correction instead of an overpowering one.
- **Translated text fits its box.** Word-wrap and best-fit sizing keep longer strings inside their frames, without touching the font or size of the Latin and numeric UI around them.

## "Low Resources mode"

The performance one-switch for weak PCs is renamed from *Potato mode* to **Low Resources mode** everywhere in the menu and web panel. Same switch, clearer name.

## Stability

A focused pass on every hard crash and soft-lock reported from real sessions.

- **The skill-purchase crash is fixed.** Confirming a skill buy could close the game outright and leave the confirmation pop-up stranded. The result screen was reading a typed field as if it were text and dereferencing garbage; it now reads only genuine string fields, and the buy-confirm flow completes cleanly.
- **Mid-career and random crashes eliminated.** A whole class of intermittent access-violation crashes traced back to the overlay holding on to game objects the garbage collector had already moved or freed. Every one of those sites — the story controller, the skip flows, race telemetry, shop callbacks — now pins its objects correctly, so they stay valid for as long as the overlay uses them.
- **Boot crashes diagnosed.** The "fatal GC error" some players hit on launch was pinned to *other* injected tools loading alongside the game, not Overseer, and Overseer's own start-up was hardened against it regardless.
- **Self-healing** watches for a stuck internal flag every frame — the cause of every soft-lock found so far — and clears it before a screen can freeze.

## Fixes

- **"Recently translated" was always empty.** The feed dropped every entry it should have kept; it now shows the last strings translated, as intended.
- **Settings stopped forgetting themselves.** The skip-warnings, skip-inspiration and event-auto-choice toggles were silently resetting on every launch. They persist now, with the rest of your configuration.
