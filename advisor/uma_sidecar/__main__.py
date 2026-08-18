"""Overseer advisor sidecar.

Bridges the in-process DLL (which captures + decrypts the game's career/race network payloads and
serves the web SPA on 127.0.0.1:1620) to Icarus's `career_bot` decision engine, WITHOUT the DLL ever
running Python or the engine ever touching the game. Loop:

    GET  /internal/capture?since=<seq>   -> {seq, kind, payload_b64}   (base64 of raw msgpack)
    (decode with msgpack; compute advice/reveal with career_bot)
    POST /internal/advice  {seq, advice, reveal, sidecar}

All requests carry `X-Overseer-Token` (per-boot, from the OVERSEER_TOKEN env). The advice/reveal JSON
shape is FROZEN by the SPA renderers — see the Advice/Reveal contract in the DLL. This file is a
robust scaffold: it always emits a valid (possibly minimal) Advice from the captured state, and
upgrades to full career_bot output when the engine is importable and the payload matches.

Run: `python -m uma_sidecar`  (env: OVERSEER_URL, OVERSEER_TOKEN, OVERSEER_GAME_PID, OVERSEER_ICARUS_DIR)
"""
from __future__ import annotations

import base64
import json
import os
import sys
import time
import urllib.request

URL = os.environ.get("OVERSEER_URL", "http://127.0.0.1:1620")
TOKEN = os.environ.get("OVERSEER_TOKEN", "")
GAME_PID = int(os.environ.get("OVERSEER_GAME_PID", "0") or "0")
ICARUS_DIR = os.environ.get("OVERSEER_ICARUS_DIR", "")

FACILITY = {101: "Speed", 105: "Stamina", 102: "Power", 103: "Guts", 106: "Wit"}
GAIN = {1: "Spd", 2: "Sta", 3: "Pwr", 4: "Gut", 5: "Wit", 30: "Skl", 10: "Nrg"}


def log(m):
    print("[sidecar] " + m, flush=True)


# ── optional deps ──────────────────────────────────────────────────────────────────────────────
try:
    import msgpack  # type: ignore
    HAVE_MSGPACK = True
except Exception:
    HAVE_MSGPACK = False

# ── Icarus career_bot engine ────────────────────────────────────────────────────────────────────
# The REAL advisor is next_decision (scenarios) + scoring + RacePlanner/SkillBuyer/EventManager —
# there is no `career_bot.advisor` module (the old import here always failed → minimal extractor).
# CRITICAL: never import career_bot.dailies / claimables / uma_api — those pull native wheels
# (curl_cffi, pycryptodome, frida) that a plug-and-play embeddable-Python bundle can't ship. The
# advice path below is 100% pure-Python + stdlib sqlite3 (reads the game's live master.mdb).
ENGINE_OK = False
STRATEGIES = None
UraStrategy = None
normalize_preset = None
scoring = None
PLANNER = None
SKILLBUYER = None
EVENTS = None
TRAINING_COMMANDS = {}
TRAINING_NAMES = ["Speed", "Stamina", "Power", "Guts", "Wit"]
if ICARUS_DIR and os.path.isdir(ICARUS_DIR):
    sys.path.insert(0, ICARUS_DIR)
try:
    from career_bot.scenarios import STRATEGIES  # type: ignore
    from career_bot.scenarios.ura import UraStrategy  # type: ignore
    from career_bot.presets import normalize_preset  # type: ignore
    from career_bot import scoring  # type: ignore
    from career_bot.races import RacePlanner  # type: ignore
    from career_bot.skills import SkillBuyer  # type: ignore
    from career_bot.events import EventManager  # type: ignore
    from career_bot.constants import TRAINING_COMMANDS, TRAINING_NAMES  # type: ignore

    PLANNER = RacePlanner(ICARUS_DIR)
    SKILLBUYER = SkillBuyer(ICARUS_DIR)
    EVENTS = EventManager(ICARUS_DIR)
    ENGINE_OK = True
    log("career_bot engine loaded (real Icarus advisor)")
except Exception as e:  # noqa: BLE001
    log(f"career_bot not available ({type(e).__name__}: {e}); using minimal extractor")


# ── HTTP ────────────────────────────────────────────────────────────────────────────────────────
def _req(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(URL + path, data=data, method=method)
    r.add_header("X-Overseer-Token", TOKEN)
    if data is not None:
        r.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(r, timeout=30) as resp:
        if resp.status == 204:
            return None
        raw = resp.read()
        return json.loads(raw) if raw else None


def game_alive():
    """Liveness by PID. On Windows os.kill(pid, 0) does NOT probe — it can terminate or error — so
    use the Win32 API. We also fall back to "assume alive" and let the loop's connection-failure
    counter handle a dead server (the DLL's web server disappears when the game closes)."""
    if not GAME_PID:
        return True
    if sys.platform == "win32":
        try:
            import ctypes
            from ctypes import wintypes
            PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
            k32 = ctypes.windll.kernel32
            h = k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, GAME_PID)
            if not h:
                return False
            code = wintypes.DWORD()
            k32.GetExitCodeProcess(h, ctypes.byref(code))
            k32.CloseHandle(h)
            return code.value == 259  # STILL_ACTIVE
        except Exception:
            return True  # can't tell → don't exit on a probe error; the fail-counter covers it
    try:
        os.kill(GAME_PID, 0)
        return True
    except OSError:
        return False


