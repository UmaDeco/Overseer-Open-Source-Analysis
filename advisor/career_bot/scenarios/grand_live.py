"""Grand Live ("Grand Concert") scenario strategy — scenario_id 3.

Grand Live is URA training with an idol-concert layer bolted on:

  * **Lessons** — training earns five *performance tokens* (dance / passion /
    vocal / visual / mental). You spend them on *squares* (mini-lessons) to learn
    techniques (flat stat / skill-point bonuses) and to **queue songs**.
  * **Lives** — five scheduled performances (turns 24 / 36 / 48 / 60 / 72). Enter
    a live with **>= 3 songs queued** (``live_data_set.next_live_id_array``) and it
    is a **Great Success** instead of a plain Success.

The whole per-turn train / rest / race / event / finish brain is reused unchanged
from ``UraStrategy`` (our shared scenario base — the same one Unity Cup builds on);
only the lesson-spend and live-perform layers are new, and they live entirely in
this file so a Grand Live fix can never touch the shared URA/MANT paths.

Wired against the Manhattan Cafe (chara 1025) JP career capture 20260712_190959
(410 records). JP-only; Grand Concert is not on Global yet — this is the pre-launch
foundation, structured so scenario-specific values (the square table, thresholds)
update without a rewrite once Global gameplay data lands.

--------------------------------------------------------------------------------
TWO WIRE FACTS that the original handoff docs/reference got WRONG (both proven
directly against the raw capture; see the queue traces below):

  1. **Songs are queued by ``master_square`` (which COSTS tokens), NOT by
     ``reserve_square``.** ``reserve_square`` only sets a cosmetic scalar
     ``live_data_set.reserve_square_id`` (last-write-wins; square_id 0 clears it)
     and queues nothing — verified: reserve_square 40001/40002 (capture req
     0729/0730) left ``next_live_id_array`` frozen at [1006], while ``master_square``
     40001 (req 0758) moved it [1006] -> [1006, 1040] AND dropped vocal/mental
     tokens by the square's cost. So every square is taken via ``master_square``,
     song squares included, and songs are **affordability-gated** like any other.

  2. **The pre-live song dump happens at ``playing_state == 10`` (the live turn),
     not only on ordinary turns.** Verified: turn 60 enters ps 10 with an EMPTY
     queue and masters three song squares there before ``live_start`` (Great
     Success); the finale builds seven. So the lesson gate opens at ps 1 AND 10,
     and ``next_decision`` checks lessons BEFORE performing the live — otherwise
     the bot would perform Live 4 and the finale with 0-1 songs and miss Great
     Success entirely.

v1 scope: a COMPLETE, CORRECT Great-Success career, not an optimal one. Deferred
(post-launch, once Global data lands): optimal song/stat targeting, Live-Bonus
stacking, token-economy banking, and perf-token-aware training scoring. Training
stays the shared URA scorer.
"""
import json
from pathlib import Path

from career_bot.scenarios.base import Decision
from career_bot.scenarios.ura import UraStrategy


LIVE_PLAYING_STATE = 10

_SQUARE_REFERENCE_CACHE = {}


def load_square_reference(base_dir):
    """data/grand_live_squares_core.json, cached by path. Static reference data
    extracted once from master.mdb (89 squares), so a process-lifetime cache is
    enough. Returns {} if missing/unreadable so callers degrade to "nothing
    affordable" rather than crash a turn."""
    if not base_dir:
        return {}
    path = Path(base_dir) / "data" / "grand_live_squares_core.json"
    key = str(path)
    if key in _SQUARE_REFERENCE_CACHE:
        return _SQUARE_REFERENCE_CACHE[key]
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        payload = {}
    _SQUARE_REFERENCE_CACHE[key] = payload
    return payload


def _affordable(square_row, tokens):
    """Every colour in the square's token_cost must be covered by the current
    balance. token_cost keys are Capitalised ("Dance"); the balances are
    lowercased, so compare case-insensitively."""
    cost = (square_row or {}).get("token_cost") or {}
    for token_name, amount in cost.items():
        if int(tokens.get(str(token_name).lower()) or 0) < int(amount or 0):
            return False
    return True


