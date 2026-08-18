import json
from pathlib import Path

from career_bot.logging_utils import get_logger
from career_bot.scenarios import scenario_traits

_RACE_CACHE = {}

log = get_logger("races")


class RacePlanner:
    def __init__(self, base_dir):
        self.base_dir = Path(base_dir)
        self.meta = {}
        self.program = {}
        self.instance = {}
        self.route_races = {}
        self.rejected = set()
        self._ai_policy = {}
        self._ai_policy_mtime = 0.0
        self._load()

    def _refresh_ai_policy(self):
        """Reload policy_adjustments.json when it changed on disk (cheap stat).
        Produced by the AI trainer; only consulted when a preset opts in."""
        try:
            from career_bot.logging_utils import runtime_output_root

            path = runtime_output_root(self.base_dir) / "ai" / "policy_adjustments.json"
            if not path.exists():
                return
            mtime = path.stat().st_mtime
            if mtime <= self._ai_policy_mtime:
                return
            self._ai_policy = json.loads(path.read_text(encoding="utf-8")) or {}
            self._ai_policy_mtime = mtime
            log.info("AI race policy loaded (%d programs)", len(self._ai_policy))
        except Exception as e:
            log.warning("AI race policy load failed: %s", e)

    def _ai_rerank(self, pids):
        """Order candidate programs by learned Bayesian adjustment, confidence-
        weighted by sample count. Ties/unknown programs keep preset order."""
        if len(pids) < 2 or not self._ai_policy:
            return pids

        def _rank(pid):
            entry = self._ai_policy.get(str(pid)) or {}
            adj = float(entry.get("adjustment") or 0.0)
            starts = int(entry.get("starts") or 0)
            conf = min(1.0, starts / 20.0)
            return adj * conf

        ranked = sorted(pids, key=_rank, reverse=True)
        if ranked != pids:
            log.info("AI re-ranked wanted races: %s -> %s", pids, ranked)
        return ranked

    def _load(self):
        global _RACE_CACHE
        if _RACE_CACHE:
            self.meta = _RACE_CACHE["meta"]
            self.program = _RACE_CACHE["program"]
            self.instance = _RACE_CACHE["instance"]
            self.route_races = _RACE_CACHE.get("route_races", {})
            return

        path = self.base_dir / "data" / "race_map.json"
        if not path.exists():
            return
        data = json.loads(path.read_text(encoding="utf-8"))
        self.meta = {int(k): v for k, v in (data.get("meta") or {}).items()}
        self.program = {int(k): v for k, v in (data.get("program") or {}).items()}
        self.instance = {
            int(k): [int(item) for item in v]
            for k, v in (data.get("instance") or {}).items()
        }

        _RACE_CACHE["meta"] = self.meta
        _RACE_CACHE["program"] = self.program
        _RACE_CACHE["instance"] = self.instance

        route_path = self.base_dir / "data" / "single_mode_route_race.json"
        if route_path.exists():
            try:
                route_data = json.loads(route_path.read_text(encoding="utf-8"))
                for row in route_data:
                    route_id = int(row.get("id") or 0)
                    if route_id:
                        self.route_races[route_id] = {
                            "turn": int(row.get("turn") or 0),
                            "condition_id": int(row.get("condition_id") or 0),
                            "target_type": int(row.get("target_type") or 0),
                            "condition_type": int(row.get("condition_type") or 0),
                            "condition_value_1": int(row.get("condition_value_1") or 0),
                        }
            except Exception as e:
                log.warning("failed to load single_mode_route_race.json: %s", e)
        _RACE_CACHE["route_races"] = self.route_races

    def get_rival_race_map(self, state):
        rivals = (
            state.get("data", {})
            .get("free_data_set", {})
            .get("rival_race_info_array", [])
        )
        return {
            int(r["program_id"]): int(r["chara_id"])
            for r in rivals
            if "program_id" in r and "chara_id" in r
        }

    def wanted_programs(self, preset, turn=None):
        result = []
        seen = set()
        current_turn = int(turn or 0)
        for value in preset.get("extra_race_list") or []:
            try:
                race_id = int(value)
            except (TypeError, ValueError):
                continue

            pids = []
            if race_id in self.meta:
                info = self.meta[race_id]
                occurrence_turn = int(info.get("turn") or 0)
                if current_turn and occurrence_turn and occurrence_turn != current_turn:
                    continue
                pid = info.get("program_id")
                if pid:
                    pids.append(pid)
            elif race_id in self.program:
                pids.append(race_id)
            else:
                for program_id in self.instance.get(race_id, []):
                    pids.append(program_id)

            for pid in pids:
                if pid not in seen:
                    seen.add(pid)
                    result.append(pid)
        return result

    def available_programs(self, state):
        data = state.get("data") or {}
        rca = data.get("race_condition_array") or []
        available = set()
        for item in rca:
            pid = int(item.get("program_id") or 0)
            if pid:
                available.add(pid)
        return available

    # (target_type, condition_type) -> goal metric. Only CONFIRMED metrics are
    # forced. (1,3) = "reach N total fans by <turn>" (URA + shared-pool goals,
    # confirmed). MANT's own goals (1,7)/(1,8) and Unity finals (3,1) use
    # unconfirmed metrics and are deliberately absent so the bot never forces the
    # wrong race for them.
    _GOAL_METRICS = {(1, 3): "fans"}

    def upcoming_goal(self, state):
        """Nearest not-yet-passed, not-yet-met FORCEABLE goal (currently only fan
        goals), read from chara_info.route_race_id_array; or None.

        MANT (scenario_id 4) is excluded per the maintainer directive - its goals
        use a different, unconfirmed metric.
        """
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        if int(chara.get("scenario_id") or 0) == 4:
            return None
        turn = int(chara.get("turn") or 0)
        current = int(chara.get("fans") or 0)
        best = None
        for rid in chara.get("route_race_id_array") or []:
            info = self.route_races.get(int(rid))
            if not info:
                continue
            metric = self._GOAL_METRICS.get(
                (info.get("target_type"), info.get("condition_type"))
            )
            if metric != "fans":
                continue
            deadline = int(info.get("turn") or 0)
            required = int(info.get("condition_value_1") or 0)
            if deadline < turn or required <= 0 or current >= required:
                continue
            if best is None or deadline < best["deadline"]:
                best = {
                    "metric": metric,
                    "deadline": deadline,
                    "required": required,
                    "current": current,
                    "turns_left": deadline - turn,
                    "shortfall": required - current,
                }
        return best

    def goal_at_risk(self, state, preset):
        """The upcoming fan goal, but ONLY when we're about to miss it: its
        deadline is within the final goal_race_lead_turns turns and we're still
        short on fans. This is a last-ditch save, NOT proactive pacing - the bot
        trains and runs its normal planned races the whole career and only forces
        a race in these final turns. Forcing stops the instant the fan target is
        reached (upcoming_goal returns None once current >= required).
        """
        g = self.upcoming_goal(state)
        if not g:
            return None
        try:
            lead = int((preset or {}).get("goal_race_lead_turns", 3))
        except (TypeError, ValueError):
            lead = 3
        if g["turns_left"] <= lead:
            return g
        return None

    def _forced_goal_race(self, state, preset):
        """Branch B: race is optional this turn - force an available race ONLY if
        a fan goal is at risk, to farm fans before its deadline (so the game never
        trips playing_state 5 = goal failed -> early career end)."""
        preset = preset or {}
        if not preset.get("force_goal_races", True):
            return 0
        goal = self.goal_at_risk(state, preset)
        if not goal:
            return 0
        data = state.get("data") or {}
        chara = data.get("chara_info") or {}
        current_turn = int(chara.get("turn") or 0)
        available_pids = [
            int(item.get("program_id") or 0)
            for item in (data.get("race_condition_array") or [])
        ]
        # Over-race guard: if a wanted raceable program exists this turn, let
        # choose() run it instead of forcing a duplicate.
        if any(
            p in available_pids and (current_turn, p) not in self.rejected
            for p in self.wanted_programs(preset, current_turn)
        ):
            return 0
        # Maiden gate (mirrors choose()): while the trainee still needs a maiden
        # win (lost Make Debut, or the server 205-refused a regular entry), the
        # server refuses regular race entries - forcing one here would burn the
        # runner's 3-strike entry budget and abort the career. Restrict to
        # Maiden / Make Debut races; if none is runnable this turn, force
        # nothing and let choose()'s maiden logic own the turn.
        wins = data.get("win_saddle_id_array")
        if wins is None:
            wins = chara.get("win_saddle_id_array")
        assumed_maiden = getattr(self, "assumed_maiden_active", False)
        if assumed_maiden and isinstance(wins, list) and len(wins) > 0:
            assumed_maiden = False
        if assumed_maiden and 12 < current_turn <= 24:
            for pid in available_pids:
                if pid and (current_turn, pid) not in self.rejected:
                    race_name = (self.program.get(pid) or {}).get("name", "")
                    if ("Maiden" in race_name or "Make Debut" in race_name) and self.check_aptitude(chara, pid):
                        return pid
            return 0
        # Prefer an aptitude race the trainee can actually run well.
        for pid in available_pids:
            if (
                pid
                and (current_turn, pid) not in self.rejected
                and self.check_aptitude(chara, pid)
            ):
                return pid
        if not preset.get("goal_race_aptitude_only", True):
            for pid in available_pids:
                if pid and (current_turn, pid) not in self.rejected:
                    return pid
        return 0

    def forced_mandatory(self, state):
        """Branch A only: the mandatory route-race pid when racing is the SOLE
        enabled command this turn; 0 otherwise. Used by the runner's clock-retry
        gate so auto-retry-with-clocks stays scoped to must-win mandatory races
        and never extends to Branch-B fan-goal races (those obey burn_clocks /
        per-race retry options like any optional race - a fan goal only needs
        the race FINISHED, not won)."""
        data = state.get("data") or {}
        commands = ((data.get("home_info") or {}).get("command_info_array")) or []
        race_enabled = any(
            cmd.get("command_type") == 4
            and cmd.get("command_id") == 401
            and cmd.get("is_enable", 0)
            for cmd in commands
        )
        other_enabled = any(
            cmd.get("command_type") != 4 and cmd.get("is_enable", 0)
            for cmd in commands
        )
        if not race_enabled or other_enabled:
            return 0
        return self.forced_program(state)

    def forced_program(self, state, preset=None):
        data = state.get("data") or {}
        home = data.get("home_info") or {}
        commands = home.get("command_info_array") or []
        race_enabled = any(
            cmd.get("command_type") == 4
            and cmd.get("command_id") == 401
            and cmd.get("is_enable", 0)
            for cmd in commands
        )
        if not race_enabled:
            return 0
        other_enabled = any(
            cmd.get("command_type") != 4 and cmd.get("is_enable", 0) for cmd in commands
        )
        # Race is OPTIONAL this turn (other commands enabled): only force a race if
        # a fan goal is at risk (Branch B). Otherwise fall through to the mandatory
        # route-race handling below (Branch A - race is the only enabled command).
        if other_enabled:
            return self._forced_goal_race(state, preset)
            
        chara = data.get("chara_info") or {}
        current_turn = int(chara.get("turn") or 0)
        route_race_ids = chara.get("route_race_id_array") or []
        
        target_program_id = 0
        for rid in route_race_ids:
            route_info = self.route_races.get(int(rid))
            if route_info and route_info["turn"] == current_turn:
                target_program_id = route_info["condition_id"]
                break

        rca = data.get("race_condition_array") or []
        available_pids = [int(item.get("program_id") or 0) for item in rca]

        # Respect races the server already refused this run (entry 205/208) -
        # re-forcing one loops the runner into the crash guard.
        if (
            target_program_id
            and target_program_id in available_pids
            and (current_turn, target_program_id) not in self.rejected
        ):
            return target_program_id

        for pid in available_pids:
            if pid and (current_turn, pid) not in self.rejected:
                return pid
                
        race = data.get("race_start_info") or {}
        return int(race.get("program_id") or 0)

    def check_aptitude(self, chara, program_id):
        info = self.program.get(int(program_id or 0)) or {}
        ground = int(info.get("ground") or 1)
        distance = int(info.get("distance") or 1200)

        if ground == 2:
            g_apt = int(chara.get("proper_ground_dirt") or 1)
        else:
            g_apt = int(chara.get("proper_ground_turf") or 1)

        if distance <= 1400:
            d_apt = int(chara.get("proper_distance_short") or 1)
        elif distance <= 1800:
            d_apt = int(chara.get("proper_distance_mile") or 1)
        elif distance <= 2400:
            d_apt = int(chara.get("proper_distance_middle") or 1)
        else:
            d_apt = int(chara.get("proper_distance_long") or 1)

        return g_apt >= 6 and d_apt >= 6

    def choose(self, state, preset):
        data = state.get("data") or {}
        turn = int((data.get("chara_info") or {}).get("turn") or 0)

        home = data.get("home_info") or {}
        commands = home.get("command_info_array") or []
        race_enabled = any(
            cmd.get("command_type") == 4
            and cmd.get("command_id") == 401
            and cmd.get("is_enable", 0)
            for cmd in commands
        )
        if not race_enabled:
            return 0

        available = self.available_programs(state)
        if not available:
            return 0

        chara = data.get("chara_info") or {}

        # --- MAIDEN RACE FALLBACK LOGIC ---
        wins = data.get("win_saddle_id_array")
        if wins is None:
            wins = chara.get("win_saddle_id_array")

        assumed_maiden = getattr(self, "assumed_maiden_active", False)

        if assumed_maiden and isinstance(wins, list) and len(wins) > 0:
            self.assumed_maiden_active = False
            assumed_maiden = False

        is_maiden = False
        if 12 < turn <= 24:
            if assumed_maiden:
                is_maiden = True

        if is_maiden:
            for pid in available:
                if (turn, pid) not in self.rejected:
                    race_info = self.program.get(pid) or {}
                    race_name = race_info.get("name", "")
                    if "Maiden" in race_name or "Make Debut" in race_name:
                        if self.check_aptitude(chara, pid):
                            return pid
            # If we need a maiden win but no suitable race exists, do not enter preset races
            return 0
        # ----------------------------------

        wanted = self.wanted_programs(preset, turn)
        valid_wanted = [
            pid
            for pid in wanted
            if pid in available and (turn, pid) not in self.rejected
        ]

        # Optional AI race re-ranking (learned Bayesian win-rate order). OFF by
        # default; only active when the preset opts in AND a trained policy
        # exists. Forced/route/maiden races are handled elsewhere, so this only
        # reorders the user's own wanted list.
        if preset.get("use_ai_race_rerank"):
            self._refresh_ai_policy()
            valid_wanted = self._ai_rerank(valid_wanted)

        if not valid_wanted:
            fans = int(chara.get("fans") or 0)
            if fans < 350 and turn > 11:
                for pid in available:
                    if (turn, pid) not in self.rejected and self.check_aptitude(
                        chara, pid
                    ):
                        return pid
            return 0

        # Rival-overwrite race preference is a per-scenario trait (MANT).
        traits = scenario_traits(
            (data.get("chara_info") or {}).get("scenario_id")
        )
        if not traits.has_rival_race_map:
            return valid_wanted[0]

        main_race_id = valid_wanted[0]
        rival_map = self.get_rival_race_map(state)

        if main_race_id in rival_map:
            return main_race_id

        for overwrite_pid in valid_wanted[1:]:
            if overwrite_pid in rival_map:
                return overwrite_pid

        return main_race_id

    def reject(self, turn, program_id):
        self.rejected.add((int(turn or 0), int(program_id or 0)))

    def label(self, program_id):
        info = self.program.get(int(program_id or 0)) or {}
        name = info.get("name") or ""
        race_instance_id = info.get("race_instance_id") or ""
        return f"{program_id} {race_instance_id} {name}".strip()
