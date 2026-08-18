"""
career_bot/ai_dataset.py
=========================
Append-only JSONL export layer.  Called from report.py:write_report() after
every career.  Never blocks or raises — all writes are try/except wrapped.

Records accumulate in  uma_runtime/ai/  across hundreds of careers and are
consumed by ai_trainer.py to build learned scoring models.

Files written
-------------
turn_decisions.jsonl      – one record per turn
event_outcome_rows.jsonl  – one record per event seen during a turn
career_summaries.jsonl    – one record per finished career
failed_runs.jsonl         – one record per career that errored

Derived (JSON, written by rebuild_advisor_stats):
advisor_stats.json        – per-program win/loss/reward aggregates
"""

from __future__ import annotations

import hashlib
import json
import os
import tempfile
import time
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Iterable, List, Mapping, MutableMapping, Optional, Tuple

SCHEMA_VERSION = 1
AI_DATASET_VERSION = "Icarus AI Dataset v1"

DATASET_FILES = {
    "turn_decisions":   "turn_decisions.jsonl",
    "event_outcomes":   "event_outcome_rows.jsonl",
    "career_summaries": "career_summaries.jsonl",
    "failed_runs":      "failed_runs.jsonl",
}

BUILD_VERSION = "icarus-1.0"


# ── path helpers ───────────────────────────────────────────────────────────────

def runtime_output_root(base_dir: Any) -> Path:
    override = os.environ.get("UMA_RUNTIME_DIR")
    if override:
        return Path(override).expanduser().resolve()
    base = Path(base_dir).resolve()
    for candidate in (base, *base.parents):
        if (candidate / ".git").exists():
            return candidate / "uma_runtime"
    return base.parent / "uma_runtime"


def ai_root_from_output_dir(output_dir: Any) -> Path:
    out = Path(output_dir)
    if out.name == "bot_logs":
        return out.parent / "ai"
    return out / "ai"


# ── safe helpers ───────────────────────────────────────────────────────────────

def safe_int(value: Any, default: int = 0) -> int:
    try:
        return int(value if value is not None else default)
    except Exception:
        return int(default)


def safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value if value is not None else default)
    except Exception:
        return float(default)


def now_iso() -> str:
    return datetime.now().isoformat(timespec="seconds")


def _json_default(obj: Any) -> str:
    if isinstance(obj, bytes):
        return obj.hex()
    return str(obj)


def _career_id(report: Mapping[str, Any]) -> str:
    key = f"{report.get('started_at', '')}:{report.get('preset_name', '')}:{report.get('scenario_id', '')}"
    return hashlib.sha1(key.encode()).hexdigest()[:12]


# ── JSONL I/O ─────────────────────────────────────────────────────────────────

def _append_jsonl(path: Path, rows: Iterable[Mapping[str, Any]]) -> int:
    path.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with open(path, "a", encoding="utf-8") as f:
        for row in rows:
            f.write(json.dumps(row, ensure_ascii=False, default=_json_default) + "\n")
            count += 1
    return count