def select_lesson_pick(live_data_set, square_reference):
    """The next square to take this visit as (square_id_str, row), or None when
    nothing affordable is offered. Every pick is taken via master_square.

    Priority (v1 — simple, correct, SP-aware; NOT optimal selection):
      (1) if we are still short of the Great-Success song threshold, an
          AFFORDABLE offered song square (``adds_song_live_id`` set) whose song is
          not already queued — mastering it queues that song and spends tokens.
          Prioritised above everything while short, so performance tokens flow to
          songs first and the >= 3-song target is reached before each live.
      (2) an AFFORDABLE SP-granting square (never cap Skill-Point acquisition).
      (3) any other AFFORDABLE offered square (including extra songs beyond the
          threshold, whose immediate stat/SP bonus still pays off).

    Offered squares are tried in their given order; ties are not optimised (v1
    non-goal). The caller (GrandLiveScenario.run_lessons) re-calls this against
    the FRESH state after each master_square and loops until it returns None.
    """
    squares = (square_reference or {}).get("squares") or {}
    threshold = int((square_reference or {}).get("great_success_song_threshold") or 3)
    offered = (live_data_set or {}).get("next_square_info_array") or []
    perf = (live_data_set or {}).get("live_performance_info") or {}
    tokens = {
        str(k).lower(): v for k, v in perf.items() if not str(k).startswith("max_")
    }
    queued = (live_data_set or {}).get("next_live_id_array") or []
    queued_ids = {int(x) for x in queued if str(x).lstrip("-").isdigit()}

    rows = []
    for offer in offered:
        square_id = str((offer or {}).get("square_id") or "")
        row = squares.get(square_id)
        if row:
            rows.append((square_id, row))

    affordable = [(sid, row) for sid, row in rows if _affordable(row, tokens)]
    if not affordable:
        return None

    # (1) Still short of Great Success -> queue an affordable, not-yet-queued song.
    if len(queued) < threshold:
        for sid, row in affordable:
            song = row.get("adds_song_live_id")
            if song is not None and int(song) not in queued_ids:
                return sid, row
    # (2) SP-granting square.
    for sid, row in affordable:
        if row.get("grants_sp"):
            return sid, row
    # (3) any other affordable square.
    return affordable[0]


class GrandLiveStrategy(UraStrategy):
    """Grand Live (scenario_id 3). Thin overlay on the shared URA brain, mirroring
    UnityStrategy: inherits the entire scenario-agnostic training / event /
    recreation / race engine and adds two Decision actions the runner dispatches
    to GrandLiveScenario — ``lessons`` (spend tokens on offered squares) and
    ``live_perform`` (perform a scheduled live)."""

    scenario_id = 3
    display_name = "Grand Live"
    api_prefix = "single_mode_live"
    # ps 10 = live-ready; 1/2/4/5 are the normal home/race/settle states observed
    # in the capture. (ps 3 mid-race and ps 6 claw-crane never appear in the
    # reference career; if a claw-crane surfaces on Global it will trip blocked-
    # state recovery — a known v1 gap to wire from launch data.)
    allowed_playing_states = frozenset({1, 2, 3, 4, 5, 10})

    def __init__(self, race_planner=None):
        super().__init__(race_planner=race_planner)
        base_dir = self.race_planner.base_dir if self.race_planner else None
        self.square_reference = load_square_reference(base_dir)

    def _event_decision(self, chara, events):
        """Emit an event Decision (same shape URA builds inline). Grand Live must
        drain events itself, BEFORE super(), because URA reads playing_state == 5
        as "goal failed -> finish" — but in Grand Live ps 5 is a benign
        post-action event-settle (verified: 114/116 ps-5 records carry a pending
        event, 0 carry single_mode_finish_common). Draining first keeps URA's
        ps-5->finish from ever firing. Mirrors UnityStrategy._event_decision."""
        event = events[0] or {}
        choice = self._choice(event)
        if choice is None:
            payload = {
                "event_id": event.get("event_id"),
                "_event": event,
                "_current_turn": chara.get("turn", 1),
            }
        else:
            payload = {
                "event_id": event.get("event_id"),
                "chara_id": event.get("chara_id", 0),
                "choice_number": choice,
                "current_turn": chara.get("turn", 1),
            }
        return Decision("event", payload, "event")

    def next_decision(self, state, preset, suppress_print=False):
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        turn = int(chara.get("turn") or 0)
        playing_state = int(chara.get("playing_state") or 0)
        events = data.get("unchecked_event_array") or []

        # Genuine career end only (finish payload / chara.state == 3). NOT ps 5.
        finished = "single_mode_finish_common" in data or chara.get("state") == 3
        if not finished:
            # 1. Drain pending events first (covers the ps-5 event-settle trap).
            if events:
                return self._event_decision(chara, events)

            # 2. Lessons BEFORE the live/train ladder. The gate opens at ps 1 AND
            #    ps 10 so the pre-live song dump (which happens at ps 10) runs
            #    before live_start. run_lessons drains what's affordable this
            #    visit; the next poll finds nothing left and falls through.
            if self._lesson_gate_open(data, chara):
                live_data = data.get("live_data_set") or {}
                if select_lesson_pick(live_data, self.square_reference) is not None:
                    return Decision(
                        "lessons",
                        {"current_turn": turn, "_strategy": self},
                        "Grand Live lessons",
                    )

            # 3. Perform the live once nothing is left to master this visit.
            if playing_state == LIVE_PLAYING_STATE:
                return Decision(
                    "live_perform",
                    {"current_turn": turn, "_strategy": self},
                    "Grand Live performance",
                )

        return super().next_decision(state, preset, suppress_print=suppress_print)

    @staticmethod
    def _lesson_gate_open(data, chara):
        """Squares are only taken on a clean home/live screen: no pending
        event/finish, and not while a race is mid-flight. Opens at playing_state
        1 (ordinary turn) or 10 (a live is due — its home screen still offers
        squares and the queue is built there)."""
        if "single_mode_finish_common" in data:
            return False
        if data.get("unchecked_event_array"):
            return False
        if int(chara.get("state") or 0) in (2, 3):
            return False
        race = data.get("race_start_info")
        if race and race.get("program_id") and int(chara.get("playing_state") or 0) in (2, 3, 4):
            return False
        return int(chara.get("playing_state") or 0) in (1, LIVE_PLAYING_STATE)


