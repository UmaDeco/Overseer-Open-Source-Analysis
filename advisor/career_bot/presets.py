import json
import re
from pathlib import Path

from career_bot.logging_utils import get_logger

log = get_logger("presets")

EXCLUDED_KEYS = {
    "facility_period_configs",
    "facility_ratios",
    # Secrets / instance settings that must never live in a shareable preset.
    "discord_webhook_url",
    "discord_webhook_enabled",
    "webhook_url",
    "steam_session_ticket",
    "sid",
    "viewer_id",
    "auth_key",
    "udid",
    "device_id",
    "steam_username",
    "steam_password",
}

RENAMES = {
    "race_list": "extra_race_list",
    "skill_priority_list": "learn_skill_list",
    "skill_blacklist": "learn_skill_blacklist",
    "blacklistedSkills": "learn_skill_blacklist",
    "extraWeight": "extra_weight",
    "scoreValue": "score_value",
    "baseScore": "base_score",
    "statValueMultiplier": "stat_value_multiplier",
    "witSpecialMultiplier": "wit_special_multiplier",
    "cureAsapConditions": "cure_asap_conditions",
}

MANT_SCENARIO_ID = 4


def slugify(value):
    text = re.sub(r"[^a-zA-Z0-9._ ()-]+", "", str(value or "").strip())
    text = re.sub(r"\s+", " ", text).strip()
    return text or "preset"


def split_csv(value):
    if isinstance(value, list):
        return value
    return [part.strip() for part in str(value or "").split(",") if part.strip()]


def normalize_skill_list(value):
    rows = value if isinstance(value, list) else []
    result = []
    for row in rows:
        if isinstance(row, list):
            parts = []
            for item in row:
                parts.extend(split_csv(item))
        else:
            parts = split_csv(row)
        if parts:
            result.append(parts)
    return result