def _atomic_write_json(path: Path, payload: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(payload, f, ensure_ascii=False, indent=2, default=_json_default)
        Path(tmp).replace(path)
    except Exception:
        try:
            os.unlink(tmp)
        except Exception:
            pass
        raise


def _read_jsonl(path: Path, limit: int = 300_000) -> List[Dict[str, Any]]:
    if not path.exists():
        return []
    rows = []
    try:
        with open(path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    rows.append(json.loads(line))
                except Exception:
                    pass
                if len(rows) >= limit:
                    break
    except Exception:
        pass
    return rows


# ── field extraction from our report.py structure ────────────────────────────

def _extract_chara_info(turn: Mapping[str, Any]) -> Dict[str, Any]:
    """Pull the most recent chara_info from API response calls in this turn."""
    for call in reversed(turn.get("api_calls") or []):
        if call.get("direction") != "RES":
            continue
        inner = (call.get("data") or {}).get("data") or {}
        chara = inner.get("chara_info") or inner.get("single_mode_chara_light")
        if chara and isinstance(chara, dict):
            return chara
    return {}


def _stats_from_chara(chara: Mapping[str, Any]) -> Dict[str, Any]:
    # chara_info uses "wiz" for wisdom (not "wisdom" or "wit")
    return {
        "speed":       safe_int(chara.get("speed")),
        "stamina":     safe_int(chara.get("stamina")),
        "power":       safe_int(chara.get("power")),
        "guts":        safe_int(chara.get("guts")),
        "wit":         safe_int(chara.get("wiz") or chara.get("wisdom") or chara.get("wit")),
        "skill_point": safe_int(chara.get("skill_point")),
        "hp":          safe_int(chara.get("vital")),
        "max_hp":      safe_int(chara.get("max_vital")),
        "mood":        safe_int(chara.get("motivation")),
        "fans":        safe_int(chara.get("fans")),
    }


def _stats_from_turn(turn: Mapping[str, Any]) -> Dict[str, Any]:
    """Read stats directly from the turn snapshot (always accurate, no API parsing needed)."""
    s = turn.get("stats") or {}
    # fans rides on turn['stats'] since the runner started logging it; fall
    # back to chara_info (api_calls, debug-mode only) for older logs.
    fans = safe_int(s.get("fans"))
    if not fans:
        fans = safe_int(_extract_chara_info(turn).get("fans"))
    return {
        "speed":       safe_int(s.get("speed")),
        "stamina":     safe_int(s.get("stamina")),
        "power":       safe_int(s.get("power")),
        "guts":        safe_int(s.get("guts")),
        "wit":         safe_int(s.get("wit") or s.get("wiz")),
        "skill_point": safe_int(s.get("skill_point")),
        "hp":          safe_int(s.get("hp") or s.get("vital")),
        "max_hp":      safe_int(s.get("max_hp") or s.get("max_vital")),
        "mood":        safe_int(s.get("motivation")),
        "fans":        fans,
    }


def _extract_race_result(turn: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    """Extract race result: structured runner row first, api_calls fallback."""
    # The runner logs a "race_result" event with the parsed race_end payload -
    # present on every run, debug toggle or not.
    for ev in turn.get("events") or []:
        if isinstance(ev, dict) and ev.get("event") == "race_result":
            rank = safe_int(ev.get("rank"), 99)
            return {
                "rank":        rank if rank > 0 else 99,
                "fans_gained": safe_int(ev.get("fans_gained")),
            }
    # Fallback: dig the race_end response out of full api_call capture.
    for call in turn.get("api_calls") or []:
        if call.get("direction") != "RES":
            continue
        ep = str(call.get("ep") or call.get("endpoint") or "")
        if "race_end" not in ep:
            continue
        inner = (call.get("data") or {}).get("data") or {}
        # API uses "race_reward_info" (not "race_result_info")
        race_info = inner.get("race_reward_info") or inner.get("race_result_info") or {}
        if race_info:
            return {
                "rank":        safe_int(race_info.get("result_rank") or race_info.get("rank"), 99),
                "fans_gained": safe_int(race_info.get("gained_fans") or race_info.get("fans")),
            }
    return None


def _action_type(turn: Mapping[str, Any]) -> str:
    action = str(turn.get("selected_action") or turn.get("current_action_taken") or "")
    if action:
        return action
    # Infer from API calls
    for call in turn.get("api_calls") or []:
        ep = str(call.get("ep") or call.get("endpoint") or "")
        if "race_entry" in ep or "race_end" in ep or "race_start" in ep:
            return "race"
        if "exec_command" in ep:
            return "train"
        if "check_event" in ep:
            return "event"
    return "unknown"


def _program_id(turn: Mapping[str, Any]) -> Optional[int]:
    cmd = turn.get("current_command") or {}
    pid = cmd.get("program_id")
    if pid:
        return safe_int(pid)
    for call in turn.get("api_calls") or []:
        ep = str(call.get("ep") or "")
        if "race_entry" in ep or "race_end" in ep:
            payload = (call.get("data") or {})
            pid = payload.get("program_id")
            if pid:
                return safe_int(pid)
    return None


def _turn_reward(
    turn: Mapping[str, Any],
    next_turn: Optional[Mapping[str, Any]],
    report: Mapping[str, Any],
) -> float:
    stats = _stats_from_turn(turn)
    next_stats = _stats_from_turn(next_turn) if next_turn else {}

    reward = 0.0

    # Stat/fan progress between turns
    for key in ("speed", "stamina", "power", "guts", "wit"):
        reward += max(0, safe_int(next_stats.get(key)) - safe_int(stats.get(key))) * 0.025
    reward += max(0, safe_int(next_stats.get("skill_point")) - safe_int(stats.get("skill_point"))) * 0.015
    reward += max(0, safe_int(next_stats.get("fans")) - safe_int(stats.get("fans"))) / 10_000.0

    atype = _action_type(turn)
    if atype == "race":
        result = _extract_race_result(turn)
        if result:
            rank = safe_int(result.get("rank"), 99)
            reward += 8.0 if rank == 1 else -12.0 - min(8.0, max(0, rank - 2) * 2.0)
        else:
            reward -= 0.5
    elif atype == "train":
        reward += 1.0
    elif atype in ("rest", "recreate", "recover"):
        reward -= 1.0
    elif atype == "finish":
        reward += 20.0 if str(report.get("status")) == "finished" else -20.0

    return round(reward, 4)


def _turn_phase(turn_num: int) -> str:
    if turn_num <= 24:
        return "early"
    if turn_num <= 48:
        return "mid"
    if turn_num <= 60:
        return "late"
    return "senior"


# ── record builders ───────────────────────────────────────────────────────────

def turn_decision_records(
    report: Mapping[str, Any],
    build_version: str = BUILD_VERSION,
) -> List[Dict[str, Any]]:
    turns = report.get("turns") or []
    career_id = _career_id(report)
    records = []
    for i, turn in enumerate(turns):
        turn_num = safe_int(turn.get("turn"))
        next_turn = turns[i + 1] if i + 1 < len(turns) else None
        chara = _extract_chara_info(turn)
        race_result = _extract_race_result(turn)
        atype = _action_type(turn)
        pid = _program_id(turn)
        cmd = turn.get("current_command") or {}
        reward = _turn_reward(turn, next_turn, report)

        records.append({
            "schema":          SCHEMA_VERSION,
            "dataset_version": AI_DATASET_VERSION,
            "build_version":   build_version,
            "exported_at":     now_iso(),
            "career_id":       career_id,
            "turn":            turn_num,
            "action": {
                "type":       atype,
                "reason":     str(turn.get("decision_reason") or ""),
                "payload":    dict(cmd),
                "program_id": pid,
                "command_type":     cmd.get("command_type"),
                "command_id":       cmd.get("command_id"),
                "command_group_id": cmd.get("command_group_id"),
            },
            # Turn snapshot stats are always present; chara_info (api_calls)
            # exists only in debug-mode logs and left this all-zeros before.
            "state":   _stats_from_turn(turn),
            "outcome": {
                "reward":      reward,
                "race_result": race_result,
            },
            "turn_metadata": {
                "phase":                _turn_phase(turn_num),
                "events":               turn.get("events") or [],
                "item_usage_attempts":  turn.get("item_usage_attempts") or [],
                "item_buy_attempts":    turn.get("item_buy_attempts") or [],
                "skill_buy_attempts":   turn.get("skill_buy_attempts") or [],
            },
        })
    return records


def event_outcome_records(
    report: Mapping[str, Any],
) -> List[Dict[str, Any]]:
    turns = report.get("turns") or []
    career_id = _career_id(report)
    records = []
    for i, turn in enumerate(turns):
        turn_num = safe_int(turn.get("turn"))
        next_turn = turns[i + 1] if i + 1 < len(turns) else None
        stats_before = _stats_from_turn(turn)
        stats_after  = _stats_from_turn(next_turn) if next_turn else {}

        def _append(event_name, choice):
            stat_deltas = {
                k: safe_int(stats_after.get(k)) - safe_int(stats_before.get(k))
                for k in ("speed", "stamina", "power", "guts", "wit", "skill_point", "fans")
                if safe_int(stats_after.get(k)) - safe_int(stats_before.get(k)) != 0
            }
            reward = sum(v * 0.025 for k, v in stat_deltas.items() if k != "fans")
            reward += stat_deltas.get("fans", 0) / 10_000.0
            records.append({
                "schema":          SCHEMA_VERSION,
                "dataset_version": AI_DATASET_VERSION,
                "career_id":       career_id,
                "turn":            turn_num,
                "event_name":      event_name,
                "choice":          str(choice),
                "reward":          round(reward, 4),
                "stat_deltas":     stat_deltas,
                "exported_at":     now_iso(),
            })

        # Primary: the runner logs every choice it actually makes as an
        # "event_choice_made" row (present with or without the debug toggle).
        logged = [
            ev for ev in (turn.get("events") or [])
            if isinstance(ev, dict) and ev.get("event") == "event_choice_made"
        ]
        for ev in logged:
            event_name = str(ev.get("story_id") or ev.get("event_id") or "")
            if event_name:
                _append(event_name, ev.get("choice_number"))
        if logged:
            continue

        # Fallback for debug-mode logs without structured rows. Events surface
        # in ANY endpoint's response (exec_command, race_out, ...), not just
        # check_event - and the choice actually MADE is the choice_number of
        # the check_event REQUEST that answers it, not the offered list of the
        # next event that the old extractor recorded.
        calls = turn.get("api_calls") or []
        for idx, call in enumerate(calls):
            if call.get("direction") != "RES":
                continue
            inner = (call.get("data") or {}).get("data") or {}
            events = inner.get("unchecked_event_array") or []
            if not events:
                continue
            ev = events[0]
            if not isinstance(ev, dict):
                continue
            event_name = str(ev.get("story_id") or ev.get("event_id") or "")
            if not event_name:
                continue
            choice = ""
            for later in calls[idx + 1:]:
                if later.get("direction") != "REQ":
                    continue
                lep = str(later.get("ep") or later.get("endpoint") or "")
                if "check_event" in lep:
                    choice = (later.get("data") or {}).get("choice_number", "")
                    break
            _append(event_name, choice)
    return records


def career_summary_record(
    report: Mapping[str, Any],
    build_version: str = BUILD_VERSION,
) -> Dict[str, Any]:
    turns = report.get("turns") or []
    career_id = _career_id(report)

    # Final stats from last turn that has stats
    final_stats: Dict[str, Any] = {}
    for turn in reversed(turns):
        s = _stats_from_turn(turn)
        if any(s.get(k) for k in ("speed", "stamina", "power")):
            final_stats = s
            break

    # Race summary
    race_turns = [t for t in turns if _action_type(t) == "race"]
    wins = sum(1 for t in race_turns if safe_int((_extract_race_result(t) or {}).get("rank"), 99) == 1)
    total_races = len(race_turns)

    return {
        "schema":            SCHEMA_VERSION,
        "dataset_version":   AI_DATASET_VERSION,
        "career_id":         career_id,
        "exported_at":       now_iso(),
        "build_version":     build_version,
        "preset_name":       str(report.get("preset_name") or ""),
        "scenario_id":       safe_int(report.get("scenario_id")),
        "career_completed":  str(report.get("status")) == "finished",
        "total_turns":       safe_int(report.get("final_turn")),
        "final_fans":        safe_int(final_stats.get("fans")),
        "final_stats":       final_stats,
        "race_summary": {
            "total_races": total_races,
            "wins":        wins,
            "win_rate":    round(wins / total_races, 4) if total_races else 0.0,
        },
    }


# ── main export entry point ───────────────────────────────────────────────────

def export_report_ai_datasets(
    report: Mapping[str, Any],
    output_dir: Any,
    build_version: str = BUILD_VERSION,
) -> Dict[str, Any]:
    """Export a finished career report to JSONL datasets.

    Called by report.py:write_report().  Failures are silent.
    Returns a manifest dict describing what was written.
    """
    try:
        root = ai_root_from_output_dir(output_dir)
        root.mkdir(parents=True, exist_ok=True)

        turn_rows  = turn_decision_records(report, build_version)
        event_rows = event_outcome_records(report)
        summary    = career_summary_record(report, build_version)

        counts: Dict[str, int] = {}
        counts["turn_decisions"]   = _append_jsonl(root / DATASET_FILES["turn_decisions"],   turn_rows)
        counts["event_outcomes"]   = _append_jsonl(root / DATASET_FILES["event_outcomes"],   event_rows)
        counts["career_summaries"] = _append_jsonl(root / DATASET_FILES["career_summaries"], [summary])

        manifest = {
            "exported_at":    now_iso(),
            "career_id":      _career_id(report),
            "preset_name":    report.get("preset_name", ""),
            "status":         report.get("status", ""),
            "records_added":  counts,
            "ai_root":        str(root),
        }
        _atomic_write_json(root / "latest_export_manifest.json", manifest)

        # Trigger background training (imported lazily to avoid circular imports)
        try:
            from career_bot import ai_trainer as _trainer
            _trainer.after_career_export(output_dir, manifest, build_version)
        except Exception:
            pass

        return manifest

    except Exception as exc:
        return {"success": False, "error": str(exc)}


def failed_run_record(
    report: Mapping[str, Any],
    error: str,
    turn: int = 0,
) -> Dict[str, Any]:
    return {
        "schema":      SCHEMA_VERSION,
        "career_id":   _career_id(report),
        "exported_at": now_iso(),
        "error":       error,
        "turn":        turn,
        "preset_name": str(report.get("preset_name") or ""),
    }


def export_failed_run(report: Mapping[str, Any], output_dir: Any, error: str, turn: int = 0) -> None:
    try:
        root = ai_root_from_output_dir(output_dir)
        _append_jsonl(root / DATASET_FILES["failed_runs"], [failed_run_record(report, error, turn)])
    except Exception:
        pass


# ── advisor stats rebuild ─────────────────────────────────────────────────────

def rebuild_advisor_stats(ai_root: Path) -> Dict[str, Any]:
    """Aggregate JSONL records into advisor_stats.json for the AI advisor."""
    turn_rows    = _read_jsonl(ai_root / DATASET_FILES["turn_decisions"])
    summary_rows = _read_jsonl(ai_root / DATASET_FILES["career_summaries"])

    # Per-program flat bucket
    programs: Dict[str, Dict[str, Any]] = defaultdict(lambda: {
        "starts": 0, "wins": 0, "losses": 0, "total_reward": 0.0,
        "total_rank": 0, "rank_count": 0,
    })

    # Context buckets
    by_program:                  Dict[str, Dict] = defaultdict(lambda: dict(starts=0, wins=0, total_reward=0.0))
    by_program_scenario:         Dict[str, Dict] = defaultdict(lambda: dict(starts=0, wins=0, total_reward=0.0))
    by_program_scenario_preset:  Dict[str, Dict] = defaultdict(lambda: dict(starts=0, wins=0, total_reward=0.0))
    by_program_scenario_phase:   Dict[str, Dict] = defaultdict(lambda: dict(starts=0, wins=0, total_reward=0.0))

    action_counts: Dict[str, Dict] = defaultdict(lambda: {"count": 0, "total_reward": 0.0})

    scenario_id = None
    preset_name = ""

    for row in turn_rows:
        action = row.get("action") or {}
        atype   = str(action.get("type") or "unknown")
        reward  = safe_float(row.get("outcome", {}).get("reward"))
        turn_n  = safe_int(row.get("turn"))
        phase   = _turn_phase(turn_n)

        action_counts[atype]["count"] += 1
        action_counts[atype]["total_reward"] += reward

        if atype != "race":
            continue

        pid = action.get("program_id")
        if not pid:
            continue
        pid_str = str(pid)

        race_result = (row.get("outcome") or {}).get("race_result") or {}
        rank = safe_int(race_result.get("rank"), 99) if race_result else 99
        won  = rank == 1

        # Infer context from summary rows (best effort — use career_id)
        career_id = row.get("career_id", "")

        p = programs[pid_str]
        p["starts"]       += 1
        p["wins"]         += 1 if won else 0
        p["losses"]       += 0 if won else 1
        p["total_reward"] += reward
        if race_result and rank < 99:
            p["total_rank"]  += rank
            p["rank_count"]  += 1

        def _bump(d: Dict, key: str) -> None:
            d[key]["starts"]       += 1
            d[key]["wins"]         += 1 if won else 0
            d[key]["total_reward"] += reward

        _bump(by_program, pid_str)

    # Finalise per-program
    finalised: Dict[str, Any] = {}
    for pid_str, p in programs.items():
        starts = p["starts"]
        wins   = p["wins"]
        finalised[pid_str] = {
            "starts":     starts,
            "wins":       wins,
            "losses":     p["losses"],
            "win_rate":   round(wins / starts, 4) if starts else 0.0,
            "avg_reward": round(p["total_reward"] / starts, 4) if starts else 0.0,
            "avg_rank":   round(p["total_rank"] / p["rank_count"], 2) if p["rank_count"] else None,
        }

    def _finalise_ctx(d: Dict[str, Dict]) -> Dict[str, Any]:
        out = {}
        for k, v in d.items():
            s = v["starts"]
            out[k] = {
                "starts":     s,
                "wins":       v["wins"],
                "win_rate":   round(v["wins"] / s, 4) if s else 0.0,
                "avg_reward": round(v["total_reward"] / s, 4) if s else 0.0,
            }
        return out

    action_stats: Dict[str, Any] = {}
    for atype, v in action_counts.items():
        c = v["count"]
        action_stats[atype] = {
            "count":      c,
            "avg_reward": round(v["total_reward"] / c, 4) if c else 0.0,
        }

    stats = {
        "built_at":      now_iso(),
        "race_programs": finalised,
        "race_programs_context": {
            "by_program": _finalise_ctx(by_program),
        },
        "actions": action_stats,
        "records": {
            "turn_decisions":   len(turn_rows),
            "career_summaries": len(summary_rows),
        },
    }

    _atomic_write_json(ai_root / "advisor_stats.json", stats)
    return stats


# ── dataset status ────────────────────────────────────────────────────────────

def dataset_status(base_dir: Any) -> Dict[str, Any]:
    """Return record counts and file sizes for all dataset files."""
    root = runtime_output_root(base_dir) / "ai"
    result: Dict[str, Any] = {"ai_root": str(root), "files": {}}
    for key, filename in DATASET_FILES.items():
        path = root / filename
        if path.exists():
            size = path.stat().st_size
            rows = _read_jsonl(path, limit=1_000_000)
            result["files"][key] = {
                "exists":      True,
                "size_bytes":  size,
                "record_count": len(rows),
                "modified_at": datetime.fromtimestamp(path.stat().st_mtime).isoformat(timespec="seconds"),
            }
        else:
            result["files"][key] = {"exists": False, "record_count": 0}

    manifest_path = root / "latest_export_manifest.json"
    if manifest_path.exists():
        try:
            result["latest_manifest"] = json.loads(manifest_path.read_text(encoding="utf-8"))
        except Exception:
            pass

    return result


def rebuild_from_career_logs(base_dir: Any, build_version: str = BUILD_VERSION) -> Dict[str, Any]:
    """Re-export all career log JSONs from uma_runtime/bot_logs/ into JSONL datasets."""
    root = runtime_output_root(base_dir)
    logs_dir = root / "bot_logs"
    if not logs_dir.exists():
        return {"success": False, "detail": "bot_logs directory not found"}

    exported = 0
    errors = 0
    for log_path in sorted(logs_dir.glob("career_log_*.json")):
        try:
            report = json.loads(log_path.read_text(encoding="utf-8"))
            export_report_ai_datasets(report, logs_dir, build_version)
            exported += 1
        except Exception as exc:
            errors += 1

    return {"success": True, "exported": exported, "errors": errors}
