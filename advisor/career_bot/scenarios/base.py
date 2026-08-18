"""Scenario strategy contract + per-scenario traits.

Every career scenario (URA, MANT, Unity Cup, ...) is one Strategy class. The
runner drives a generic loop and consults two things:

* ``next_decision(state, preset)`` — the scenario's brain. Returns a
  :class:`Decision` whose ``action`` must be one the runner dispatches:
  ``command`` / ``event`` / ``race`` / ``race_progress`` / ``team_race`` /
  ``finish`` / ``idle`` / ``done``. Any UNKNOWN action string stops the run
  (runner ``else`` branch), so add a dispatch branch before inventing one.

* **Class traits** (below) — declarative facts about the scenario that the
  runner / race planner read instead of hard-coding ``scenario_id == N``
  branches. New scenarios override the traits; core code stays untouched.

HOW TO ADD A NEW SCENARIO
=========================
1. Capture a manual playthrough with the recorder and mine it:
   endpoints prefix, ``chara_info.playing_state`` values seen, per-turn flow.
2. Create ``career_bot/scenarios/<name>.py`` subclassing ``UraStrategy``
   (training/event engine is scenario-agnostic) or ``ScenarioStrategy``.
   Set ``scenario_id``, ``display_name``, ``api_prefix`` and any traits that
   differ from the defaults below.
3. Register the class in ``career_bot/scenarios/__init__.py`` ALL_STRATEGIES.
4. Add the scenario_id -> api_prefix entry to
   ``uma_api/client.py`` SCENARIO_API_PREFIXES (client routing is independent
   of career_bot; the runner warns at start if the two disagree).
5. Frontend: add the ``<option>`` in public/index.html's #preset-scenario-id
   and extend the scenarioType map + section toggles in public/app.js
   (search for "Unity" to find every touch point).
6. Validate offline before a live run: replay captured states through
   ``next_decision`` (see the Unity work: decisions at every captured state,
   zero exceptions, correct actions at the scenario's special turns).

Unity Cup (scenario 2) is the reference implementation for a scenario with
extra mechanics: custom playing_states, a dedicated runner sub-flow
(``_team_race``) and scoring hooks (``_score_command`` override).
"""

from dataclasses import dataclass

from career_bot.logging_utils import get_logger

log = get_logger("scoring")


@dataclass
class Decision:
    action: str
    payload: dict
    reason: str