def normalize_preset(raw):
    data = dict(raw or {})
    normalized = {}
    for key, value in data.items():
        if key in EXCLUDED_KEYS:
            continue
        normalized[RENAMES.get(key, key)] = value
    normalized["name"] = slugify(normalized.get("name") or data.get("name"))
    normalized["scenario_id"] = int(
        normalized.get("scenario_id") or data.get("scenario") or MANT_SCENARIO_ID
    )
    normalized.pop("scenario", None)
    normalized["learn_skill_list"] = normalize_skill_list(
        normalized.get("learn_skill_list")
    )
    blacklist = []
    blacklist.extend(split_csv(data.get("blacklistedSkills")))
    blacklist.extend(split_csv(data.get("skill_blacklist")))
    blacklist.extend(split_csv(data.get("learn_skill_blacklist")))
    normalized["learn_skill_blacklist"] = list(dict.fromkeys(blacklist))
    normalized["cure_asap_conditions"] = split_csv(
        normalized.get("cure_asap_conditions")
    )
    if not normalized["cure_asap_conditions"]:
        normalized["cure_asap_conditions"] = [
            "Migraine",
            "Night Owl",
            "Skin Outbreak",
            "Slacker",
            "Slow Metabolism",
            "(Practice poor isn't worth a turn to cure)",
        ]
    normalized.setdefault("expect_attribute", [9999, 9999, 9999, 9999, 9999])
    normalized.setdefault(
        "score_value",
        [
            [0.11, 0.1, 0.006, 0.09],
            [0.11, 0.1, 0.006, 0.09],
            [0.11, 0.1, 0.006, 0.09],
            [0.03, 0.05, 0.006, 0.09],
            [0, 0, 0.006, 0],
        ],
    )
    normalized.setdefault("base_score", [0, 0, 0, 0, 0])
    normalized.setdefault(
        "stat_value_multiplier", [0.01, 0.01, 0.01, 0.01, 0.01, 0.005]
    )
    normalized.setdefault("extra_weight", [[0, 0, 0, 0, 0]] * 4)
    normalized.setdefault(
        "npc_score_value",
        [
            [0.05, 0.05, 0.05],
            [0.05, 0.05, 0.05],
            [0.05, 0.05, 0.05],
            [0.03, 0.05, 0.05],
            [0, 0, 0.05],
        ],
    )
    normalized.setdefault("special_training", [0.095, 0.095, 0.095, 0.095, 0])
    normalized.setdefault("spirit_explosion", [[0.16, 0.16, 0.16, 0.06, 0.11]] * 5)
    normalized.setdefault("wit_special_multiplier", [1.57, 1.37])
    normalized.setdefault("compensate_failure", True)
    normalized.setdefault("summer_score_threshold", 0.34)
    normalized.setdefault("motivation_threshold_year1", 3)
    normalized.setdefault("motivation_threshold_year2", 4)
    normalized.setdefault("motivation_threshold_year3", 4)
    normalized.setdefault("prioritize_recreation", False)
    normalized.setdefault("pal_thresholds", [])
    normalized.setdefault("pal_friendship_score", [0.08, 0.057, 0.018])
    normalized.setdefault("pal_card_multiplier", 0.1)
    normalized.setdefault("rest_threshold", 48)
    normalized.setdefault("learn_skill_threshold", 888)
    normalized.setdefault("manual_purchase_at_end", False)
    normalized.setdefault("stop_on_turn", 0)
    # Last-ditch fan-goal racing: ONLY force a race in the final turns before a
    # fan goal (route-race condition_type 3) when we're about to miss it, so a
    # career doesn't fail the goal. Not proactive - the bot trains/plans normally
    # otherwise. Applies to every scenario EXCEPT MANT (excluded in races.py).
    normalized.setdefault("force_goal_races", True)          # master on/off
    normalized.setdefault("goal_race_lead_turns", 3)         # only force in the final N turns before the deadline
    normalized.setdefault("goal_race_aptitude_only", True)   # only force races the trainee is apt for
    normalized.setdefault("mant_config", {})
    normalized.setdefault(
        "unity_config",
        {
            "unity_training_weight": 0.6,
            "spirit_burst_weight": 5.0,
            "default_running_style": 1,
        },
    )
    normalized.setdefault(
        "grand_live_config",
        {
            "performance_training_weight": 0.6,  # per performance gain on a facility
            "star_burst_weight": 5.0,            # high-value special performance payoff
        },
    )

    # Training scorer v2 (see career_bot/scoring.py) is the only engine; the old
    # "legacy" engine has been removed. Deep-merge so presets override single keys.
    normalized.pop("scoring_engine", None)  # retired toggle; safe to drop
    normalized.setdefault("use_ai_race_rerank", False)  # opt-in: order wanted races by the AI's learned win-rates
    _v2_defaults = {
        # priorities / filters
        "stat_priority": ["speed", "stamina", "power", "wit", "guts"],
        "summer_stat_priority": [],
        "max_failure_chance": 20,
        "risky_training": False,
        "risky_min_stat_gain": 20,
        "risky_max_failure_chance": 30,
        # targets (the core mechanic). mode "auto" (default) resolves endgame
        # targets from the codex data by scenario x distance x running style
        # (scoring.resolve_targets); "custom" uses the explicit stat_targets list.
        "stat_targets_mode": "auto",
        "stat_targets": [1150, 750, 950, 700, 450],
        "disable_stat_targets": False,
        "junior_milestone_pct": 33,
        "classic_milestone_pct": 66,
        "ratio_multipliers": [5.0, 4.0, 3.0, 2.0, 1.0, 0.5, 0.3],
        "priority_coefficient": 0.5,
        # caps
        "disable_training_on_maxed_stat": True,
        "stat_cap_buffer": 100,
        # composition
        "stat_weight_with_bars": 0.6,
        "stat_weight_no_bars": 0.7,
        "relationship_weight": 0.1,
        "misc_weight": 0.3,
        "main_stat_thresholds": [30, 30, 30, 30, 15],
        "main_stat_bonus": 1.0,  # CV-era hack; rainbows are exact here, keep off
        # relationship / junior mode
        "junior_mode_max_turn": 24,  # 0 disables the friendship-mode split
        "rel_bar_values": {"blue": 2.5, "green": 1.0, "orange": 0.0},
        "bond_green_threshold": 60,
        "rel_diminishing_factor": 0.5,
        "rel_early_game_bonus": 1.3,
        "rel_friend_bonus": 1.15,
        # rainbow / anticipation
        "rainbow_multiplier": 2.0,
        "anticipatory_enabled": True,
        "anticipatory_min_bond": 50,
        "anticipatory_coefficient": 0.2,
        "anticipatory_cap": 0.6,
        # facility levels
        "training_level_weighting": True,
        "level_boost_factors": [0.75, 0.25, 0.10],
        # hints / energy / behavior
        "prioritize_skill_hints": False,
        "skill_hint_score": 10,
        "skill_hint_override": 10000,
        "energy_weight": 2.0,
        "recreation_score_gate": 60,
        "train_wit_on_finale": True,
        "recreation_on_finale": False,  # URA finale training turns (73/75/77): outings instead of training
        "must_rest_before_summer": False,
        "ura_duel_bonus": 500,
    }
    v2 = normalized.setdefault("scoring_v2", {})
    if isinstance(v2, dict):
        # Migration: a preset that previously hand-tuned stat_targets (differs
        # from the retired flat default) keeps them via custom mode; everyone
        # else adopts the new data-driven auto targets.
        if (
            "stat_targets_mode" not in v2
            and v2.get("stat_targets")
            and v2.get("stat_targets") != [1100, 700, 900, 400, 600]
        ):
            v2["stat_targets_mode"] = "custom"
        for k, v in _v2_defaults.items():
            v2.setdefault(k, v)
    return normalized


class PresetStore:
    def __init__(self, base_dir):
        self.base_dir = Path(base_dir)
        self.preset_dir = self.base_dir / "data" / "presets"

    def ensure(self):
        self.preset_dir.mkdir(parents=True, exist_ok=True)

    def read_all(self):
        self.ensure()
        loaded = {}
        for path in self._source_files():
            try:
                data = normalize_preset(json.loads(path.read_text(encoding="utf-8")))
            except Exception as e:
                log.warning("skipping unreadable preset %s: %s", path.name, e)
                continue
            loaded[data["name"]] = data
        return sorted(loaded.values(), key=lambda item: item["name"].lower())

    def read_one(self, name):
        wanted = str(name or "").strip().lower()
        for preset in self.read_all():
            if preset["name"].lower() == wanted:
                return preset
        return None

    def write(self, preset):
        self.ensure()
        data = normalize_preset(preset)
        path = self.preset_dir / f"{slugify(data['name'])}.json"
        path.write_text(
            json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        return data

    def delete(self, name):
        path = self.preset_dir / f"{slugify(name)}.json"
        if path.exists():
            path.unlink()
            return True
        return False

    def _source_files(self):
        if self.preset_dir.exists():
            return list(self.preset_dir.glob("*.json"))
        return []
