import gzip
import base64
import copy
import threading
import time
import json
import random
import math
import uuid
from pathlib import Path

from career_bot.logging_utils import get_logger, runtime_output_root
from career_bot.scenarios import STRATEGIES, scenario_traits
from career_bot.races import RacePlanner
from career_bot.skills import SkillBuyer
from career_bot.items import (
    MantItemManager,
    ITEM_NAMES,
    SHOP_ITEM_COSTS,
    DISPLAY_TO_ID,
    display_to_slug,
)


from career_bot.report import (
    new_report,
    add_event,
    add_api_call,
    add_decision,
    finish_report,
    write_report,
    set_error,
)

# Scenario registry + traits live in career_bot/scenarios/__init__.py;
# runtime_output_root in career_bot/logging_utils.py (both re-exported here
# via the imports above for backwards compatibility).

log = get_logger("runner")


TRAINING_LABELS = {
    101: "Speed",
    102: "Power",
    103: "Guts",
    105: "Stamina",
    106: "Wit",
    601: "Speed",
    602: "Stamina",
    603: "Power",
    604: "Guts",
    605: "Wit",
}

CONDITION_NAMES = {
    1: "Night Owl",
    2: "Slacker",
    3: "Skin Outbreak",
    4: "Slow Metabolism",
    5: "Migraine",
    6: "Practice Poor",
    7: "Hot Topic",
    8: "Charming ○",
    9: "Fast Learner",
    10: "Practice Perfect ◯",
}

PAL_NAMES = {
    1: "Tazuna Hayakawa",
    2: "Director Akikawa",
    3: "Etsuko Otonashi",
    4: "Trainer Kiryuin",
    5: "Sasami Anshinzawa",
    6: "Riko Kashimoto",
    7: "Light Hello",
    8: "Mei Suruga",
    9: "Tsurugi",
    1013: "Mejiro McQueen (Team Sirius)",
    1030: "Rice Shower (Team Sirius)",
    1035: "Winning Ticket (Team Sirius)",
    1016: "Narita Brian (Team Sirius)",
    1002: "Silence Suzuka (Team Sirius)",
    1001: "Special Week (Team Sirius)",
}


_CARD_APT_CACHE = {}


def _load_card_aptitudes(base_dir):
    """data/card_aptitudes.json keyed by full card_id (e.g. '100101') ->
    [turf, dirt, short, mile, middle, long, nige, senko, sashi, oikomi] with
    1=G..8=S. Cached per base_dir; empty dict if unavailable."""
    key = str(base_dir)
    cached = _CARD_APT_CACHE.get(key)
    if cached is not None:
        return cached
    data = {}
    try:
        path = Path(base_dir) / "data" / "card_aptitudes.json"
        if path.exists():
            with open(path, "r", encoding="utf-8") as f:
                data = json.load(f) or {}
    except Exception:
        data = {}
    _CARD_APT_CACHE[key] = data
    return data