class ScenarioStrategy:
    # --- identity -------------------------------------------------------
    scenario_id = 0
    display_name = "Unknown Scenario"
    #: endpoint prefix the game client uses for this scenario's career API.
    #: Routing itself lives in uma_api.client.SCENARIO_API_PREFIXES - keep
    #: both in sync (the runner logs a warning when they disagree).
    api_prefix = "single_mode"

    # --- behaviour traits (read by the runner / race planner) -----------
    #: chara_info.playing_state values that are NORMAL flow for this
    #: scenario. Anything outside triggers blocked-state recovery, and if
    #: that fails, the run stops. (Unity adds 7/8/9 for team races.)
    allowed_playing_states = frozenset({1, 2, 3, 4, 5})
    #: scenario has the MANT-style coin shop (pre-race item buying/usage).
    uses_item_shop = False
    #: call {prefix}/race_reward after race_end in the normal race flow.
    calls_race_reward = True
    #: call {prefix}/race_reward when resuming a disconnected race.
    calls_race_reward_on_resume = True
    #: races.py picks rival-overwrite program ids from extra_race_options
    #: (MANT's TS-Climax style multi-race slots).
    has_rival_race_map = False

    def next_decision(self, state, preset, suppress_print=False):
        raise NotImplementedError

    # --- training scorer v2 (career_bot/scoring.py) ----------------------
    # The only training engine. Shared decision flow used by every scenario;
    # subclasses swap the scorer via _v2_score_command (Unity) or call
    # _best_command_v2 from their own _best_command (URA, MANT). Requires the
    # subclass to provide _should_recreate().

    def _v2_score_command(self, cmd, ctx, preset, used):
        """Scorer hook for the v2 flow - scenario subclasses override this to
        swap in their own scorer while keeping the shared decision flow."""
        from career_bot import scoring as sc

        return sc.score_command(cmd, ctx, over_buffer_used=used)

    def _best_command_v2(
        self,
        state,
        chara,
        preset,
        training,
        rest,
        recreation,
        pal_recreation,
        suppress_print=False,
    ):
        """Scorer-v2 decision flow (see career_bot/scoring.py):
        score everything -> hard fail filter -> energy floor -> finale Wit /
        pre-summer prep -> recreation check -> train the best facility."""
        from career_bot import scoring as sc

        cfg = preset.get("scoring_v2") or {}
        turn = int(chara.get("turn") or 0)
        vital = int(chara.get("vital") or 0)
        rest_threshold = int(preset.get("rest_threshold") or 45)

        # One-time per-run over-buffer rainbow allowance (strategy instances
        # are per-career, so this resets naturally).
        used = getattr(self, "_over_buffer_used", None)
        if used is None:
            used = self._over_buffer_used = set()

        data = state.get("data") or {}
        ura_cmds = (data.get("ura_data_set") or {}).get("command_info_array") or []
        ura_cmd_map = {(c.get("command_type"), c.get("command_id")): c for c in ura_cmds}

        ctx = sc.build_context(chara, preset, scenario_id=getattr(self, "scenario_id", None))
        if not suppress_print:
            log.info(f"\n=== Turn {turn} Training Evaluation (v2) ===")
        duel_bonus = float(cfg.get("ura_duel_bonus", 500))
        rows, evals = [], {}
        for cmd in training:
            score, bd = self._v2_score_command(cmd, ctx, preset, used)
            ura_cmd = ura_cmd_map.get((cmd.get("command_type"), cmd.get("command_id"))) or {}
            if ura_cmd.get("versus_event_partner_id"):
                score += duel_bonus
                bd["duel"] = True
            rows.append((score, bd, cmd))
            evals[bd["facility"]] = round(score, 2)
            if not suppress_print:
                log.info(sc.format_eval_line(bd, score))
        state["_training_eval"] = evals

        # Recreation-only finale (opt-in): spend the URA finale's training
        # turns on outings instead of training - tops up mood/energy for the
        # finale races with zero training-failure risk. The finale is turns
        # 73-78; its RACES land on 74/76/78 and are decided before
        # _best_command in next_decision, so the turns this actually touches
        # are the 3 training turns 73/75/77 (same finale window as
        # train_wit_on_finale below). Falls back to rest if no outing is
        # available, and to normal training only when neither exists.
        # (72-turn scenarios never reach turn 73, so this is effectively a
        # URA-length-scenario option.)
        if cfg.get("recreation_on_finale") and turn > 72:
            pick = pal_recreation or recreation or rest
            if pick:
                which = "recreation" if (pal_recreation or recreation) else "rest"
                state["_decision_reason"] = (
                    f"Finale {which} - preset spends the finale's training turns on outings, not training."
                )
                if not suppress_print:
                    log.info(
                        f"--> BOT DECISION (v2): Finale {which} (recreation_on_finale is on)"
                    )
                return pick

        passing = [r for r in rows if r[1].get("fail_ok")]

        # Energy floor: resting beats training on an empty tank.
        if vital <= rest_threshold and rest:
            state["_decision_reason"] = f"Rested - energy {vital} at/below {rest_threshold}."
            if not suppress_print:
                log.info(f"--> BOT DECISION (v2): Rest (energy {vital} <= {rest_threshold})")
            return rest

        # Pre-summer prep (opt-in): arrive at camp rested and in a good mood.
        motivation = int(chara.get("motivation") or 3)
        if cfg.get("must_rest_before_summer") and turn in (35, 59):
            if vital < 70 and rest:
                state["_decision_reason"] = f"Rested before summer camp (energy {vital} < 70)."
                if not suppress_print:
                    log.info("--> BOT DECISION (v2): Rest before summer camp")
                return rest
            if motivation < 5 and recreation:
                state["_decision_reason"] = "Recreation before summer camp - mood below Great."
                if not suppress_print:
                    log.info("--> BOT DECISION (v2): Recreation before summer camp")
                return recreation

        # Hard fail filter: when nothing is worth the risk, spend the turn
        # safely instead of gambling (finale turns prefer Wit's safe value).
        if not passing:
            if turn > 72 and cfg.get("train_wit_on_finale", True):
                wit = next((r for r in rows if r[1]["facility"] == "Wit"), None)
                if wit:
                    state["_decision_reason"] = (
                        "Trained Wit on a finale turn - every option was risky and Wit is the safest value."
                    )
                    if not suppress_print:
                        log.info("--> BOT DECISION (v2): Finale Wit (all facilities above fail threshold)")
                    return wit[2]
            state["_decision_reason"] = "Rested - every facility is above the failure threshold."
            if not suppress_print:
                log.info("--> BOT DECISION (v2): Rest (no facility passes the fail filter)")
            fallback = max(rows, key=lambda r: r[0])[2] if rows else None
            return rest or pal_recreation or recreation or fallback

        best_score, best_bd, best_cmd = max(passing, key=lambda r: r[0])

        if self._should_recreate(
            recreation,
            preset,
            turn,
            motivation,
            vital,
            best_cmd,
            best_score,
            score_gate=float(cfg.get("recreation_score_gate", 60)),
        ):
            state["_decision_reason"] = "Recreation - mood/energy low and no strong training this turn."
            if not suppress_print:
                log.info("--> BOT DECISION (v2): Recreation (mood/energy, no strong training)")
            return recreation

        # Consume the one-time over-buffer rainbow allowance if this pick used it.
        if best_bd.get("over_buffer"):
            used.add(best_bd.get("stat_idx"))

        reason = f"Trained {best_bd['facility']} - v2 score {best_score:.0f}"
        if best_bd.get("duel"):
            reason += " (duel priority)"
        elif best_bd.get("rainbow"):
            reason += f" (rainbow x{best_bd.get('mult')})"
        state["_decision_reason"] = reason + f" at {best_bd['fail']}% fail risk."
        if not suppress_print:
            log.info(
                f"--> BOT DECISION (v2): {best_bd['facility']} "
                f"(Score: {best_score:.1f}, Fail: {best_bd['fail']}%)"
            )
        return best_cmd