class GrandLiveScenario:
    """Runner-side Grand Live operations, isolated from the shared runner paths
    (mirrors the Unity team-race helper). Holds a back-reference to the
    CareerRunner for its logging helper. Dispatched by the runner for the two
    Grand-Live Decision actions."""

    def __init__(self, runner):
        self.runner = runner

    def run_lessons(self, client, strategy, state):
        """Drain every currently-affordable offered square this visit: pick
        (select_lesson_pick), master it, refresh state (the response carries the
        updated live_data_set + a fresh next_square_info_array), repeat until
        nothing is affordable. Capped at 60 iterations as a stall guard (the
        busiest observed visit — the turn-72 pre-finale dump — took ~24).

        Some sub-action responses carry only live_data_set (no chara_info /
        home_info), so each response is merged onto the last-known chara/home
        rather than replacing it wholesale; otherwise the runner's next iteration
        would look chara-less."""
        r = self.runner
        turn = int(((state.get("data") or {}).get("chara_info") or {}).get("turn") or 0)
        taken = 0
        songs = 0
        for _ in range(60):
            data = state.get("data") or {}
            live_data = data.get("live_data_set") or {}
            pick = select_lesson_pick(live_data, strategy.square_reference)
            if not pick:
                break
            square_id, row = pick
            prev_data = data
            resp = client.master_square(int(square_id), turn)
            new_data = resp.get("data") or {}
            if not new_data.get("chara_info") and prev_data.get("chara_info"):
                merged = dict(prev_data)
                merged.update({k: v for k, v in new_data.items() if v is not None})
                merged["chara_info"] = prev_data["chara_info"]
                if not new_data.get("home_info") and prev_data.get("home_info"):
                    merged["home_info"] = prev_data["home_info"]
                resp = dict(resp)
                resp["data"] = merged
            state = resp
            taken += 1
            if row.get("adds_song_live_id") is not None:
                songs += 1
            r._log(
                "grand_live_lesson",
                turn,
                f"{row.get('name_en') or square_id} (master_square)",
            )
        if taken:
            queued = len(
                ((state.get("data") or {}).get("live_data_set") or {}).get(
                    "next_live_id_array"
                )
                or []
            )
            r._log(
                "grand_live_lessons",
                turn,
                f"took {taken} lesson(s), +{songs} song(s); {queued} queued for the next live",
            )
        return state

    def run_live(self, client, state):
        """Perform the live at the current turn — the SAME single call for every
        live, promo AND finale. v1 skips the finale's cosmetic ``/live/live_start``
        jukebox MV (a non-single_mode concert render, not a scenario mechanic) and
        calls only ``single_mode_live/live_start``. Draining the resulting events
        is the caller's job."""
        turn = int(((state.get("data") or {}).get("chara_info") or {}).get("turn") or 0)
        self.runner._log("grand_live_live", turn, "perform live")
        return client.live_start(current_turn=turn)