class CareerRunner:
    def __init__(self, base_dir):
        self.base_dir = Path(base_dir)
        self.report = None
        self.lock = threading.Lock()
        self.thread = None
        self.stop_requested = False
        self.pause_requested = False
        self.burn_clocks = False
        self.race_planner = RacePlanner(base_dir)
        self.skill_buyer = SkillBuyer(base_dir)
        self.item_manager = MantItemManager()

        self.event_names = {}
        names_path = self.base_dir / "data" / "event_names.json"
        if names_path.exists():
            try:
                with open(names_path, "r", encoding="utf-8") as f:
                    self.event_names = json.load(f)
            except Exception:
                pass

        self.chara_names = {}
        chara_names_path = self.base_dir / "data" / "chara_names.json"
        if chara_names_path.exists():
            try:
                with open(chara_names_path, "r", encoding="utf-8") as f:
                    self.chara_names = json.load(f)
            except Exception:
                pass

        self.status = {
            "run_id": None,
            "running": False,
            "preset": "",
            "scenario_id": 0,
            "turn": 0,
            "steps": 0,
            "last_action": "",
            "last_error": "",
            "finished": False,
            "skills_bought": 0,
            "items_bought": 0,
            "items_used": 0,
            "clocks_used": 0,
            "log": [],
            "action_history": [],
            "turn_details": {},
            "start_time": 0,
            "start_turn": 1,
            "last_fans": 0,
            "total_run_time": "",
            "run_history": [],
        }

    def clear_history(self):
        with self.lock:
            self.status["run_history"] = []

    def _init_debug_log(self, preset=None, scenario_id=4):
        self.report = new_report(preset, scenario_id)

    def _debug(self, event, state=None, data=None):
        row = {
            "event": event,
        }
        if state:
            d = state.get("data") or {}
            chara = d.get("chara_info") or {}
            free = d.get("free_data_set") or {}
            row["turn"] = int(chara.get("turn") or 0)
            row["skill_point"] = int(chara.get("skill_point") or 0)
            row["mant_coin"] = int(
                free.get("coin_num")
                if free.get("coin_num") is not None
                else free.get("gained_coin_num") or 0
            )
            row["motivation"] = int(chara.get("motivation") or 0)
            row["stats"] = self._turn_stats(chara)
        if data:
            row.update(data)
        if self.report:
            add_event(self.report, row)

    def _setup_graceful_shutdown(self):
        if getattr(self, "_shutdown_hooked", False):
            return
        self._shutdown_hooked = True
        # Hooking OS signals in a background thread causes race conditions on Windows
        # and conflicts with FastAPI's native graceful shutdown sequence.
        # The main application (main.py) safely calls .stop() and .join() on this runner instead.
        pass

    def start(
        self,
        client,
        preset,
        initial_result,
        max_steps=2500,
        burn_clocks=False,
        dev_mode=False,
        save_report=False,
    ):
        with self.lock:
            if self.status["running"]:
                raise RuntimeError("Career runner already active")

            # --- Apply Default Values for Missing Keys (Part 2) ---
            if preset.get("event_overrides") is None:
                preset["event_overrides"] = {}
            if preset.get("pal_friendship_score") is None:
                preset["pal_friendship_score"] = [1, 1, 1]
            if preset.get("pal_card_multiplier") is None:
                preset["pal_card_multiplier"] = 1.15
            if preset.get("prioritize_recreation") is None:
                preset["prioritize_recreation"] = True
            if preset.get("manual_purchase_at_end") is None:
                # Default OFF so the bot buys skills mid-run once SP passes the
                # threshold. Users who prefer buying only at career end enable the
                # "Only buy skills at end of career" toggle. (Matches presets.py.)
                preset["manual_purchase_at_end"] = False
            if preset.get("wit_race_search_threshold") is None:
                preset["wit_race_search_threshold"] = 999.0
            if preset.get("skip_double_circle_unless_high_hint") is None:
                preset["skip_double_circle_unless_high_hint"] = False
            if preset.get("friendship_score_groups") is None:
                preset["friendship_score_groups"] = []
            if preset.get("hint_boost_characters") is None:
                preset["hint_boost_characters"] = []

            if preset.get("mant_config") is None:
                preset["mant_config"] = {}
            if preset["mant_config"].get("bbq_unmaxxed_cards") is None:
                preset["mant_config"]["bbq_unmaxxed_cards"] = 2
            if preset["mant_config"].get("charm_failure_rate") is None:
                preset["mant_config"]["charm_failure_rate"] = 15
            if preset["mant_config"].get("tier_thresholds") is None:
                preset["mant_config"]["tier_thresholds"] = {
                    "3": 31,
                    "7": 100,
                    "8": 99999999999,
                }
            if preset["mant_config"].get("whistle_focus_summer") is None:
                preset["mant_config"]["whistle_focus_summer"] = True
            if preset["mant_config"].get("focus_summer_classic") is None:
                preset["mant_config"]["focus_summer_classic"] = 2  # Hoard up to 2
            if preset["mant_config"].get("focus_summer_senior") is None:
                preset["mant_config"]["focus_summer_senior"] = 2  # Hoard up to 2
            if preset["mant_config"].get("mega_summer_bonus") is None:
                preset["mant_config"]["mega_summer_bonus"] = 50
            if preset["mant_config"].get("mega_race_penalty") is None:
                preset["mant_config"]["mega_race_penalty"] = 100
            # ------------------------------------------------------

            self._setup_graceful_shutdown()

            scenario_id = int(preset.get("scenario_id") or 4)
            strategy_cls = STRATEGIES.get(scenario_id)
            if not strategy_cls:
                raise RuntimeError(f"No runner for scenario {scenario_id}")
            client_prefix = getattr(client, "api_prefix", None)
            if client_prefix and client_prefix != strategy_cls.api_prefix:
                # main.py points the client at the scenario of the career that is
                # actually in progress on the server (resume path). Driving that
                # career with the preset's strategy 205s every command until the
                # stuck-guard aborts the run - follow the career, not the preset.
                actual_id = next(
                    (
                        sid
                        for sid, cls in STRATEGIES.items()
                        if cls.api_prefix == client_prefix
                    ),
                    None,
                )
                if actual_id is not None:
                    log.warning(
                        "in-progress career is %s (scenario %s) but preset %r wants "
                        "scenario %s - resuming with the career's strategy",
                        STRATEGIES[actual_id].display_name,
                        actual_id,
                        preset.get("name", ""),
                        scenario_id,
                    )
                    scenario_id = actual_id
                    strategy_cls = STRATEGIES[actual_id]
                else:
                    # client routing (uma_api SCENARIO_API_PREFIXES) and strategy
                    # traits drifted apart - loud warning, this breaks careers.
                    log.warning(
                        "api_prefix mismatch: client=%r strategy=%r (scenario %s)",
                        client_prefix,
                        strategy_cls.api_prefix,
                        scenario_id,
                    )
            log.info(
                "run start: %s (scenario %s) | preset %r",
                strategy_cls.display_name,
                scenario_id,
                preset.get("name", ""),
            )
            self.stop_requested = False
            self.pause_requested = False
            self.burn_clocks = burn_clocks
            self.save_report = save_report
            self.dev_mode = dev_mode
            self.race_planner = RacePlanner(self.base_dir)
            self.skill_buyer = SkillBuyer(self.base_dir)
            self.item_manager = MantItemManager()
            # Per-run Unity team-race stuck-guard state (reset so a prior
            # career's aborted team race can't insta-fail the next one).
            self._team_race_turn = None
            self._team_race_tries = 0
            self._final_skill_point = None

            existing_history = self.status.get("run_history", [])
            self.status = {
                "run_id": str(uuid.uuid4()),
                "running": True,
                "paused": False,
                "preset": preset.get("name", ""),
                "scenario_id": scenario_id,
                "turn": 0,
                "steps": 0,
                "last_action": "started",
                "last_error": "",
                "finished": False,
                "target_turn_reached": False,
                "skills_bought": 0,
                "items_bought": 0,
                "items_used": 0,
                "clocks_used": 0,
                "log": [],
                "action_history": [],
                "turn_details": {},
                "failed_commands": set(),
                "start_time": time.time(),
                "start_turn": (
                    int(
                        (initial_result or {})
                        .get("data", {})
                        .get("chara_info", {})
                        .get("turn", 1)
                    )
                    if initial_result
                    else 1
                ),
                "last_fans": 0,
                "total_run_time": "",
                "run_history": existing_history,
            }
            # The report is always built: the AI datasets learn from its slim
            # per-turn rows (stats, decisions, race results, event choices).
            # save_report only controls the FAT parts - full api_call payload
            # capture and the career_log_*.json disk write.
            self.report = new_report(preset, scenario_id)
            if client:
                client.report = self.report

                if hasattr(client, "set_trace_enabled"):
                    client.set_trace_enabled(save_report)

                def _on_api_log(direction, ep, data, req_id=None):
                    if self.report and getattr(self, "save_report", False):
                        add_api_call(
                            self.report,
                            {
                                "ts": time.time(),
                                "direction": direction,
                                "endpoint": ep,
                                "data": data,
                                "req_id": req_id,
                                "turn": self.status.get("turn", 0),
                            },
                        )

                client.on_api_log = _on_api_log
            self._log_locked(
                "started",
                0,
                f"preset {preset.get('name', '')} (burn_clocks={burn_clocks})",
            )
            self.thread = threading.Thread(
                target=self._run,
                args=(
                    client,
                    preset,
                    initial_result,
                    strategy_cls(self.race_planner),
                    max_steps,
                ),
                daemon=True,
            )
            self.thread.start()

    def stop(self):
        with self.lock:
            self.stop_requested = True

    def clear_stop(self):
        """Clear a latched stop so a fresh run isn't instantly aborted."""
        with self.lock:
            self.stop_requested = False

    def pause(self):
        # Hold the run at the next safe point WITHOUT ending it. The worker
        # thread stays alive (blocked in _wait_if_paused), so resume continues
        # the same career.
        with self.lock:
            self.pause_requested = True

    def resume(self):
        with self.lock:
            self.pause_requested = False

    def snapshot(self):
        # Deep-copy under the lock: the worker thread mutates nested containers
        # (log/action_history lists, turn_details dict, failed_commands set)
        # while FastAPI serializes this on the event loop - a shallow copy
        # would race ("dict changed size during iteration" -> 500).
        with self.lock:
            data = copy.deepcopy(self.status)
            data["burn_clocks"] = self.burn_clocks
            data["stop_requested"] = self.stop_requested
            data["pause_requested"] = self.pause_requested
            return data

    def set_burn_clocks(self, value):
        with self.lock:
            self.burn_clocks = value
            self._log_locked("update_setting", 0, f"burn_clocks set to {value}")

    def _run(self, client, preset, result, strategy, max_steps):

        state = result or {}
        last_turn = -1
        last_turn_for_loop_check = -1
        consecutive_reloads_on_turn = 0
        try:
            for i in range(max_steps):
                if self._should_stop():
                    break
                self._wait_if_paused()
                if self._should_stop():
                    break
                data = state.get("data") or {}
                chara = data.get("chara_info") or {}
                turn = int(chara.get("turn") or 0)

                if turn > 0 and turn == last_turn_for_loop_check:
                    consecutive_reloads_on_turn += 1
                else:
                    consecutive_reloads_on_turn = 0
                last_turn_for_loop_check = turn

                if consecutive_reloads_on_turn > 5:
                    msg = f"Stuck in a loop on turn {turn}, stopping."
                    self._log("error", turn, msg)
                    self._mark(last_error=msg)
                    break

                fans = int(chara.get("fans") or 0)
                if fans > 0:
                    self.status["last_fans"] = fans

                def _check_and_print_turn(current_chara, current_turn):
                    nonlocal last_turn
                    if current_turn != last_turn:
                        if hasattr(client, "wait_turn_delay"):
                            client.wait_turn_delay()
                        last_turn = current_turn
                        stats = self._turn_stats(current_chara)
                        if stats:
                            mood_val = stats.get("motivation", 3)
                            mood_str = {
                                1: "Awful",
                                2: "Bad",
                                3: "Normal",
                                4: "Good",
                                5: "Great",
                            }.get(mood_val, f"Unknown({mood_val})")
                            log.info(
                                f"\n--- Turn {current_turn} | HP {stats.get('hp')}/{stats.get('max_hp')} | Mood: {mood_str} | SPD {stats.get('speed')} STA {stats.get('stamina')} PWR {stats.get('power')} GUT {stats.get('guts')} WIT {stats.get('wit')} SP {stats.get('skill_point')} ---"
                            )

                _check_and_print_turn(chara, turn)

                self._mark(turn=turn)

                stop_on_turn = int(preset.get("stop_on_turn") or 0)
                if stop_on_turn > 0 and turn >= stop_on_turn:
                    self._log(
                        "stop_turn",
                        turn,
                        f"Target turn {stop_on_turn} reached, stopping.",
                    )
                    self._mark(
                        target_turn_reached=True, last_action=f"Stopped on turn {turn}"
                    )
                    break

                self._track_turn_scores(state)

                self.skill_buyer.last_attempt = []
                self.skill_buyer.last_result = {}
                self.item_manager.last_buy_attempt = []
                self.item_manager.last_buy_result = {}
                self.item_manager.last_use_attempt = []
                self.item_manager.last_use_result = {}
                self.skill_buyer.attempt_events = []
                self.item_manager.buy_attempt_events = []
                self.item_manager.use_attempt_events = []

                if data.get("unchecked_event_array"):

                    state = self._drain_events(client, strategy, state, preset)
                    data = state.get("data") or {}
                    chara = data.get("chara_info") or {}
                    turn = int(chara.get("turn") or 0)
                    _check_and_print_turn(chara, turn)
                    self._mark(turn=turn)
                    self._track_turn_scores(state)

                if self._blocked_playing_state(chara):

                    state = self._recover_blocked_state(client, strategy, state, preset)
                    data = state.get("data") or {}
                    chara = data.get("chara_info") or {}
                    turn = int(chara.get("turn") or 0)
                    _check_and_print_turn(chara, turn)
                    self._mark(turn=turn)
                    if self._blocked_playing_state(chara):

                        self._mark(
                            last_action=f"blocked state {chara.get('playing_state')}"
                        )
                        break

                self._debug_turn(state, preset)

                # Temporary debug: Print available recreation commands
                home_info = data.get("home_info") or {}
                all_cmds = home_info.get("command_info_array") or []
                rec_cmds = []
                for c in all_cmds:
                    if c.get("command_type") == 3:
                        cmd_id = c.get("command_id") or 0
                        is_enable = c.get("is_enable", 0)
                        if cmd_id == 390:
                            label = "Pal Outing"
                        else:
                            label = "Regular Outing"
                        rec_cmds.append(
                            f"[{label} (id:{cmd_id}) | Enabled:{is_enable}]"
                        )
                if rec_cmds:
                    log.debug(f"Available Recreations: {', '.join(rec_cmds)}")

                state["_action_history"] = self.status.get("action_history", [])
                state["_failed_commands"] = self.status.get("failed_commands", set())

                # First pass: evaluate silently to determine the intended command
                decision = strategy.next_decision(state, preset, suppress_print=True)
                log.debug(
                    "decision t%s: %s (%s) payload=%s",
                    chara.get("turn"),
                    decision.action,
                    decision.reason,
                    {k: v for k, v in (decision.payload or {}).items() if not k.startswith("_")},
                )

                if decision.action == "command":

                    old_items_bought = self.status.get("items_bought", 0)
                    old_items_used = self.status.get("items_used", 0)

                    state = self._handle_items(
                        client,
                        state,
                        preset,
                        self._command_from_decision(state, decision),
                    )
                    data = state.get("data") or {}
                    events_drained = False
                    if data.get("unchecked_event_array"):

                        state = self._drain_events(client, strategy, state, preset)
                        events_drained = True

                    items_changed = (
                        self.status.get("items_bought", 0) > old_items_bought
                        or self.status.get("items_used", 0) > old_items_used
                    )
                    if events_drained or items_changed:
                        data = state.get("data") or {}
                        chara = data.get("chara_info") or {}
                        turn = int(chara.get("turn") or 0)
                        _check_and_print_turn(chara, turn)
                        self._mark(turn=turn)
                        state["_action_history"] = self.status.get("action_history", [])
                        state["_failed_commands"] = self.status.get(
                            "failed_commands", set()
                        )

                        # Re-evaluate with printing enabled since state changed
                        decision = strategy.next_decision(
                            state, preset, suppress_print=False
                        )
                    else:
                        # Items didn't change, we evaluate again just to print the evaluation
                        _ = strategy.next_decision(state, preset, suppress_print=False)

                elif decision.action == "idle":
                    # Print the idle evaluation reason
                    _ = strategy.next_decision(state, preset, suppress_print=False)

                    if self.report:
                        add_decision(self.report, state, decision)

                reason_str = decision.reason
                if decision.action != "race":
                    items_used_this_turn = self.status.get("turn_details", {}).get(str(chara.get("turn", 0)), {}).get("items_used", [])
                    if items_used_this_turn:
                        reason_str = f"Used: {', '.join(items_used_this_turn)}, {reason_str}"
                
                self._log(decision.action, chara.get("turn", 0), reason_str)
                if decision.action == "idle":
                    self._mark(last_action=reason_str)
                    break
                if decision.action == "done":
                    self._mark(last_action=decision.reason, finished=True)
                    break

                if decision.action == "event":
                    try:
                        state = self._event(
                            client, strategy, decision.payload, state, preset
                        )
                    except Exception as exc:
                        if (
                            "Network error" in str(exc)
                            or "205" in str(exc)
                            or "208" in str(exc)
                        ):
                            log.warning(
                                "event handling failed (%s), reloading career state",
                                exc,
                            )
                            state = self._fresh_career_state(client, strategy, preset)
                            continue
                        raise
                elif decision.action == "command":
                    self._log(
                        "command_exec",
                        decision.payload.get("current_turn", 0),
                        f"{decision.payload.get('command_type')}:{decision.payload.get('command_id')}:{decision.payload.get('command_group_id')}",
                    )
                    self._record_action(decision, chara, state)
                    try:
                        state = client.exec_command(**decision.payload)
                        data = state.get("data") or {}
                        if data.get("unchecked_event_array"):
                            state = self._drain_events(client, strategy, state, preset)
                    except Exception as exc:
                        if (
                            "Network error" in str(exc)
                            or "201" in str(exc)
                            or "391" in str(exc)
                            or "394" in str(exc)
                            or "205" in str(exc)
                            or "208" in str(exc)
                        ):
                            if "205" in str(exc) and decision.action == "command":
                                payload = decision.payload or {}
                                cmd_type = payload.get("command_type")
                                if cmd_type is not None:
                                    cmd_id = (
                                        payload.get("command_id")
                                        or payload.get("command_group_id")
                                        or 0
                                    )
                                    sel_id = payload.get("select_id", 0)
                                    log.warning(
                                        f"205 on command {cmd_type}:{cmd_id} (target {sel_id}), blacklisting it for this run."
                                    )
                                    with self.lock:
                                        self.status.setdefault(
                                            "failed_commands", set()
                                        ).add((cmd_type, cmd_id, sel_id))

                                    if cmd_type == 3:
                                        log.warning(
                                            "Recreation command failed with 205. Redirecting endpoint to Rest/Summer Recreation."
                                        )
                                        try:
                                            turn_idx = payload.get("current_turn", 1)
                                            is_summer = turn_idx in {
                                                36,
                                                37,
                                                38,
                                                39,
                                                40,
                                                60,
                                                61,
                                                62,
                                                63,
                                                64,
                                            }
                                            c_type = 3 if is_summer else 7
                                            c_id = 0 if is_summer else 701
                                            c_g_id = 304 if is_summer else 0
                                            state = client.exec_command(
                                                command_type=c_type,
                                                command_id=c_id,
                                                command_group_id=c_g_id,
                                                select_id=0,
                                                current_turn=turn_idx,
                                                current_vital=payload.get(
                                                    "current_vital", 0
                                                ),
                                            )
                                            data = state.get("data") or {}
                                            if data.get("unchecked_event_array"):
                                                state = self._drain_events(
                                                    client, strategy, state, preset
                                                )
                                            continue
                                        except Exception as rest_exc:
                                            log.error(
                                                f"Rest endpoint also failed: {rest_exc}. Auto-restarting career."
                                            )
                                            try:
                                                client.finish_career(
                                                    current_turn=payload.get(
                                                        "current_turn", 1
                                                    ),
                                                    is_force_delete=True,
                                                )
                                            except Exception:
                                                pass
                                            raise RuntimeError(
                                                "Rest fallback failed after 205. Career deleted to force auto-restart."
                                            )
                            state = self._fresh_career_state(client, strategy, preset)
                            continue
                        if not any(err in str(exc) for err in ("102", "1503")):
                            raise
                        state = self._recover_blocked_state(
                            client, strategy, state, preset
                        )
                        data = state.get("data") or {}
                        chara = data.get("chara_info") or {}
                        if self._blocked_playing_state(chara):
                            self._mark(
                                last_action=f"blocked state {chara.get('playing_state')}"
                            )
                            break
                        continue
                elif decision.action == "race":

                    self._record_action(decision, chara, state)
                    state = self._race(client, state, preset, decision.payload)
                elif decision.action == "race_progress":

                    self._record_action(decision, chara, state)
                    state = self._race_progress(client, decision.payload)
                elif decision.action == "team_race":

                    self._record_action(decision, chara, state)
                    state = self._team_race(
                        client, strategy, state, preset, decision.payload
                    )
                elif decision.action == "finish":

                    self._record_action(decision, chara, state)

                    state = self._buy_skills(client, state, preset, True)

                    data = state.get("data") or {}
                    current_chara = data.get("chara_info") or {}
                    playing_state = int(current_chara.get("playing_state") or 0)

                    if data.get("race_start_info") and playing_state in {2, 3, 4}:
                        self._log(
                            "race_out",
                            decision.payload.get("current_turn", 78),
                            "clearing active race",
                        )
                        try:
                            state = client.race_out(
                                current_turn=decision.payload.get("current_turn", 78)
                            )
                        except Exception as e:
                            if any(
                                err in str(e)
                                for err in ("102", "201", "StateRecoveryError")
                            ):
                                self._log(
                                    "race_out_reconciled",
                                    decision.payload.get("current_turn", 78),
                                    f"graceful exit: {e}",
                                )
                            else:
                                raise
                    state = self._drain_events(
                        client, strategy, state, preset, limit=50
                    )

                    chara = (state.get("data") or {}).get("chara_info") or {}
                    if int(chara.get("skill_point") or 0) > 200:
                        log.warning(
                            f"SP still high ({chara.get('skill_point')}), retrying final purchase..."
                        )
                        state = self._buy_skills(client, state, preset, True)
                        chara = (state.get("data") or {}).get("chara_info") or chara
                    # Remembered for the run-complete webhook (the finish
                    # response's chara_info no longer carries skill_point).
                    self._final_skill_point = int(chara.get("skill_point") or 0)

                    try:
                        current_t = decision.payload.get("current_turn", 78)
                        
                        try:
                            client.factor_select(current_turn=current_t)
                            log.info("--> BOT DECISION: Selected Factors")
                        except Exception as e:
                            if getattr(e, "result_code", None) != 102:
                                log.warning(f"factor_select error (might be okay if older game version): {e}")

                        state = client.finish_career(
                            current_turn=current_t,
                            is_force_delete=False,
                        )
                        self._capture_career_summary(state)
                    except Exception as e:
                        if getattr(e, "result_code", None) in (102, 201) or type(
                            e
                        ).__name__ == "StateRecoveryError":
                            self._log(
                                "finish_reconciled",
                                decision.payload.get("current_turn", 78),
                                f"graceful exit: {e}",
                            )
                        else:
                            raise
                    self._mark(last_action="finish", finished=True)
                    break
                else:

                    self._mark(last_action=decision.action)
                    break

                if decision.action not in {"finish"}:
                    state = self._buy_skills(client, state, preset, False)

                self._advance(decision.action)
                target_mean = 0.5
                sigma = 0.25
                mu = math.log(target_mean) - (sigma**2) / 2.0
                roll = random.lognormvariate(mu, sigma)
                time.sleep(max(0.1, min(1.5, roll)))
        except Exception as exc:
            err_msg = str(exc)
            code = getattr(exc, "result_code", None)

            def _is_code(*codes):
                # Prefer the structured ApiError code; for non-ApiError
                # exceptions fall back to the ANCHORED "API error N on" phrase
                # (never a bare digit, which misclassifies program/item ids).
                if code in codes:
                    return True
                return code is None and any(
                    f"API error {c} on" in err_msg for c in codes
                )

            if "Bot stopped by user" in err_msg:
                user_msg = "API call aborted safely due to Safe Exit."
                log.info(f"RUNNER STOPPED: {user_msg}")
                self._log("stop", self.snapshot().get("turn", 0), user_msg)
                self._mark(last_error="Stopped by user")
            elif _is_code(201, 391, 394):
                user_msg = f"Session Expired. Please relaunch main.py... If error persists, Try Delete Career. ({err_msg[:30]})"
                log.error(f"RUNNER STOPPED: {user_msg}")
                self._log("error", self.snapshot().get("turn", 0), user_msg)
                self._mark(last_error=user_msg)
                if self.report:
                    set_error(self.report, Exception(user_msg))
            elif _is_code(217):
                # 217 = "Session Verification Error" (master.mdb cat 1/2). The server
                # invalidated the session mid-run - no retry on the SAME session can
                # recover it (every load/index/start just 217s again), so stop cleanly
                # instead of crashing into a futile resume + auto-restart storm.
                user_msg = (
                    "Session Verification Error (217): the game session was invalidated. "
                    "Usually the account was opened elsewhere (another device, the real "
                    "client, or a second bot instance) or the server ended the session. "
                    "Make sure it isn't logged in anywhere else, then relaunch main.py."
                )
                log.error(f"RUNNER STOPPED: {user_msg}")
                self._log("error", self.snapshot().get("turn", 0), user_msg)
                self._mark(last_error=user_msg)
                if self.report:
                    set_error(self.report, Exception(user_msg))
            elif _is_code(214):
                user_msg = f"Game Updated (Error 214). Please open the game manually to download data, then restart the bot."
                log.error(f"RUNNER STOPPED: {user_msg}")
                self._log("error", self.snapshot().get("turn", 0), user_msg)
                self._mark(last_error=user_msg)
                if self.report:
                    set_error(self.report, Exception(user_msg))
            else:
                import traceback

                trace_str = traceback.format_exc()
                # Full traceback goes to console, the debug log file AND
                # crash_trace.txt so a crash is always diagnosable post-hoc.
                log.error("RUNNER CRASH: %s\n%s", exc, trace_str)

                crash_log_path = runtime_output_root(self.base_dir) / "crash_trace.txt"
                try:
                    crash_log_path.parent.mkdir(parents=True, exist_ok=True)
                    with open(crash_log_path, "a", encoding="utf-8") as f:
                        f.write(
                            f"--- CRASH AT {time.strftime('%Y-%m-%d %H:%M:%S')} ---\n"
                        )
                        f.write(trace_str)
                        f.write("\n\n")
                except Exception:
                    pass

                self._log("error", self.snapshot().get("turn", 0), str(exc))
                self._mark(last_error=str(exc))
                if self.report:
                    set_error(self.report, exc)
        finally:
            end_time = time.time()
            start_time = self.status.get("start_time", end_time)
            total_time = end_time - start_time
            m, s = divmod(total_time, 60)
            h, m = divmod(m, 60)
            time_str = f"{int(h):02d}:{int(m):02d}:{int(s):02d}"
            start_turn = self.status.get("start_turn", 1)
            if self.status.get("finished") or self.status.get("target_turn_reached"):
                turn_desc = (
                    f" (Resume from Turn {start_turn})" if start_turn > 1 else ""
                )
                log.info(
                    f"Total run Time: {time_str}{turn_desc} | Fans: {self.status.get('last_fans', 0):,}"
                )
            with self.lock:
                self.status["total_run_time"] = time_str
                if self.status.get("finished") or self.status.get(
                    "target_turn_reached"
                ):
                    history = self.status.setdefault("run_history", [])
                    history.append(
                        {
                            "run_id": self.status.get("run_id"),
                            "start_turn": start_turn,
                            "total_run_time": time_str,
                            "fans": self.status.get("last_fans", 0),
                            "scenario_id": self.status.get("scenario_id", 0),
                            # Wall-clock finish time (epoch) - the UI renders it
                            # in the viewer's local timezone.
                            "finished_at": int(time.time()),
                        }
                    )
            if self._should_stop():
                self._log("stop", self.snapshot().get("turn", 0), "stop requested")
                if self.report:
                    finish_report(self.report, "stopped")
            else:
                if self.report:
                    finish_report(
                        self.report, "finished" if self.status["finished"] else "error"
                    )
            self._mark(running=False)
            if self.report:
                try:
                    root_trace_dir = runtime_output_root(self.base_dir) / "bot_logs"
                    if getattr(self, "save_report", False):
                        # Full debug log to disk; write_report also feeds the
                        # AI datasets (ledger-guarded, never double-counts).
                        out = write_report(self.report, root_trace_dir)
                        log.info(f"career report written: {out}")
                    else:
                        # No debug log requested - still feed the AI datasets
                        # so learning no longer depends on the debug toggle.
                        from career_bot.ai_ingest import export_report_once
                        export_report_once(self.report, root_trace_dir, source="bot")
                except Exception as e:
                    log.error(f"failed to write/export report: {e}")

    def _should_stop(self):
        with self.lock:
            return self.stop_requested

    def _wait_if_paused(self):
        # Block the worker thread at a safe point (top of the turn loop) while a
        # pause is active. Stop always wins, so a paused run can still be
        # cancelled. status["paused"] tracks the real thread state for the UI.
        blocked = False
        while True:
            with self.lock:
                if self.stop_requested or not self.pause_requested:
                    if blocked:
                        self.status["paused"] = False
                        self._log_locked(
                            "resume", self.status.get("turn", 0), "run resumed"
                        )
                    return
                if not blocked:
                    self.status["paused"] = True
                    self._log_locked(
                        "pause", self.status.get("turn", 0), "run paused"
                    )
                    blocked = True
            time.sleep(0.25)

    def _advance(self, action):
        with self.lock:
            self.status["steps"] += 1
            self.status["last_action"] = action

    def _mark(self, **values):
        with self.lock:
            self.status.update(values)

    def _log_locked(self, action, turn, detail):
        # Mirror the UI timeline into the debug log file so post-mortems see
        # exactly what the operator saw (status["log"] keeps only 120 rows).
        log.debug("[ui] %s t%s: %s", action, turn, detail)
        items = self.status.setdefault("log", [])
        items.append(
            {
                "id": len(items) + 1,
                "action": action,
                "turn": int(turn or 0),
                "detail": str(detail or ""),
                "time": time.strftime("%H:%M:%S"),
            }
        )
        if len(items) > 120:
            del items[: len(items) - 120]

    def _log(self, action, turn, detail):
        with self.lock:
            self._log_locked(action, turn, detail)

    def _capture_career_summary(self, finish_state):
        """Distil the finish_career response into status["career_summary"]
        for the run-complete Discord webhook (rich embed in main.py).

        single_mode_finish_common carries the finished chara's final entry
        AND the account's full trained-chara roster, so grade points, final
        stats, sparks (factor ids) and the all-time ranking all come straight
        from the server - no local bookkeeping needed.
        """
        try:
            data = (finish_state or {}).get("data") or {}
            common = data.get("single_mode_finish_common") or {}
            roster = common.get("trained_chara") or []
            my_id = common.get("trained_chara_id")
            mine = next(
                (t for t in roster if t.get("trained_chara_id") == my_id), None
            )
            if not mine:
                log.debug("career summary: own trained_chara %s not in roster", my_id)
                return

            # key= on the score alone: tuple comparison would TypeError on
            # tied scores when a roster entry has card_id None.
            scored = sorted(
                (
                    (int(t.get("rank_score") or 0), t.get("card_id"), t.get("trained_chara_id"))
                    for t in roster
                ),
                key=lambda row: row[0],
                reverse=True,
            )
            position = next(
                (i + 1 for i, row in enumerate(scored) if row[2] == my_id), 0
            )
            summary = {
                "card_id": mine.get("card_id"),
                "rank_score": int(mine.get("rank_score") or 0),
                "rank": mine.get("rank"),
                "stats": {
                    "speed": mine.get("speed"),
                    "stamina": mine.get("stamina"),
                    "power": mine.get("power"),
                    "guts": mine.get("guts"),
                    "wiz": mine.get("wiz"),
                },
                "fans": int(mine.get("fans") or 0),
                "wins": mine.get("wins"),
                "skill_point": getattr(self, "_final_skill_point", None),
                "race_count": len(mine.get("race_result_list") or []),
                "skill_count": len(mine.get("skill_array") or []),
                "factor_ids": [
                    f.get("factor_id")
                    for f in (mine.get("factor_info_array") or [])
                    if f.get("factor_id")
                ],
                "scenario_id": mine.get("scenario_id"),
                "ranking": {
                    "position": position,
                    "total": len(roster),
                    "top": [
                        {"card_id": cid, "rank_score": score}
                        for score, cid, _tid in scored[:5]
                    ],
                },
            }
            self._mark(career_summary=summary)
            log.debug("career summary captured: %s", summary)
        except Exception as e:  # noqa: BLE001 - webhook garnish must never kill a finish
            log.warning("career summary capture failed: %s", e)

    def _build_reasoning(self, orig_action, action, facility, stats, decision, state):
        """Human-readable 'why' bullets for the UI Action Log. Best-effort and
        purely cosmetic: the strategy engine records its rationale on the command
        Decision (see ura._best_command); here we pair it with the per-facility
        training scores. Returns [] when we have nothing better than the
        frontend's own synthesis (which then takes over)."""
        reasons = []
        engine = (getattr(decision, "reason", "") or "").strip()
        if orig_action == "command" and engine and engine != "URA Command Exec":
            reasons.append(engine)
        if action == "train":
            evals = (state or {}).get("_training_eval") or {}
            if evals:
                ordered = sorted(evals.items(), key=lambda kv: kv[1], reverse=True)
                chosen = next(
                    (kv for kv in ordered if kv[0].lower() == str(facility).lower()),
                    ordered[0],
                )
                runner_up = next((kv for kv in ordered if kv[0] != chosen[0]), None)
                if runner_up:
                    reasons.append(
                        f"Chose {chosen[0]} ({chosen[1]:.0f}) over next-best "
                        f"{runner_up[0]} ({runner_up[1]:.0f})."
                    )
            hp = stats.get("hp", 0)
            mx = stats.get("max_hp", 100) or 100
            reasons.append(
                f"HP is low ({hp}/{mx}), so this training had to beat resting."
                if hp < mx * 0.35
                else f"HP is workable ({hp}/{mx}), allowing training."
            )
        seen = set()
        out = []
        for r in reasons:
            if r and r not in seen:
                seen.add(r)
                out.append(r)
        return out[:5]

    def _record_action(self, decision, chara=None, state=None):
        payload = decision.payload or {}
        action = decision.action
        orig_action = action
        turn = int(payload.get("current_turn") or 0)
        stats = self._turn_stats(chara or {})
        detail = self._format_turn_stats(stats) or str(decision.reason or "")
        facility = ""
        if action == "command":
            command_type = int(payload.get("command_type") or 0)
            command_id = int(
                payload.get("command_id") or payload.get("command_group_id") or 0
            )
            select_id = int(payload.get("select_id") or 0)
            if command_type == 1:
                action = "train"
                facility = TRAINING_LABELS.get(command_id, str(command_id))
            elif command_type == 8:
                action = "medic"
            elif command_type == 7:
                action = "rest"
                facility = str(command_id)
            elif command_type == 3:
                action = "recreation"
                if select_id > 0:
                    char_id_str = str(select_id)
                    facility = self.chara_names.get(
                        char_id_str,
                        PAL_NAMES.get(
                            select_id, PAL_NAMES.get(select_id - 9000, char_id_str)
                        ),
                    )
                else:
                    facility = str(command_id)
            else:
                action = f"command {command_type}"
                facility = str(command_id)
        elif action in {"race", "race_progress"}:
            action = "race"
            program_id = int(payload.get("program_id") or 0)
            if program_id and self.race_planner:
                facility = self.race_planner.label(program_id)
            else:
                facility = str(program_id or "")
        elif action == "finish":
            action = "finish"
        row = {
            "turn": turn,
            "action": action,
            "facility": facility,
            "detail": detail,
            "stats": stats,
            "time": time.strftime("%H:%M:%S"),
        }
        reasoning = self._build_reasoning(orig_action, action, facility, stats, decision, state)
        if reasoning:
            row["reasoning"] = reasoning
        with self.lock:
            history = self.status.setdefault("action_history", [])
            if (
                history
                and history[-1].get("turn") == row["turn"]
                and history[-1].get("action") == row["action"]
                and history[-1].get("facility") == row["facility"]
            ):
                history[-1] = row
            else:
                history.append(row)

            turn_details = self.status.setdefault("turn_details", {})
            current_detail = turn_details.setdefault(
                str(turn),
                {
                    "stats": {},
                    "training_eval": {},
                    "items_bought": [],
                    "skills_learned": [],
                    "decision": None,
                    "facility": None,
                    "races": [],
                },
            )
            current_detail["stats"] = stats
            current_detail["decision"] = action
            current_detail["facility"] = facility
            if state and "_training_eval" in state:
                current_detail["training_eval"] = state["_training_eval"]

    def _turn_stats(self, chara):
        if not chara:
            return {}
        return {
            "hp": int(chara.get("vital") or 0),
            "max_hp": int(chara.get("max_vital") or 100),
            "motivation": int(chara.get("motivation") or 0),
            "speed": int(chara.get("speed") or 0),
            "stamina": int(chara.get("stamina") or 0),
            "power": int(chara.get("power") or 0),
            "guts": int(chara.get("guts") or 0),
            "wit": int(chara.get("wiz") or 0),
            "skill_point": int(chara.get("skill_point") or 0),
            "fans": int(chara.get("fans") or 0),
        }

    def _format_turn_stats(self, stats):
        if not stats:
            return ""
        mood_str = {1: "Awful", 2: "Bad", 3: "Normal", 4: "Good", 5: "Great"}.get(
            stats["motivation"], str(stats["motivation"])
        )
        return (
            f"HP {stats['hp']}/{stats['max_hp']} | "
            f"MOOD {mood_str} | "
            f"SPD {stats['speed']} STA {stats['stamina']} PWR {stats['power']} "
            f"GUT {stats['guts']} WIT {stats['wit']} SP {stats['skill_point']}"
        )

    def _get_skill_name(self, skill_id):
        if not hasattr(self, "_skill_data_cache"):
            skill_path = self.base_dir / "data" / "skill_data.json"
            if skill_path.exists():
                try:
                    with open(skill_path, "r", encoding="utf-8") as f:
                        self._skill_data_cache = json.load(f)
                except Exception:
                    self._skill_data_cache = {}
            else:
                self._skill_data_cache = {}

        skill_info = self._skill_data_cache.get(str(skill_id), {})
        return skill_info.get("name", f"Skill ID: {skill_id}")

    def _blocked_playing_state(self, chara):
        """A playing_state outside the scenario's declared normal set means
        the career is stuck (e.g. a minigame or unknown screen) and needs
        recovery. Each scenario declares its own set via strategy traits
        (Unity adds 7/8/9 for team races)."""
        playing_state = int((chara or {}).get("playing_state") or 1)
        traits = scenario_traits((chara or {}).get("scenario_id"))
        blocked = playing_state not in traits.allowed_playing_states
        if blocked:
            log.debug(
                "blocked playing_state %s (scenario %s allows %s)",
                playing_state,
                traits.scenario_id,
                sorted(traits.allowed_playing_states),
            )
        return blocked

    def _recover_blocked_state(self, client, strategy, state, preset=None):
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        if int(chara.get("playing_state") or 0) == 6:
            turn = chara.get("turn", 1)
            if hasattr(client, "minigame_end"):
                state = client.minigame_end(current_turn=turn)
            else:
                api_prefix = getattr(client, "api_prefix", "single_mode_free")
                state = client.call(
                    f"{api_prefix}/minigame_end",
                    {
                        "result": {
                            "result_state": 1,
                            "result_value": 0,
                            "result_detail_array": None,
                        },
                        "current_turn": turn,
                    },
                )
            data = state.get("data") or {}
            if data.get("unchecked_event_array"):
                state = self._drain_events(client, strategy, state, preset)
            return state
        try:
            if hasattr(client, "hard_reset"):
                state = client.hard_reset()
            else:
                state = self._fresh_career_state(client, strategy)
        except Exception as e:
            log.error(f"Blocked State Recovery Failure: {e}")
            return state
        return state

    def _debug_turn(self, state, preset):
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        free = data.get("free_data_set") or {}
        self.skill_buyer.preview(state, preset)
        self._debug(
            "turn",
            state,
            {
                "owned_skills": self._debug_owned_skills(state),
                "inventory": self._debug_inventory(state),
                "server_skill_tips_raw": chara.get("skill_tips_array") or [],
                "server_owned_skill_raw": chara.get("skill_array") or [],
                "skill_rows_enriched": self._debug_skill_options(state, preset),
                "bot_skill_candidates": list(self.skill_buyer.last_candidates),
                "bot_skill_selected": list(self.skill_buyer.last_selected),
                "bot_skill_attempt": list(self.skill_buyer.last_attempt),
                "bot_skill_result": dict(self.skill_buyer.last_result),
                "server_shop_rows_raw": free.get("pick_up_item_info_array") or [],
                "shop_rows_enriched": self._debug_item_buy_options(state, preset),
                "bot_shop_candidates": list(self.item_manager.last_buy_options),
                "bot_shop_selected": list(self.item_manager.last_buy_selected),
                "bot_shop_attempt": list(self.item_manager.last_buy_attempt),
                "bot_shop_result": dict(self.item_manager.last_buy_result),
                "decision_item_use_rows": list(self.item_manager.last_use_options),
                "bot_item_use_selected": list(self.item_manager.last_use_selected),
                "bot_item_use_attempt": list(self.item_manager.last_use_attempt),
                "bot_item_use_result": dict(self.item_manager.last_use_result),
            },
        )

    def _debug_skill_options(self, state, preset):
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        points = int(chara.get("skill_point") or 0)
        owned = {
            int(item.get("skill_id") or 0) for item in chara.get("skill_array") or []
        }
        owned_groups = {
            self.skill_buyer.skill_to_group_id.get(skill_id, skill_id // 10)
            for skill_id in owned
        }
        priority = self.skill_buyer._priority_context(preset)
        blacklist = self.skill_buyer._blacklist(preset)
        selected = {
            item["skill_id"]: item
            for item in self.skill_buyer._candidates(chara, preset)
        }
        result = []
        for tip in chara.get("skill_tips_array") or []:
            resolved = self.skill_buyer.resolve_skill_tip(
                tip, owned, owned_groups, priority, blacklist, preset
            )
            skill_id = int((resolved or {}).get("resolved_skill_id") or 0)
            cost = int((resolved or {}).get("cost") or 0)
            selected_flag = skill_id in selected
            skip_reason = (resolved or {}).get("skip_reason")
            if not skip_reason and cost > points:
                skip_reason = "unaffordable"
            elif not skip_reason and not selected_flag:
                skip_reason = "rule_rejected"
            result.append(
                {
                    "skill_id": skill_id,
                    "group_id": int(
                        (resolved or {}).get("group_id") or tip.get("group_id") or 0
                    ),
                    "tip_rarity": int(
                        (resolved or {}).get("tip_rarity") or tip.get("rarity") or 0
                    ),
                    "hint_level": int(
                        (resolved or {}).get("hint_level") or tip.get("level") or 0
                    ),
                    "candidate_skill_ids": (resolved or {}).get("candidate_skill_ids")
                    or [],
                    "name": (resolved or {}).get("resolved_name") or "",
                    "cost": cost,
                    "affordable": cost <= points,
                    "owned_group": (resolved or {}).get("skip_reason") == "owned_group",
                    "known": bool((resolved or {}).get("master_exists")),
                    "failed_scope": (resolved or {}).get("failed_scope"),
                    "selected": selected_flag,
                    "resolution_reason": (resolved or {}).get("resolution_reason")
                    or "",
                    "skip_reason": skip_reason,
                }
            )
        return result

    def _debug_owned_skills(self, state):
        chara = (state.get("data") or {}).get("chara_info") or {}
        result = []
        for row in chara.get("skill_array") or []:
            skill_id = int(row.get("skill_id") or 0)
            result.append(
                {
                    "skill_id": skill_id,
                    "group_id": self.skill_buyer.skill_to_group_id.get(
                        skill_id, skill_id // 10
                    ),
                    "name": self.skill_buyer.skill_names.get(skill_id, ""),
                }
            )
        return result

    def _debug_inventory(self, state):
        free = (state.get("data") or {}).get("free_data_set") or {}
        result = []
        for name, count in sorted(self.item_manager._owned_map(free).items()):
            item_id = DISPLAY_TO_ID.get(name)
            if not item_id:
                continue
            result.append(
                {
                    "name": name,
                    "item_id": item_id,
                    "current_num": int(count),
                    "failed_scope": (
                        "this_turn"
                        if item_id in self.item_manager.failed_use_this_turn
                        else None
                    ),
                }
            )
        return result

    def _debug_item_buy_options(self, state, preset):
        data = state.get("data") or {}
        free = data.get("free_data_set") or {}
        current_turn = int((data.get("chara_info") or {}).get("turn") or 0)
        coin_val = free.get("coin_num")
        if coin_val is None:
            coin_val = free.get("gained_coin_num")
        budget = int(coin_val or 0)
        owned = self.item_manager._owned_map(free)
        result = []
        for row in free.get("pick_up_item_info_array") or []:
            shop_item_id = int(row.get("shop_item_id") or 0)
            item_id = int(row.get("item_id") or 0)
            name = ITEM_NAMES.get(item_id)
            if not name:
                continue
            limit_turn = int(row.get("limit_turn") or 0)
            cost = int(row.get("coin_num") or 0)
            original_cost = int(row.get("original_coin_num") or cost)
            bought = int(row.get("item_buy_num") or 0)
            limit = int(row.get("limit_buy_count") or 1)
            expired = limit_turn > 0 and current_turn > limit_turn
            rejected = shop_item_id in self.item_manager.failed_exchange_this_snapshot
            skip_reason = None
            if expired:
                skip_reason = "expired"
            elif bought >= limit:
                skip_reason = "limit_reached"
            elif rejected:
                skip_reason = "rejected"
            elif cost > budget:
                skip_reason = "unaffordable"
            result.append(
                {
                    "shop_item_id": shop_item_id,
                    "item_id": item_id,
                    "name": name,
                    "cost": cost,
                    "original_cost": original_cost,
                    "mant_coin": budget,
                    "affordable": cost <= budget,
                    "current_num": bought,
                    "limit": limit,
                    "absolute_limit_turn": limit_turn,
                    "server_turn_delta": (
                        (limit_turn - current_turn) if limit_turn > 0 else None
                    ),
                    "ui_turns_left": None,
                    "limit_reached": bought >= limit,
                    "expired": expired,
                    "rejected": rejected,
                    "skip_buy": False,
                    "selected": False,
                    "skip_reason": skip_reason,
                }
            )
        cfg = self.item_manager._mant_cfg(preset)
        tiers = cfg.get("item_tiers") or {}
        tier_count = int(cfg.get("tier_count") or 8)
        remaining_budget = budget
        simulated_owned = dict(owned)
        for tier in range(1, tier_count + 1):
            tier_rows = [
                row
                for row in result
                if row.get("skip_reason") is None
                and not row.get("selected")
                and int(tiers.get(display_to_slug(row.get("name")), 999)) == tier
            ]
            tier_rows.sort(
                key=lambda row: (
                    int(row.get("absolute_limit_turn") or 99),
                    int(row.get("cost") or 9999),
                )
            )
            for row in tier_rows:
                name = row.get("name")

                # Dynamically check if item cap is reached with simulated purchases
                if self.item_manager._skip_buy(name, simulated_owned, preset):
                    row["skip_reason"] = "skip_buy"
                    row["skip_buy"] = True
                    continue

                cost = int(row.get("cost") or 0)
                remaining = remaining_budget - cost
                if remaining < 0:
                    row["skip_reason"] = "unaffordable"
                    continue
                threshold = 0
                thresholds = cfg.get("tier_thresholds") or {}
                if tier > 1 and current_turn <= 64:
                    threshold = int(
                        thresholds.get(str(tier), thresholds.get(tier, (tier - 1) * 50))
                        or 0
                    )
                if threshold > 0 and remaining < threshold:
                    row["skip_reason"] = "rule_rejected"
                    continue
                row["selected"] = True
                simulated_owned[name] = simulated_owned.get(name, 0) + 1
                remaining_budget = remaining
        return result

    def _api_result(self, result):
        result = dict(result or {})
        error = str(result.get("error") or "")
        code = None
        for token in error.replace(":", " ").replace(",", " ").split():
            if token.isdigit():
                value = int(token)
                if value in {201, 202, 205, 208, 394, 709}:
                    code = value
                    break
        if result.get("result") == "ok":
            code = 1
        return {
            "ok": result.get("result") == "ok",
            "result_code": code,
            "error": error or None,
        }

    def _sum_cost(self, rows):
        return sum(int((row or {}).get("cost") or 0) for row in rows or [])

    def _shop_attempt_cost(self, attempt, selected):
        costs = {
            int(row.get("shop_item_id") or 0): int(row.get("cost") or 0)
            for row in selected or []
        }
        return sum(
            costs.get(int(row.get("shop_item_id") or 0), 0) for row in attempt or []
        )

    def _fresh_career_state(self, client, strategy=None, preset=None):
        errors = []
        max_retries = 8
        for attempt in range(max_retries):
            try:
                if hasattr(client, "load_career"):
                    state = client.load_career()
                else:
                    api_prefix = getattr(client, "api_prefix", "single_mode_free")
                    state = client.call(f"{api_prefix}/load", {})
                if strategy and (state.get("data") or {}).get("unchecked_event_array"):
                    state = self._drain_events(client, strategy, state, preset)
                self.skill_buyer.reset_scoped_failures()
                self.item_manager.reset_scoped_failures()
                return state
            except Exception as exc:
                err_str = str(exc)
                # A user Stop surfaces as "Bot stopped by user" from the
                # wrapped client - bail immediately instead of burning ~70s of
                # 10s-sleep retries while the operator waits for shutdown.
                if self._should_stop() or "Bot stopped by user" in err_str:
                    raise
                errors.append(err_str)
                log.warning(
                    "career state reload failed (attempt %s/%s): %s",
                    attempt + 1,
                    max_retries,
                    err_str,
                )
                if attempt < max_retries - 1:
                    time.sleep(10)
        if hasattr(client, "hard_reset"):
            log.warning("career state reload exhausted, falling back to hard_reset")
            return client.hard_reset()
        raise RuntimeError("career recovery failed: " + " | ".join(errors[-2:]))

    def _event(self, client, strategy, payload, state=None, preset=None):
        if self._should_stop():
            return {}
        data = dict(payload)
        event = data.pop("_event", None)
        current_turn = data.pop("_current_turn", 0)
        if event:
            reward_data = None
            # --- START get_choice_reward SIMPLE DEBUG ---
            try:
                choices = (event.get("event_contents_info") or {}).get(
                    "choice_array"
                ) or []
                if len(choices) > 1:
                    reward_res = client.get_choice_reward(data["event_id"])
                    reward_data = reward_res.get("data", {}).get(
                        "choice_reward_array", []
                    )

                    story_id = str(event.get("story_id", ""))
                    jp_name = self.event_names.get(story_id, "Unknown Event")

                    log.debug(f"[REWARD] Event: [{jp_name}] (Story ID: {story_id})")

                    aggregated_choices = {}
                    for reward in reward_data:
                        s_idx = reward.get("select_index", 0)
                        if s_idx not in aggregated_choices:
                            aggregated_choices[s_idx] = {}

                        for param in reward.get("gain_param_array") or []:
                            d_id = param.get("display_id")
                            v0 = param.get("effect_value_0", 0)
                            v1 = param.get("effect_value_1", 0)
                            val_str = f"+{v1}" if v1 > 0 else str(v1)

                            group_key = f"{d_id}_{v0}"
                            fmt_str = ""

                            if d_id in (1, 2):
                                op_str = f"+{abs(v1)}" if d_id == 1 else f"-{abs(v1)}"
                                if v0 == 1:
                                    fmt_str = f"Speed {op_str}"
                                elif v0 == 2:
                                    fmt_str = f"Stamina {op_str}"
                                elif v0 == 3:
                                    fmt_str = f"Power {op_str}"
                                elif v0 == 4:
                                    fmt_str = f"Guts {op_str}"
                                elif v0 == 5:
                                    fmt_str = f"Wiz {op_str}"
                                elif v0 == 10:
                                    fmt_str = f"Energy/HP {op_str}"
                                elif v0 == 20:
                                    fmt_str = f"Motivation {op_str}"
                                elif v0 == 30:
                                    fmt_str = f"Skill Points {op_str}"
                                elif v0 == 51:
                                    fmt_str = f"Speed Cap {op_str}"
                                elif v0 == 52:
                                    fmt_str = f"Stamina Cap {op_str}"
                                elif v0 == 53:
                                    fmt_str = f"Power Cap {op_str}"
                                elif v0 == 54:
                                    fmt_str = f"Guts Cap {op_str}"
                                elif v0 == 55:
                                    fmt_str = f"Wiz Cap {op_str}"
                                elif v0 == 11:
                                    fmt_str = f"Random Stat(s) Cap {op_str}"
                                else:
                                    fmt_str = f"Stat Type {v0} {op_str}"
                            elif d_id == 3:
                                fmt_str = f"Fans +{v0}"
                            elif d_id in (4, 5):
                                char_name = self.chara_names.get(
                                    str(v0), f"Char/Support ID: {v0}"
                                )
                                fmt_str = f"Bond {val_str} ({char_name})"
                            elif d_id == 6:
                                skill_name = self._get_skill_name(v0)
                                fmt_str = f"Skill Hint Lv {v1} ({skill_name})"
                            elif d_id == 8:
                                fmt_str = f"Skill Points (Variable)"
                            elif d_id == 9:
                                cond_name = CONDITION_NAMES.get(v0, f"Target: {v0}")
                                fmt_str = f"Gain Good Condition ({cond_name})"
                            elif d_id == 10:
                                cond_name = CONDITION_NAMES.get(v0, f"Target: {v0}")
                                fmt_str = f"Cures Condition ({cond_name})"
                            elif d_id == 11:
                                fmt_str = f"Unlock Recreation (Char ID: {v0})"
                            elif d_id == 37:
                                cond_name = CONDITION_NAMES.get(v0, f"Target: {v0}")
                                fmt_str = (
                                    f"Gain Bad Condition ({cond_name}) (10% Chance)"
                                )
                            elif d_id == 12:
                                fmt_str = f"Event chain ended"
                                group_key = "12_chain"
                            elif d_id == 13:
                                fmt_str = f"All Stats +{v0}"
                                group_key = "13_all"
                            elif d_id == 14:
                                fmt_str = f"Random Stat(s) +{v1} (Count: {v0})"
                                group_key = "14_rand"
                            elif d_id == 15:
                                fmt_str = f"Cures all bad conditions"
                                group_key = "15_cure"
                            elif d_id == 16:
                                fmt_str = (
                                    f"Randomly Cures Bad Condition(s) (Count: {v0})"
                                )
                                group_key = "16_rand_cure"
                            elif d_id == 19:
                                fmt_str = f"Last Trained Stat +{v0} & Cure Condition (10% Chance)"
                                group_key = "19_last_stat"
                            elif d_id == 20:
                                fmt_str = f"Last Trained Stat -{v0} (10% Chance)"
                                group_key = "20_last_stat"
                            elif d_id == 21:
                                fmt_str = f"Random Stat Increase (Variable)"
                                group_key = "21_rand"
                            elif d_id == 22:
                                fmt_str = f"All Stats Increase (Variable)"
                                group_key = "22_all"
                            elif d_id == 34:
                                fmt_str = (
                                    f"Random Stat(s) -{v1} (Count: {v0}) (10% Chance)"
                                )
                                group_key = "34_rand_dec"
                            elif d_id == 35:
                                char_name = self.chara_names.get(
                                    str(v0), f"Char/Support ID: {v0}"
                                )
                                fmt_str = f"Bond -{v1} ({char_name})"
                            else:
                                fmt_str = (
                                    f"Unknown (display_id: {d_id}, v0: {v0}, v1: {v1})"
                                )
                                group_key = f"{d_id}_{v0}_{v1}"

                            if group_key not in aggregated_choices[s_idx]:
                                aggregated_choices[s_idx][group_key] = []
                            if fmt_str not in aggregated_choices[s_idx][group_key]:
                                aggregated_choices[s_idx][group_key].append(fmt_str)

                    for s_idx in sorted(aggregated_choices.keys()):
                        log.debug(f"[REWARD] Choice {s_idx}:")
                        for group_key, fmt_strs in aggregated_choices[s_idx].items():
                            log.debug(f"[REWARD]   -> {' or '.join(fmt_strs)}")
            except Exception as e:
                log.warning(f"get_choice_reward failed: {e}")
            # --- END get_choice_reward SIMPLE DEBUG ---

            choice = strategy.choose_from_event(
                event, current_turn, reward_data, state, preset
            )
            self._log(
                "event_choice", current_turn, f"{data.get('event_id')} -> {choice}"
            )
            # Structured row so the AI learns the choice that was actually
            # MADE (the old extractor mis-read the next event's offered list).
            try:
                self._debug("event_choice_made", data={
                    "turn": int(current_turn or 0),
                    "event_id": data.get("event_id"),
                    "story_id": event.get("story_id"),
                    "choice_number": choice,
                })
            except Exception:
                pass
            return client.check_event(
                event_id=data["event_id"],
                chara_id=event.get("chara_id", 0),
                choice_number=choice,
                current_turn=current_turn,
            )
        if "event_id" not in data:
            self._log(
                "recover",
                current_turn,
                "event requested without event_id, forcing state refresh",
            )
            return self._fresh_career_state(client, strategy, preset)
        return client.check_event(**data)

    def _drain_events(self, client, strategy, state, preset=None, limit=20):
        current = state
        for _ in range(limit):
            if self._should_stop():
                break
            data = current.get("data") or {}
            events = data.get("unchecked_event_array") or []
            if not events:
                return current
            event = events[0] or {}
            choice = strategy._choice(event)
            chara_turn = (data.get("chara_info") or {}).get("turn")
            turn = chara_turn if chara_turn is not None else self.status.get("turn")
            turn = turn if turn is not None else 1
            payload = {
                "event_id": event.get("event_id"),
                "chara_id": event.get("chara_id", 0),
                "choice_number": choice,
                "current_turn": turn,
            }
            if choice is not None:
                try:
                    self._debug("event_choice_made", data={
                        "turn": int(turn or 0),
                        "event_id": event.get("event_id"),
                        "story_id": event.get("story_id"),
                        "choice_number": choice,
                    })
                except Exception:
                    pass
            if choice is None:
                payload = {
                    "event_id": event.get("event_id"),
                    "_event": event,
                    "_current_turn": turn,
                }
            current = self._event(client, strategy, payload, current, preset)
        return current

    def _buy_daily_clock(self, client, current_turn=0):
        """Buy the daily-shop alarm clock with carats (item_exchange 10220:
        1 clock for 2,000 carats, once per game day - verified from master
        data and a live capture of the real client buying it). Returns True
        when a clock was bought."""
        CLOCK_EXCHANGE_ID = 10220
        CLOCK_PRICE = 2000
        CARAT_ITEM_ID = 59
        try:
            money = int((getattr(client, "item_map", None) or {}).get(CARAT_ITEM_ID, 0) or 0)
            if money < CLOCK_PRICE:
                self._log(
                    "race_clock_buy",
                    current_turn,
                    f"skipped - {money:,} carats on hand, the daily clock costs {CLOCK_PRICE:,}",
                )
                return False
            show = client.item_show_exchange()
            servertime = (show.get("data_headers") or {}).get("servertime")
            from career_bot.dailies import _fmt_get_list_time
            buy = [{"exchange_id": CLOCK_EXCHANGE_ID, "count": 1, "ex_param": {"open_count": 1}}]
            use_item = [{"item_id": CARAT_ITEM_ID, "number": money}]
            client.item_exchange_multi(buy, use_item, _fmt_get_list_time(servertime))
            self._log(
                "race_clock_buy",
                current_turn,
                f"bought 1 alarm clock for {CLOCK_PRICE:,} carats (daily shop)",
            )
            return True
        except Exception as e:
            # Most common rejection: today's clock was already bought.
            self._log(
                "race_clock_buy",
                current_turn,
                f"purchase failed - probably already bought today ({e})",
            )
            return False

    def _get_clocks_left(self, root, max_clocks=5):
        data = root.get("data") or {}

        home_info = data.get("home_info")
        if isinstance(home_info, dict) and "available_continue_num" in home_info:
            std = int(home_info.get("available_continue_num", 0))
            free = int(home_info.get("available_free_continue_num", 0))
            continue_type = 1 if free > 0 else 2
            return {
                "source": "data.home_info.available_continue_num",
                "clocks_left": std + free,
                "continue_type": continue_type,
            }

        race_start_info = data.get("race_start_info")
        if isinstance(race_start_info, dict) and "continue_num" in race_start_info:
            used = int(race_start_info["continue_num"])
            return {
                "source": "data.race_start_info.continue_num",
                "clocks_used": used,
                "clocks_left": max_clocks - used,
                "continue_type": 2,
            }

        return {"source": "unknown", "clocks_left": 0, "continue_type": 2}

    def _parse_race_rank(self, res):
        import struct

        data = res.get("data", {})
        headers = res.get("data_headers", {})
        viewer_id = int(headers.get("viewer_id") or 0)

        race_start_info = data.get("race_start_info", {})
        horses = race_start_info.get("race_horse_data", [])

        player = next(
            (
                horse
                for horse in horses
                if int(horse.get("viewer_id") or 0) == viewer_id
            ),
            None,
        )
        if not player:
            return 99

        frame_order = player.get("frame_order")
        if not frame_order:
            return 99

        result_index = frame_order - 1

        scenario_b64 = data.get("race_scenario")
        if not scenario_b64:
            return 99

        try:
            blob = gzip.decompress(base64.b64decode(scenario_b64))
        except Exception:
            return 99

        offset = 0

        if len(blob) < offset + 4:
            return 99
        header_len = struct.unpack_from("<i", blob, offset)[0]
        offset += 4 + header_len

        if len(blob) < offset + 16:
            return 99
        distance_diff_max, horse_num, horse_frame_size, horse_result_size = (
            struct.unpack_from("<fiii", blob, offset)
        )
        offset += 16

        if len(blob) < offset + 4:
            return 99
        pad_len = struct.unpack_from("<i", blob, offset)[0]
        offset += 4 + pad_len

        if len(blob) < offset + 8:
            return 99
        frame_count, frame_size = struct.unpack_from("<ii", blob, offset)
        offset += 8 + frame_count * frame_size

        if len(blob) < offset + 4:
            return 99
        pad_len = struct.unpack_from("<i", blob, offset)[0]
        offset += 4 + pad_len

        if not (0 <= result_index < horse_num):
            return 99

        if len(blob) < offset + (result_index + 1) * horse_result_size:
            return 99

        finish_order = struct.unpack_from(
            "<i", blob, offset + result_index * horse_result_size
        )[0]

        return finish_order + 1

    def _build_unity_team(self, team_data_set, chara_info, preset):
        """Build the team_data_array roster for Unity Cup team_edit.

        Structure confirmed from captures: 5 divisions (distance_type 1-5,
        sprint/mile/medium/long/dirt), max 3 slots per division, member_id is
        the slot number WITHIN the division (1..3). The roster fields the
        trainee (from chara_info.card_id) plus every joined team member
        (evaluation_info_array entries with member_state == 1; state 0 rows
        are scenario NPCs / not yet scouted).

        Members are placed into the 5 distance divisions by their APTITUDES
        (best single fit picks a slot first, max 3 per division) and each gets
        the running style it is most apt for, instead of round-robin + one fixed
        style — so members no longer race a distance/style they have no aptitude
        for. Falls back to the old behaviour for anyone with no aptitude data.
        """
        cfg = (preset or {}).get("unity_config") or {}
        default_style = int(cfg.get("default_running_style", 1))
        apt = _load_card_aptitudes(self.base_dir)

        # (aptitude_key, roster_chara_id): the trainee gives a full card_id for
        # an exact aptitude lookup and races as card_id//100; joined members give
        # a bare chara_id (best aptitude across that character's cards).
        entries = []
        card_id = int((chara_info or {}).get("card_id") or 0)
        if card_id:
            entries.append((str(card_id), card_id // 100))
        for member in (team_data_set or {}).get("evaluation_info_array") or []:
            cid = member.get("chara_id")
            if cid and int(member.get("member_state") or 0) == 1:
                entries.append((str(int(cid)), int(cid)))

        # aptitude vector: [turf,dirt,short,mile,middle,long,nige,senko,sashi,
        # oikomi], 1=G..8=S. Exact card_id key, else best across the character's
        # cards (keys that start with the bare chara_id).
        def apt_vec(key):
            v = apt.get(key)
            if isinstance(v, list) and len(v) >= 10:
                return v
            best = None
            for k, val in apt.items():
                if k.startswith(key) and isinstance(val, list) and len(val) >= 10:
                    best = val if best is None else [max(a, b) for a, b in zip(best, val)]
            return best

        DIV_APT = {1: 2, 2: 3, 3: 4, 4: 5, 5: 1}   # sprint/mile/medium/long/dirt
        STYLE_APT = {1: 6, 2: 7, 3: 8, 4: 9}       # nige/senko/sashi/oikomi

        # Rank each member's divisions by aptitude; the strongest single fit
        # claims a slot first so the best-suited runner gets its best division.
        scored = []
        for key, roster_cid in entries:
            v = apt_vec(key)
            if v:
                order = sorted((1, 2, 3, 4, 5), key=lambda d: -v[DIV_APT[d]])
                style = max((1, 2, 3, 4), key=lambda s: v[STYLE_APT[s]])
                top = v[DIV_APT[order[0]]]
            else:
                order = [1, 2, 3, 4, 5]
                style = default_style
                top = -1
            scored.append((top, roster_cid, order, style))
        scored.sort(key=lambda t: -t[0])

        roster = []
        slots = {d: 0 for d in (1, 2, 3, 4, 5)}
        for _, roster_cid, order, style in scored:
            division = next((d for d in order if slots[d] < 3), None)
            if division is None:
                break  # all 15 slots filled
            slots[division] += 1
            roster.append(
                {
                    "distance_type": division,
                    "member_id": slots[division],
                    "chara_id": roster_cid,
                    "running_style": int(style),
                }
            )
        return roster

    def _team_race(self, client, strategy, state, preset, payload):
        """Unity Cup (scenario_id 2) team-race sub-flow.

        playing_state 7 -> full sequence (confirmed from captures):
            team_edit -> opponent_list -> team_race_analyze
                      -> team_race_start -> team_race_end -> team_race_out
        playing_state 8 (race already started) -> team_race_end -> team_race_out
        playing_state 9 (race already ended)   -> team_race_out

        Optional steps (team_edit, analyze) are best-effort; required steps
        that fail trigger a fresh career state so the strategy re-evaluates.
        A stuck-guard aborts the run cleanly instead of looping forever if the
        same turn's team race keeps failing.
        """
        if self._should_stop():
            return state
        current_turn = payload.get("current_turn", 1)
        phase = payload.get("phase") or "full"

        # Stuck-guard: the strategy re-emits team_race while playing_state
        # stays 7/8/9, so repeated failures on one turn must abort, not spin.
        if getattr(self, "_team_race_turn", None) == current_turn:
            self._team_race_tries = getattr(self, "_team_race_tries", 0) + 1
        else:
            self._team_race_turn = current_turn
            self._team_race_tries = 1
        if self._team_race_tries > 3:
            raise RuntimeError(
                f"Unity team race stuck at turn {current_turn} "
                f"({self._team_race_tries - 1} attempts failed)"
            )

        data = state.get("data") or {}
        team = data.get("team_data_set") or {}
        chara = data.get("chara_info") or {}
        self._log("team_race", current_turn, f"Unity Cup team race ({phase})")

        def drain(res):
            if isinstance(res, dict) and (res.get("data") or {}).get(
                "unchecked_event_array"
            ):
                res = self._drain_events(client, strategy, res, preset)
            return res

        def optional(name, fn):
            try:
                return drain(fn())
            except Exception as e:  # noqa: BLE001 - optional steps never abort
                self._log(f"team_{name}_skipped", current_turn, f"{e}")
                return None

        def required(name, fn):
            try:
                return drain(fn())
            except Exception as e:
                self._log(f"team_{name}_failed", current_turn, f"{e}")
                return None

        if phase == "full":
            # 1. Roster edit (optional: server keeps the previous roster).
            roster = self._build_unity_team(team, chara, preset)
            if roster:
                res = optional(
                    "team_edit",
                    lambda: client.team_edit(roster, current_turn=current_turn),
                )
                if res:
                    state = res

            # 2. Opponent list -> exposes the team_race_set_id to run.
            res = required(
                "opponent_list",
                lambda: client.opponent_list(current_turn=current_turn),
            )
            race_set_id = None
            if res:
                state = res
                opponents = (
                    ((res.get("data") or {}).get("team_data_set") or {}).get(
                        "opponent_info_array"
                    )
                    or []
                )
                if opponents:
                    race_set_id = opponents[0].get("team_race_set_id")
            if race_set_id is None:
                self._log(
                    "team_race_aborted", current_turn, "no team_race_set_id"
                )
                return self._fresh_career_state(client, strategy, preset)

            # 3-4. Analyze (optional) + start (required).
            optional(
                "team_race_analyze",
                lambda: client.team_race_analyze(
                    race_set_id, current_turn=current_turn
                ),
            )
            res = required(
                "team_race_start",
                lambda: client.team_race_start(
                    race_set_id, current_turn=current_turn
                ),
            )
            if not res:
                return self._fresh_career_state(client, strategy, preset)
            state = res

        if phase in ("full", "end"):
            res = required(
                "team_race_end",
                lambda: client.team_race_end(current_turn=current_turn),
            )
            if res:
                state = res

        res = required(
            "team_race_out",
            lambda: client.team_race_out(current_turn=current_turn),
        )
        if res:
            state = res

        # team_race_out responses carry no home_info; the post-race event
        # drain usually restores it, but refresh explicitly if it is missing
        # so the next command evaluation sees the training menu.
        if not ((state.get("data") or {}).get("home_info") or {}).get(
            "command_info_array"
        ):
            state = self._fresh_career_state(client, strategy, preset)

        self._log("team_race_done", current_turn, "team race complete")
        return state

    def _race(self, client, state, preset, payload):
        if self._should_stop():
            return state
        if scenario_traits((preset or {}).get("scenario_id") or 4).uses_item_shop:
            self.item_manager.recover_after_use_error = False
            state, used = self.item_manager.handle_pre_race(
                client, state, preset, payload, self.status, self.race_planner
            )
            with self.lock:
                details = self.status.setdefault("turn_details", {}).setdefault(
                    str(payload.get("current_turn") or 1), {}
                )
                if self.item_manager.buy_attempt_events:
                    items_bought_list = details.setdefault("items_bought", [])
                    for event in self.item_manager.buy_attempt_events:
                        res = event.get("result") or {}
                        if self._api_result(res).get("ok"):
                            start_coin = res.get("start_mant_coin") or 0
                            end_coin = res.get("mant_coin") or 0
                            if start_coin > 0 or end_coin > 0:
                                items_bought_list.append(
                                    f"Coins: {start_coin} -> {end_coin}"
                                )
                            for item in event.get("attempt") or []:
                                name = item.get("name")
                                if name:
                                    items_bought_list.append(name)
            for event in self.item_manager.use_attempt_events:
                self._debug(
                    "items_use_attempt",
                    state,
                    {
                        "selected": event.get("selected") or [],
                        "attempt": event.get("attempt") or [],
                        "payload": event.get("payload") or [],
                        "result": self._api_result(event.get("result") or {}),
                    },
                )

                res = event.get("result") or {}
                if res.get("result") == "ok":
                    attempted = event.get("attempt") or []
                    if attempted:
                        used_names = []
                        for item in attempted:
                            item_id = item.get("item_id")
                            use_num = item.get("use_num", 1)
                            name = ITEM_NAMES.get(item_id, str(item_id))
                            if "Cleat Hammer" in name:
                                name = f"{name} (+Stats)"
                            elif "Glow Sticks" in name:
                                name = f"{name} (+Fans)"
                            for _ in range(use_num):
                                used_names.append(name)
                        prog_id = payload.get("program_id")
                        race_name = (
                            self.race_planner.label(prog_id)
                            if self.race_planner and prog_id
                            else "Unknown Race"
                        )
                        log.info(
                            f"--> BOT DECISION: Used [ {', '.join(used_names)} ] for race: {race_name}"
                        )
                        with self.lock:
                            details = self.status.setdefault(
                                "turn_details", {}
                            ).setdefault(str(payload.get("current_turn") or 1), {})
                            details.setdefault("races", []).append(
                                f"Used: {', '.join(used_names)}"
                            )

            if (
                self.item_manager.recover_after_use_error
                or self.item_manager.use_attempt_events
            ):
                if self._should_stop():
                    return state
                state = self._fresh_career_state(
                    client, payload.get("_strategy"), preset
                )
                self._debug_turn(state, preset)
                if self.item_manager.recover_after_use_error:
                    return state
            if used > 0:
                with self.lock:
                    self.status["items_used"] += used
                    self._log_locked(
                        "items_use",
                        payload.get("current_turn") or 1,
                        f"pre-race {used}",
                    )

        program_id = payload.get("program_id")
        current_turn = payload.get("current_turn") or 1
        strategy = payload.get("_strategy")

        extra_options = preset.get("extra_race_options", {}).get(str(program_id), {})

        if scenario_traits(
            (preset or {}).get("scenario_id") or 4
        ).has_rival_race_map:
            race_name = (
                self.race_planner.label(program_id)
                if self.race_planner and program_id
                else ""
            )
            if "Climax Race 1" in race_name:
                extra_options = preset.get("extra_race_options", {}).get(
                    "TS_CLIMAX_1", extra_options
                )
            elif "Climax Race 2" in race_name:
                extra_options = preset.get("extra_race_options", {}).get(
                    "TS_CLIMAX_2", extra_options
                )
            elif "Climax Race 3" in race_name:
                extra_options = preset.get("extra_race_options", {}).get(
                    "TS_CLIMAX_3", extra_options
                )

        tactics = preset.get("tactics") or [1, 1, 1]
        if current_turn <= 24:
            tactic = tactics[0]
        elif current_turn <= 48:
            tactic = tactics[1]
        else:
            tactic = tactics[2]

        tactic_actions = preset.get("tactic_actions")
        if isinstance(tactic_actions, list):
            for action in tactic_actions:
                if (
                    isinstance(action, dict)
                    and action.get("turn") == current_turn
                    and "tactic" in action
                ):
                    tactic = int(action["tactic"])

        pre_started_res = None
        try:
            if self._should_stop():
                return state
            entry = client.race_entry(
                program_id=program_id, current_turn=current_turn
            )
        except Exception as exc:
            log.error(f"Race Entry Error at turn {current_turn}: {exc}")
            if not any(err in str(exc) for err in ("205", "208")):
                raise
            entry = None
            if "205" in str(exc):
                # A 205 on entry usually means the server ALREADY holds this
                # entry (a previous attempt crashed between race_entry and
                # race_start) - starting the race directly resolves the desync
                # instead of rejecting a race the server is waiting on.
                try:
                    pre_started_res = client.race_start(
                        is_short=1, current_turn=current_turn
                    )
                    log.warning(
                        "race_entry 205 but race_start succeeded - resuming the "
                        "already-registered entry."
                    )
                    self._log("race_entry_recovered", current_turn, program_id)
                    entry = {"data": {}}
                except Exception:
                    pre_started_res = None
                    entry = None
            if entry is None:
                self.race_planner.reject(current_turn, program_id)
                self._log("race_reject", current_turn, program_id)

                if (
                    any(err in str(exc) for err in ("205", "208"))
                    and 12 < current_turn <= 24
                ):
                    race_info = self.race_planner.program.get(program_id) or {}
                    race_name = race_info.get("name", "")
                    if "Maiden" in race_name or "Make Debut" in race_name:
                        log.warning(
                            "Error 205/208 on Maiden race entry. Assuming NOT a Maiden anymore."
                        )
                        self.race_planner.assumed_maiden_active = False
                    else:
                        log.warning(
                            "Error 205/208 on regular race entry. Assuming Maiden status."
                        )
                        self.race_planner.assumed_maiden_active = True

                with self.lock:
                    err_count = self.status.get("consecutive_race_errors", 0) + 1
                    self.status["consecutive_race_errors"] = err_count

                if err_count >= 3:
                    raise RuntimeError(
                        f"Too many consecutive race entry errors ({err_count}). Stopping runner."
                    )

                # Only return fresh state to trigger a retry if we caught a known error
                return self._fresh_career_state(client, strategy, preset)

        with self.lock:
            self.status["consecutive_race_errors"] = 0

        self._log("race_entry", current_turn, program_id)
        race_name = (
            self.race_planner.label(program_id)
            if self.race_planner and program_id
            else f"ID:{program_id}"
        )
        # The server decides the effective running style; race_entry's echo
        # carries it. The only way to change it is the dedicated
        # change_running_style call, valid ONLY in the entry->start window
        # (capture-verified; the change then persists for the whole career).
        server_style = ((entry.get("data") or {}).get("chara_info") or {}).get(
            "race_running_style"
        )
        style_note = f"sent {tactic} / server {server_style if server_style else '?'}"
        if server_style and int(server_style) != int(tactic) and pre_started_res is None:
            try:
                client.change_running_style(
                    program_id=program_id,
                    running_style=tactic,
                    current_turn=current_turn,
                )
                style_note = f"changed {server_style} -> {tactic}"
                self._log("race_style", current_turn, f"style {server_style} -> {tactic}")
            except Exception as e:
                # Never abort the race over a style change - run with the
                # server's style and say so honestly.
                style_note = f"WANTED {tactic}, server kept {server_style} ({e})"
                log.warning(
                    f"change_running_style failed at turn {current_turn}: {e} - racing with style {server_style}"
                )
        log.info(f"--> BOT DECISION: Entered Race [{race_name}] (Tactic: {style_note})")
        with self.lock:
            details = self.status.setdefault("turn_details", {}).setdefault(
                str(current_turn), {}
            )
            details.setdefault("races", []).append(race_name)
        if strategy:
            entry_data = entry.get("data") or {}
            if entry_data.get("unchecked_event_array"):
                entry = self._drain_events(client, strategy, entry, preset)
        race_start_info = (entry.get("data") or {}).get("race_start_info") or {}
        is_short = 1

        # The entry-desync recovery above may have already started the race.
        res = pre_started_res or client.race_start(
            is_short=is_short, current_turn=current_turn
        )
        self._log("race_start", current_turn, f"short {is_short}")

        rank = self._parse_race_rank(res)
        self._log("race_rank", current_turn, f"rank {rank}")

        home_info = (state.get("data") or {}).get("home_info") or {}
        std_clocks = int(home_info.get("available_continue_num", 0))
        free_clocks = int(home_info.get("available_free_continue_num", 0))

        clocks_used_this_race = 0
        clock_limit = int(preset.get("clock_use_limit") or 99)

        clock_policy = str(preset.get("clock_policy") or "use")
        allow_retry = self.burn_clocks or extra_options.get("retry", False)
        # Only MANDATORY (Branch A) forced races auto-retry with clocks - a
        # Branch-B fan-goal race just needs to be finished (fans accrue at any
        # rank), so it must not override the user's burn_clocks setting.
        if self.race_planner and program_id == self.race_planner.forced_mandatory(state):
            allow_retry = True
        if clock_policy == "never" and allow_retry and rank > 1:
            # User chose to never spend clocks: every result stands, even on
            # mandatory races - the career is allowed to fail its goal.
            self._log(
                "race_clock",
                current_turn,
                f"rank {rank} accepted - clock policy is 'never use clocks'",
            )
            allow_retry = False
        clock_buy_attempted = False
        while (
            allow_retry
            and rank > 1
            and clocks_used_this_race < clock_limit
        ):
            if self._should_stop():
                break
            if std_clocks <= 0 and free_clocks <= 0:
                # Out of clocks: optionally buy the daily carat clock
                # (exchange 10220 - 1 per game day for 2,000 carats).
                if clock_policy != "buy" or clock_buy_attempted:
                    break
                clock_buy_attempted = True
                if not self._buy_daily_clock(client, current_turn):
                    break
                std_clocks += 1
            clocks_left = std_clocks + free_clocks
            continue_type = 1 if free_clocks > 0 else 2

            self._log(
                "race_clock",
                current_turn,
                f"rank {rank}, using clock ({clocks_left} left, type {continue_type})...",
            )
            try:
                cont_res = client.race_continue(
                    current_turn=current_turn, continue_type=continue_type
                )

                cont_data = cont_res.get("data") or {}
                new_home_info = cont_data.get("home_info")
                if isinstance(new_home_info, dict):
                    std_clocks = int(new_home_info.get("available_continue_num", 0))
                    free_clocks = int(
                        new_home_info.get("available_free_continue_num", 0)
                    )
                else:
                    if free_clocks > 0:
                        free_clocks -= 1
                    else:
                        std_clocks -= 1

                if strategy:
                    if cont_data.get("unchecked_event_array"):
                        self._drain_events(client, strategy, cont_res, preset)

                roll = random.gauss(0.166 + client.api_jitter, 0.05)
                time.sleep(max(0.1, min(0.45, roll)))
                res = client.race_start(
                    is_short=is_short, current_turn=current_turn
                )
                rank = self._parse_race_rank(res)
                self._log("race_rank_retry", current_turn, f"rank {rank} after clock")
                clocks_used_this_race += 1
                with self.lock:
                    self.status["clocks_used"] = (
                        int(self.status.get("clocks_used") or 0) + 1
                    )
            except Exception as e:
                if "205" in str(e):
                    # Server declined the continue - this result isn't retryable
                    # (not a lost goal race). Accept the rank and move on.
                    self._log(
                        "race_clock_failed",
                        current_turn,
                        f"clock not usable for this result (server declined) - accepting rank {rank}",
                    )
                else:
                    self._log("race_clock_failed", current_turn, str(e))
                break

        if strategy:
            res_data = res.get("data") or {}
            if res_data.get("unchecked_event_array"):
                res = self._drain_events(client, strategy, res, preset)

        if self._should_stop():
            return res

        out = res
        try:
            end_res = client.race_end(current_turn=current_turn)
            _reward_info = end_res.get("data", {}).get("race_reward_info", {}) or {}
            rank = _reward_info.get("result_rank", "?")
            self._log("race_end", current_turn, f"Rank {rank}")
            log.info(f"[RACE RESULT] Finished race with Rank: {rank}")
            # Structured row for the AI datasets (finish position + fans were
            # previously only human-readable log strings and never learned).
            try:
                self._debug("race_result", data={
                    "turn": int(current_turn or 0),
                    "rank": int(_reward_info.get("result_rank") or 0),
                    "fans_gained": int(_reward_info.get("gained_fans") or 0),
                    "program_id": int(program_id or 0),
                })
            except Exception:
                pass

            if scenario_traits(
                getattr(client, "scenario_id", 4)
            ).calls_race_reward:
                try:
                    if hasattr(client, "race_reward"):
                        reward_res = client.race_reward(current_turn=current_turn)
                        if strategy:
                            reward_data = reward_res.get("data") or {}
                            if reward_data.get("unchecked_event_array"):
                                reward_res = self._drain_events(client, strategy, reward_res, preset)
                except Exception as e:
                    if any(err in str(e) for err in ("102", "1503")):
                        self._log("race_reward_reconciled", current_turn, "server already done (102)")
                    else:
                        raise

            # --- MAIDEN RACE DETECTION ---
            try:
                r_val = int(rank)
                if r_val == 1:
                    if self.race_planner:
                        self.race_planner.assumed_maiden_active = False
                else:
                    if self.race_planner and (current_turn == 12 or getattr(self.race_planner, "assumed_maiden_active", False)):
                        self.race_planner.assumed_maiden_active = True
                        log.warning("[RACE RESULT] Failed Make Debut / Maiden! Forcing Maiden status for next turn.")
            except (ValueError, TypeError):
                pass
        except Exception as e:
            if any(err in str(e) for err in ("102", "1503")):
                self._log(
                    "race_end_reconciled", current_turn, "server already done (102)"
                )
            else:
                raise

        if self._should_stop():
            return out

        try:
            out_res = client.race_out(current_turn=current_turn)
            out = out_res
            if strategy:
                out_data = out.get("data") or {}
                if out_data.get("unchecked_event_array"):
                    out = self._drain_events(client, strategy, out, preset)
        except Exception as e:
            if any(err in str(e) for err in ("102", "1503")):
                self._log(
                    "race_out_reconciled", current_turn, "server already done (102)"
                )
            else:
                raise

        return out

    def _race_progress(self, client, payload):
        if self._should_stop():
            return payload
        current_turn = payload.get("current_turn") or 1
        chara = payload.get("chara_info") or {}
        playing_state = int(chara.get("playing_state") or 0)
        if playing_state not in {2, 3, 4, 5}:
            self._log("race_skip", current_turn, f"not in race (state={playing_state})")
            return payload

        if playing_state == 2:
            try:
                client.race_start(is_short=1, current_turn=current_turn)
                self._log("race_start", current_turn, "resume")
            except Exception as e:
                if any(err in str(e) for err in ("102", "1503")):
                    self._log("race_start_reconciled", current_turn, "already started")
                else:
                    self._log("race_start_error", current_turn, str(e))
                    raise

        if playing_state in {2, 3}:
            try:
                end_res = client.race_end(current_turn=current_turn)
                _reward_info = end_res.get("data", {}).get("race_reward_info", {}) or {}
                rank = _reward_info.get("result_rank", "?")
                self._log("race_end", current_turn, f"resume (Rank {rank})")
                log.info(f"[RACE RESULT] Finished race with Rank: {rank}")
                try:
                    self._debug("race_result", data={
                        "turn": int(current_turn or 0),
                        "rank": int(_reward_info.get("result_rank") or 0),
                        "fans_gained": int(_reward_info.get("gained_fans") or 0),
                    })
                except Exception:
                    pass

                if scenario_traits(
                    getattr(client, "scenario_id", 4)
                ).calls_race_reward_on_resume:
                    try:
                        if hasattr(client, "race_reward"):
                            client.race_reward(current_turn=current_turn)
                    except Exception as e:
                        if any(err in str(e) for err in ("102", "1503")):
                            self._log("race_reward_reconciled", current_turn, "server already done (102)")
                        else:
                            raise

                # --- MAIDEN RACE DETECTION ---
                try:
                    r_val = int(rank)
                    if r_val == 1:
                        if self.race_planner:
                            self.race_planner.assumed_maiden_active = False
                    else:
                        if self.race_planner and (current_turn == 12 or getattr(self.race_planner, "assumed_maiden_active", False)):
                            self.race_planner.assumed_maiden_active = True
                            log.warning("[RACE RESULT] Failed Make Debut / Maiden! Forcing Maiden status for next turn.")
                except (ValueError, TypeError):
                    pass
            except Exception as e:
                if any(err in str(e) for err in ("102", "1503")):
                    self._log("race_end_reconciled", current_turn, "already ended")
                else:
                    self._log("race_end_error", current_turn, str(e))
                    raise

        if playing_state in {2, 3, 4}:
            try:
                self._log("race_out", current_turn, "resume")
                return client.race_out(current_turn=current_turn)
            except Exception as e:
                if any(err in str(e) for err in ("102", "1503")):
                    self._log(
                        "race_out_reconciled", current_turn, "already ended (102)"
                    )
                    return self._fresh_career_state(client)
                raise

        return self._fresh_career_state(client)

    def _buy_skills(self, client, state, preset, force):
        if self._should_stop():
            return state
        state, bought = self.skill_buyer.buy(client, state, preset, force)

        for event in self.skill_buyer.attempt_events:
            self._debug(
                "skills_attempt",
                state,
                {
                    "selected": event.get("selected") or [],
                    "attempt": event.get("attempt") or [],
                    "selected_total_cost": self._sum_cost(event.get("selected") or []),
                    "attempt_total_cost": self._sum_cost(event.get("attempt") or []),
                    "payload": event.get("payload") or [],
                    "result": self._api_result(event.get("result") or {}),
                },
            )
        if self.skill_buyer.attempt_events or self.skill_buyer.recover_after_error:
            try:
                state = self._fresh_career_state(client, preset=preset)
                self._debug_turn(state, preset)
            except Exception as e:
                log.warning(f"Skill phase reload failure: {e}")
                pass
        if bought:
            turn = (state.get("data") or {}).get("chara_info", {}).get("turn", 0)
            with self.lock:
                self.status["skills_bought"] += bought
                self.status["last_action"] = f"skills {bought}"
                self._log_locked(
                    "skills",
                    turn,
                    bought,
                )
                details = self.status.setdefault("turn_details", {}).setdefault(
                    str(turn), {}
                )
                skills_learned = details.setdefault("skills_learned", [])
                for event in self.skill_buyer.attempt_events:
                    res = event.get("result") or {}
                    if self._api_result(res).get("ok"):
                        for item in event.get("attempt") or []:
                            skill_id = item.get("skill_id")
                            if skill_id:
                                name = self._get_skill_name(skill_id)
                                skills_learned.append(name)
        return state

    def _handle_items(self, client, state, preset, best_command):
        if self._should_stop():
            return state
        if not scenario_traits(
            (preset or {}).get("scenario_id") or 4
        ).uses_item_shop:
            return state
        self.item_manager.recover_after_exchange_error = False
        self.item_manager.recover_after_use_error = False
        state, bought, used = self.item_manager.handle(
            client, state, preset, best_command, self.status, self.race_planner
        )

        for event in self.item_manager.buy_attempt_events:
            self._debug(
                "items_buy_attempt",
                state,
                {
                    "selected": event.get("selected") or [],
                    "attempt": event.get("attempt") or [],
                    "selected_total_cost": self._sum_cost(event.get("selected") or []),
                    "attempt_total_cost": self._shop_attempt_cost(
                        event.get("attempt") or [], event.get("selected") or []
                    ),
                    "payload": event.get("payload") or [],
                    "result": self._api_result(event.get("result") or {}),
                },
            )
        for event in self.item_manager.use_attempt_events:
            self._debug(
                "items_use_attempt",
                state,
                {
                    "selected": event.get("selected") or [],
                    "attempt": event.get("attempt") or [],
                    "payload": event.get("payload") or [],
                    "result": self._api_result(event.get("result") or {}),
                },
            )

        turn = (state.get("data") or {}).get("chara_info", {}).get("turn")
        if not turn:
            turn = self.status.get("turn", 0)
        with self.lock:
            details = self.status.setdefault("turn_details", {}).setdefault(
                str(turn), {}
            )

            free_data = state.get("data", {}).get("free_data_set")
            if free_data is not None:
                catalog_names = []
                for row in free_data.get("pick_up_item_info_array") or []:
                    if int(row.get("item_buy_num") or 0) >= int(
                        row.get("limit_buy_count") or 1
                    ):
                        continue
                    limit_turn = int(row.get("limit_turn") or 0)
                    if limit_turn > 0 and limit_turn < turn:
                        continue
                    i_id = int(row.get("item_id") or 0)
                    from career_bot.items import ITEM_NAMES, SHOP_ITEM_COSTS

                    n = ITEM_NAMES.get(i_id, str(i_id))
                    c = int(row.get("coin_num") or SHOP_ITEM_COSTS.get(n, 9999))
                    catalog_names.append(f"{n}({c})")
                details["catalog"] = catalog_names

                items_held = []
                for row in free_data.get("user_item_info_array") or []:
                    i_id = int(row.get("item_id") or 0)
                    num = int(
                        row.get("num")
                        or row.get("current_num")
                        or row.get("item_num")
                        or 0
                    )
                    if num > 0:
                        from career_bot.items import ITEM_NAMES

                        name = ITEM_NAMES.get(i_id, str(i_id))
                        items_held.append(f"{name} x{num}")
                details["items_held"] = items_held
            else:
                prev_turn = str(turn - 1)
                if turn > 1 and prev_turn in self.status.setdefault("turn_details", {}):
                    prev_details = self.status["turn_details"][prev_turn]
                    details.setdefault("catalog", prev_details.get("catalog", []))
                    details.setdefault("items_held", prev_details.get("items_held", []))

        if bought or used:
            with self.lock:
                self.status["items_bought"] += bought
                self.status["items_used"] += used
                if bought:
                    self._log_locked("items_buy", turn, bought)
                if used:
                    self._log_locked("items_use", turn, used)

                details = self.status.setdefault("turn_details", {}).setdefault(
                    str(turn), {}
                )
                if bought:
                    items_bought_list = details.setdefault("items_bought", [])
                    for event in self.item_manager.buy_attempt_events:
                        res = event.get("result") or {}
                        if self._api_result(res).get("ok"):
                            start_coin = res.get("start_mant_coin") or 0
                            end_coin = res.get("mant_coin") or 0
                            if start_coin > 0 or end_coin > 0:
                                items_bought_list.append(
                                    f"Coins: {start_coin} -> {end_coin}"
                                )
                            for item in event.get("attempt") or []:
                                name = item.get("name")
                                if name:
                                    items_bought_list.append(name)

                if used:
                    items_used_list = details.setdefault("items_used", [])
                    for event in self.item_manager.use_attempt_events:
                        res = event.get("result") or {}
                        if self._api_result(res).get("ok"):
                            for item in event.get("attempt") or []:
                                name = item.get("name")
                                if name:
                                    count = item.get("use_num") or 1
                                    for _ in range(count):
                                        items_used_list.append(name)

        if (
            self.item_manager.recover_after_exchange_error
            or self.item_manager.recover_after_use_error
            or self.item_manager.buy_attempt_events
            or self.item_manager.use_attempt_events
        ):
            try:
                state = self._fresh_career_state(client, preset=preset)
                self._debug_turn(state, preset)
            except Exception as e:
                log.warning(f"Item phase reload failure: {e}")
                pass

        return state

    def _merge_state(self, old_state, new_state):
        if not old_state:
            return new_state
        merged = dict(old_state)
        merged["data"] = dict(old_state.get("data") or {})
        for k, v in (new_state.get("data") or {}).items():
            if (
                isinstance(v, dict)
                and k in merged["data"]
                and isinstance(merged["data"][k], dict)
            ):
                merged_sub = dict(merged["data"][k])
                for sub_k, sub_v in v.items():
                    if sub_v is not None:
                        merged_sub[sub_k] = sub_v
                merged["data"][k] = merged_sub
            else:
                merged["data"][k] = v
        return merged

    def _command_from_decision(self, state, decision):
        payload = decision.payload or {}
        command_type = int(payload.get("command_type") or 0)
        p_id = int(payload.get("command_id") or payload.get("command_group_id") or 0)

        for cmd in ((state.get("data") or {}).get("home_info") or {}).get(
            "command_info_array"
        ) or []:
            if int(cmd.get("command_type") or 0) != command_type:
                continue

            c_id = int(cmd.get("command_id") or cmd.get("command_group_id") or 0)
            if c_id == p_id:
                return cmd

        return payload

    def _track_turn_scores(self, state):
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        turn = int(chara.get("turn") or 0)
        home = data.get("home_info") or {}
        commands = home.get("command_info_array") or []
        max_score = 0
        has_training = False
        for cmd in commands:
            if int(cmd.get("command_type") or 0) == 1:
                has_training = True
                score = self.item_manager._command_stat_gain(cmd)
                if score > max_score:
                    max_score = score
        if has_training:
            with self.lock:
                dh = self.status.setdefault("date_history", [])
                sh = self.status.setdefault("score_history", [])
                if not dh or dh[-1] != turn:
                    dh.append(turn)
                    sh.append(max_score)
                    if len(dh) > 48:
                        dh.pop(0)
                        sh.pop(0)
