import json
from pathlib import Path

from career_bot.logging_utils import get_logger

log = get_logger("events")


class EventManager:
    def __init__(self, base_dir):
        self.base_dir = Path(base_dir)
        self.outcomes = {}
        self.event_names = {}
        self._load()

    def _load(self):
        path = self.base_dir / "data" / "event_outcomes.json"
        names_path = self.base_dir / "data" / "event_names.json"
        overrides_path = self.base_dir / "data" / "event_overrides.json"
        if not path.exists():
            return
        try:
            self.outcomes = json.loads(path.read_text(encoding="utf-8"))
        except Exception as e:
            log.error("failed to parse event_outcomes.json (events unmapped): %s", e)

        if names_path.exists():
            try:
                self.event_names = json.loads(names_path.read_text(encoding="utf-8"))
            except Exception as e:
                log.warning("failed to parse event_names.json: %s", e)

        # User-picked default choices from the Events page: {story_id: pos}
        # where pos is the 1-based option the user wants ALWAYS taken. Highest
        # priority - overrides the good/bad outcome table. Hot-reloaded here.
        self.overrides = {}
        if overrides_path.exists():
            try:
                raw = json.loads(overrides_path.read_text(encoding="utf-8"))
                if isinstance(raw, dict):
                    self.overrides = {str(k): int(v) for k, v in raw.items() if str(v).lstrip("-").isdigit()}
            except Exception as e:
                log.warning("failed to parse event_overrides.json: %s", e)

    @staticmethod
    def _answer_for(choice, fallback_position):
        """The wire value check_event expects as choice_number.

        Confirmed from packet captures (2026-07-08): the real client always
        sends the chosen choice's gain_select_id_index — NOT select_index.
        For normal events the two coincide (both are the 1..N position), so
        select_index worked by luck; but e.g. story 400002445 has five choices
        all with select_index=2 (gsii 1-5, client sent 5), and Happy Meek's
        Challenge (400001418) has select_index {1,3,3} with gsii {2,5,6} —
        answering with select_index there is invalid, the server re-serves the
        event, and the bot loops on the same turn ("stuck at turn 24")."""
        try:
            g = int(choice.get("gain_select_id_index") or 0)
            if g > 0:
                return g
        except (TypeError, ValueError):
            pass
        try:
            s = int(choice.get("select_index") or 0)
            if s > 0:
                return s
        except (TypeError, ValueError):
            pass
        return fallback_position

    def choose(self, event, reward_data=None, state=None, preset=None):
        # Hot-reload the JSON file so your mappings take effect immediately without restarting
        self._load()

        story_id = str(event.get("story_id", ""))
        choices = (event.get("event_contents_info") or {}).get("choice_array") or []
        jp_name = self.event_names.get(story_id, "Unknown Event Name")

        if not choices:
            return 0

        # 1) A user-picked default from the Events page wins outright.
        user_pos = getattr(self, "overrides", {}).get(story_id)
        if user_pos:
            idx = max(1, min(len(choices), int(user_pos))) - 1
            answer = self._answer_for(choices[idx], idx + 1)
            log.info(
                f"[EventManager] User-pick Event: [{jp_name}] | Choices: {len(choices)} "
                f"| Bot Chose: {idx + 1} (your Events-page default)"
            )
            return answer

        outcome_data = self.outcomes.get(story_id)

        if not outcome_data and len(story_id) >= 3:
            # Fallback for an unmapped story_id (e.g. a new outfit variant of a
            # known event): borrow a mapped event's good/bad table by matching
            # the trailing 3 digits. But 3-digit suffixes COLLIDE across
            # unrelated events (1367 events, only 1000 possible suffixes), so a
            # bare match can apply a foreign table and pick a harmful choice.
            # Only trust it when UNAMBIGUOUS: exactly one mapped event shares
            # this suffix AND its option indices fit this event's choice count.
            # Otherwise fall through to the reward-based dynamic scorer.
            suffix = story_id[-3:]
            n = len(choices)

            def _suffix_fits(v):
                oc = (v or {}).get("outcomes", {})
                if not oc:
                    return False
                idxs = [int(k) for k in oc if str(k).isdigit()]
                return max(idxs, default=0) <= n

            matches = [
                v
                for k, v in self.outcomes.items()
                if k.endswith(suffix) and _suffix_fits(v)
            ]
            if len(matches) == 1:
                outcome_data = matches[0]

        selected_choice = None
        selected_index = 0
        is_mapped = outcome_data is not None

        if outcome_data:
            outcomes = outcome_data.get("outcomes", {})
            for i, choice in enumerate(choices):
                visual_index = str(i + 1)
                select_index = str(choice.get("select_index", ""))

                # Check visual index first (GUI mapping style: 1, 2, 3...)
                if visual_index in outcomes:
                    if outcomes.get(visual_index) == "good":
                        selected_choice = self._answer_for(choice, i + 1)
                        selected_index = i
                        break
                # Fallback to internal select_index (Legacy mapping style)
                elif select_index in outcomes:
                    if outcomes.get(select_index) == "good":
                        selected_choice = self._answer_for(choice, i + 1)
                        selected_index = i
                        break

        if selected_choice is None:
            selected_choice = self._answer_for(choices[0], 1) if choices else 1

        event_name = jp_name
        if outcome_data and "event_name" in outcome_data:
            event_name = outcome_data.get("event_name", event_name)

        status = "Mapped" if is_mapped else "Unmapped"
        log.info(
            f"[EventManager] {status} Event: [{event_name}] | Choices: {len(choices)} | Bot Chose: {selected_index + 1}"
        )

        if not outcome_data:
            chara_id = event.get("chara_id", "unknown")
            event_id = event.get("event_id", "unknown")
            log.warning(
                f"[EventManager] Unmapped event encountered! [{jp_name}] chara_id: {chara_id} | event_id: {event_id} | story_id: {story_id} | choices: {len(choices)}"
            )
            self._log_unmapped(story_id, choices, event, jp_name, reward_data)

            if reward_data and len(choices) > 1:
                selected_choice = self._evaluate_dynamic_choice(
                    choices, reward_data, state, preset, chara_id, story_id
                )

                visual_idx = 1
                for i, choice in enumerate(choices):
                    if self._answer_for(choice, i + 1) == selected_choice:
                        visual_idx = i + 1
                        break

                log.info(
                    f"[EventManager] Dynamic choice evaluated to: Choice {visual_idx} (select_index: {selected_choice})"
                )
                return selected_choice

        return selected_choice

    def _evaluate_dynamic_choice(self, choices, reward_data, state, preset, chara_id, story_id):
        data = state.get("data", {}) if state else {}
        chara = data.get("chara_info", {})

        current_mood = int(chara.get("motivation", 3))
        current_energy = int(chara.get("vital", 100))
        max_energy = int(chara.get("max_vital", 100))
        energy_pct = (current_energy / max_energy * 100) if max_energy > 0 else 100

        current_stats = [
            int(chara.get("speed", 0)),
            int(chara.get("stamina", 0)),
            int(chara.get("power", 0)),
            int(chara.get("guts", 0)),
            int(chara.get("wiz", 0)),
        ]

        caps = [1200, 1200, 1200, 1200, 1200]
        if preset and "expect_attribute" in preset:
            caps = preset["expect_attribute"]

        # Stat-priority tilt: a choice that gains (or spares) the trainee's
        # higher-priority stats scores more. Reuses the v2 stat_priority list
        # when present; bounded [0.6, 1.6] so it tilts toward the build without
        # overriding the below-cap boosting further down.
        _stat_keys = ["speed", "stamina", "power", "guts", "wit"]
        _prio_list = ((preset or {}).get("scoring_v2") or {}).get(
            "stat_priority"
        ) or ["speed", "stamina", "power", "wit", "guts"]
        _rank = {}
        for _r, _name in enumerate(_prio_list):
            if _name in _stat_keys and _stat_keys.index(_name) not in _rank:
                _rank[_stat_keys.index(_name)] = _r
        stat_prio_weight = [
            round(1.6 - 0.25 * _rank.get(_i, _i), 3) for _i in range(5)
        ]

        deck_supports = []
        for sc in chara.get("support_card_array", []):
            try:
                deck_supports.append(int(sc.get("support_card_id", 0)))
            except (ValueError, TypeError):
                pass

        try:
            chara_id_int = int(chara_id)
        except (ValueError, TypeError):
            chara_id_int = 0

        reward_map = {}
        for r in reward_data:
            try:
                reward_map[int(r.get("select_index", 1))] = r
            except (ValueError, TypeError):
                pass

        chara_effect_ids = []
        for effect_id in chara.get("chara_effect_id_array", []):
            try:
                chara_effect_ids.append(int(effect_id))
            except (ValueError, TypeError):
                pass

        best_choice = 0
        best_select_index = int(choices[0].get("select_index", 1)) if choices else 1
        best_score = -999999

        # NOTE: Happy Meek's Challenge (story 400001418) used to have a bespoke
        # override here; it's unnecessary now that answers use gain_select_id_index
        # (_answer_for) and the loop below returns the winning choice's wire index
        # — a value the server always accepts. (The old block also scored with the
        # wrong param keys, so it was inert anyway.)
        for i, choice in enumerate(choices):
            answer = self._answer_for(choice, i + 1)
            try:
                select_index = int(choice.get("select_index", i + 1))
            except (ValueError, TypeError):
                select_index = i + 1

            # Rewards are keyed by the same wire index the answer uses; fall
            # back to select_index for older payloads.
            reward = reward_map.get(answer) or reward_map.get(select_index, {})
            score = 0
            has_slow_metabolism = False
            other_reward_score = 0

            for param in reward.get("gain_param_array", []):
                d_id = param.get("display_id")
                v0 = param.get("effect_value_0", 0)
                v1 = param.get("effect_value_1", 0)

                if d_id in (1, 2) and v0 == 20:
                    val = v1 if d_id == 1 else -v1
                    if val > 0:
                        score += val * (1000 if current_mood < 5 else 10)
                    else:
                        score += val * 1000
                elif d_id in (1, 2) and v0 == 10:
                    val = v1 if d_id == 1 else -v1
                    if val > 0:
                        score += val * (20 if energy_pct < 85 else 5)
                    else:
                        score += val * 30
                elif d_id in (1, 2) and v0 in (1, 2, 3, 4, 5):
                    val = v1 if d_id == 1 else -v1
                    idx = v0 - 1
                    curr = current_stats[idx]
                    cap = int(caps[idx]) if idx < len(caps) else 1200
                    ratio = min(1.0, curr / cap if cap > 0 else 1.0)
                    pw = stat_prio_weight[idx] if idx < 5 else 1.0
                    if val > 0:
                        stat_score = val * (1.0 - ratio) * 50 * pw
                        score += stat_score
                        other_reward_score += stat_score
                    else:
                        # Losing a high-priority stat hurts more.
                        score += val * 50 * pw
                elif d_id in (1, 2) and v0 == 30:
                    val = v1 if d_id == 1 else -v1
                    score += val * 1.0
                    if val > 0:
                        other_reward_score += val * 1.0
                elif d_id == 8:
                    score += 20
                    other_reward_score += 20
                elif d_id == 6:
                    score += 150 + (v1 * 50)
                    other_reward_score += 150
                elif d_id == 9:
                    score += 800
                elif d_id == 10:
                    if v0 in chara_effect_ids:
                        score += 1500
                    else:
                        score += 500
                elif d_id == 11:
                    score += 1500
                elif d_id == 37:
                    # 10% chance of failure
                    if v0 == 4:
                        has_slow_metabolism = True
                        score -= 50
                    else:
                        score -= 200
                elif d_id == 12:
                    if chara_id_int in deck_supports:
                        score -= 1500
                    else:
                        score -= 50
                elif d_id == 13:
                    score += v0 * 50 * 5
                    other_reward_score += v0 * 50 * 5
                elif d_id == 14:
                    score += v1 * 50 * v0
                    other_reward_score += v1 * 50 * v0
                elif d_id in (15, 16):
                    has_bad_cond = any(
                        cond in (1, 2, 3, 4, 5, 6) for cond in chara_effect_ids
                    )
                    score += 1500 if has_bad_cond else 200
                elif d_id == 19:
                    # 10% chance of success (Cure condition + stat)
                    has_bad_cond = any(
                        cond in (1, 2, 3, 4, 5, 6) for cond in chara_effect_ids
                    )
                    bonus = 1500 if has_bad_cond else 0
                    score += (v0 * 50 + bonus) * 0.1
                    other_reward_score += (v0 * 50) * 0.1
                elif d_id == 20:
                    score -= v0 * 5
                elif d_id == 34:
                    score -= v1 * 5 * v0
                elif d_id == 3:
                    score += v0 * 0.05
                elif d_id == 4:
                    score += v1 * 10
                elif d_id == 5:
                    score += v1 * 5
                elif d_id == 35:
                    score -= v1 * 10
                elif d_id in (21, 22):
                    score += 100
                    other_reward_score += 100

            if has_slow_metabolism and other_reward_score > 250:
                score += 400

            if score > best_score:
                best_score = score
                best_choice = i
                best_select_index = select_index

        # Return the wire answer (gain_select_id_index) of the winning choice.
        return self._answer_for(choices[best_choice], best_choice + 1) if choices else 1

    def _log_unmapped(self, story_id, choices, event, jp_name="", reward_data=None):
        try:
            log_dir = self.base_dir / "unmapped_events"
            log_dir.mkdir(parents=True, exist_ok=True)
            choice_count = len(choices)
            chara_id = event.get("chara_id", "unknown")
            file_path = (
                log_dir
                / f"unmapped_chara_{chara_id}_story_{story_id}_choices_{choice_count}.json"
            )

            mapped_choices = []
            reward_map = {}
            if reward_data:
                for r in reward_data:
                    try:
                        reward_map[int(r.get("select_index", 1))] = r
                    except (ValueError, TypeError):
                        pass

            for i, choice in enumerate(choices):
                choice_copy = dict(choice)
                try:
                    select_index = int(choice.get("select_index", i + 1))
                except (ValueError, TypeError):
                    select_index = i + 1
                if select_index in reward_map:
                    choice_copy["reward_data"] = reward_map[select_index]
                mapped_choices.append(choice_copy)

            log_data = {
                "event_name": jp_name,
                "story_id": story_id,
                "event_id": event.get("event_id"),
                "chara_id": event.get("chara_id"),
                "choice_count": choice_count,
                "choices": mapped_choices,
                "raw_event_payload": event,
            }

            file_path.write_text(
                json.dumps(log_data, indent=2, ensure_ascii=False), encoding="utf-8"
            )
            log.info(f"[EventManager] Saved unmapped event payload to {file_path}")
        except Exception as e:
            log.error(f"[EventManager] Failed to save unmapped event log: {e}")