# ── advice computation ────────────────────────────────────────────────────────────────────────
def _minimal(data):
    """Fallback when the engine can't run — surface turn/stats/vital/mood so the SPA shows real state."""
    chara = data.get("chara_info", {}) if isinstance(data, dict) else {}
    def g(*keys, default=None):
        for k in keys:
            if isinstance(chara, dict) and k in chara:
                return chara[k]
        return default
    return {
        "scenario_id": g("scenario_id", default=0),
        "turn": g("turn", default=None),
        "vital": g("vital", default=0),
        "max_vital": g("max_vital", default=100),
        "mood": g("motivation", default=3),
        "action": "idle",
        "reason": "advisor engine not loaded — showing captured career state",
        "best_facility": None,
        "training": [],
        "stats": {
            "speed": g("speed", default=0), "stamina": g("stamina", default=0),
            "power": g("power", default=0), "guts": g("guts", default=0),
            "wiz": g("wiz", "wisdom", default=0), "skill_point": g("skill_point", default=0),
            "fans": g("fan_count", "fans", default=0),
        },
        "event_advice": None,
        "skill_advice": [],
    }


def _build_training(data, chara, preset, sid):
    """Score every enabled training facility this turn (independent of the chosen action, so the bars
    always render). Uses the engine's own build_facility (gains/fail) + score_command (v2 score)."""
    ctx = scoring.build_context(chara, preset, base_dir=ICARUS_DIR, scenario_id=sid)
    cmds = (data.get("home_info") or {}).get("command_info_array") or []
    names5 = ["speed", "stamina", "power", "guts", "wit"]
    rows = []
    for cmd in cmds:
        if not cmd.get("is_enable", 1) or cmd.get("command_type") != 1:
            continue
        cid = cmd.get("command_id") or cmd.get("command_group_id") or 0
        if cid not in TRAINING_COMMANDS:
            continue
        fac = scoring.build_facility(cmd, ctx)
        try:
            score, _bd = scoring.score_command(cmd, ctx, over_buffer_used=set())
        except Exception:  # noqa: BLE001
            score = 0.0
        gains = {}
        for i, v in enumerate(fac.get("gains") or []):
            iv = int(round(v))
            if iv:
                gains[names5[i]] = iv
        fail = fac.get("fail")
        rows.append({
            "facility": fac.get("name"),
            "score": round(float(score), 2),
            "success": (100 - int(fail)) if fail is not None else None,
            "gains": gains,
            "best": False,
        })
    return rows


def _translate(decision, chara, state):
    """Map an engine Decision to (action, best_facility, reason) for the SPA. Home-screen commands all
    have action=='command'; the command_type distinguishes train / rest / recreate / infirmary."""
    reason = (getattr(decision, "reason", None) if decision else None) or state.get("_decision_reason") or ""
    if decision is None:
        return "idle", None, reason or "advisor could not decide this turn"
    act = decision.action
    payload = decision.payload or {}
    if act == "command":
        ct = int(payload.get("command_type") or 1)
        if ct == 1:
            cid = payload.get("command_id") or payload.get("command_group_id") or 0
            idx = TRAINING_COMMANDS.get(cid)
            fac = TRAINING_NAMES[idx] if idx is not None and idx < len(TRAINING_NAMES) else None
            return "command", fac, reason
        return {7: "rest", 8: "infirmary", 3: "recreation"}.get(ct, "command"), None, reason
    return act, None, reason  # event / race / race_progress / team_race / finish / idle / done


def _event_advice(data, decision):
    if not (decision and decision.action == "event" and EVENTS):
        return None
    events = data.get("unchecked_event_array") or []
    if not events:
        return None
    event = events[0] or {}
    try:
        choice = EVENTS.choose(event)
    except Exception:  # noqa: BLE001
        choice = None
    story_id = str(event.get("story_id", ""))
    return {
        "name": EVENTS.event_names.get(story_id, "Unknown Event"),
        "choice": choice,
        "mapped": EVENTS.outcomes.get(story_id) is not None,
    }


def _skill_advice(state, preset):
    if not SKILLBUYER:
        return []
    try:
        SKILLBUYER.preview(state, preset, force=True)  # force: show picks even below the SP threshold
        return [
            {"name": c.get("name"), "cost": int(c.get("cost") or 0)}
            for c in (SKILLBUYER.last_selected or [])
        ][:8]
    except Exception as e:  # noqa: BLE001
        log(f"skill preview failed ({type(e).__name__}: {e})")
        return []


def compute_career(data):
    """Full advice via the real Icarus engine: next_decision picks the action + reason, scoring fills
    the per-facility bars, EventManager/SkillBuyer add the chips. Falls back to _minimal on any drift."""
    if not ENGINE_OK or not isinstance(data, dict):
        return _minimal(data)
    chara = data.get("chara_info") or {}
    if not chara:
        return _minimal(data)
    try:
        sid = int(chara.get("scenario_id") or 1)
        preset = normalize_preset({"scenario_id": sid})
        state = {"data": data}
        strategy = STRATEGIES.get(sid, UraStrategy)(PLANNER)
        try:
            decision = strategy.next_decision(state, preset, suppress_print=True)
        except Exception as e:  # noqa: BLE001
            log(f"next_decision failed ({type(e).__name__}: {e})")
            decision = None

        training = _build_training(data, chara, preset, sid)
        action, best_facility, reason = _translate(decision, chara, state)

        if best_facility:
            for t in training:
                if t["facility"] == best_facility:
                    t["best"] = True
                    break
        elif training:  # rest/race/event: still highlight the top training row for reference
            max(training, key=lambda t: t["score"])["best"] = True

        return {
            "scenario_id": sid,
            "turn": chara.get("turn"),
            "vital": int(chara.get("vital") or 0),
            "max_vital": int(chara.get("max_vital") or 100),
            "mood": int(chara.get("motivation") or 3),
            "action": action,
            "reason": reason,
            "best_facility": best_facility,
            "training": training,
            "stats": {
                "speed": int(chara.get("speed") or 0),
                "stamina": int(chara.get("stamina") or 0),
                "power": int(chara.get("power") or 0),
                "guts": int(chara.get("guts") or 0),
                "wiz": int(chara.get("wiz") or 0),
                "skill_point": int(chara.get("skill_point") or 0),
                "fans": int(chara.get("fan_count") or chara.get("fans") or 0),
            },
            "event_advice": _event_advice(data, decision),
            "skill_advice": _skill_advice(state, preset),
        }
    except Exception as e:  # noqa: BLE001
        log(f"compute_career failed ({type(e).__name__}: {e}); minimal")
        return _minimal(data)


def compute_race(msg):
    """Race reveal. The binary race_scenario decoder is not bundled in this Icarus tree (see research
    risks), so we emit null until it's sourced — Predictions stays on its 'no race yet' state."""
    return None


# ── main loop ────────────────────────────────────────────────────────────────────────────────
def main():
    if not TOKEN:
        log("no OVERSEER_TOKEN in env — refusing to run")
        sys.exit(2)  # non-zero on purpose: a clean exit 0 is the supervisor's silent "game gone" path
    log(f"starting; url={URL} msgpack={HAVE_MSGPACK} engine={'yes' if ENGINE_OK else 'no'}")
    seq = 0
    last_advice = None
    last_reveal = None
    fails = 0
    while True:
        if not game_alive():
            log("game process gone — exiting")
            return
        try:
            got = _req("GET", f"/internal/capture?since={seq}")
            fails = 0
        except Exception:
            fails += 1
            if fails > 40:  # ~40s of an unreachable server ⇒ the game (and its :1620) is gone
                log("server unreachable — exiting")
                return
            time.sleep(1.0)
            continue
        if got is None:
            # nothing new — heartbeat so the SPA shows "online", then poll again
            try:
                _req("POST", "/internal/advice", {"seq": seq, "advice": None, "reveal": None,
                                                  "sidecar": {"ok": True, "error": None}})
            except Exception:
                pass
            time.sleep(0.6)
            continue
        seq = got.get("seq", seq)
        advice, reveal, err = last_advice, last_reveal, None
        try:
            payload = base64.b64decode(got["payload_b64"]) if got.get("payload_b64") else b""
            if HAVE_MSGPACK and payload:
                msg = msgpack.unpackb(payload, raw=False, strict_map_key=False)
                data = msg.get("data", msg) if isinstance(msg, dict) else msg
                if got.get("kind") == "race":
                    reveal = compute_race(msg)
                else:
                    advice = compute_career(data)
                    last_advice = advice
            elif not HAVE_MSGPACK:
                err = "msgpack not installed in the sidecar's Python"
        except Exception as e:  # noqa: BLE001
            err = f"{type(e).__name__}: {e}"
            log(f"payload error: {err}")
        try:
            _req("POST", "/internal/advice", {
                "seq": seq, "advice": advice, "reveal": reveal,
                "sidecar": {"ok": err is None, "error": err},
            })
        except Exception:
            pass


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
