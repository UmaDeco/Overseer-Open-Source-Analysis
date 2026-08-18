"""Plain-English explanations for game API errors - single source of truth.

Three consumers, one table, so they can never drift:
  1. The log console: uma_api.client logs ONE friendly line per API error
     (explain_api_error), with the technical payloads demoted to the DEBUG
     file level for exported bug reports.
  2. The Help page: /api/help/errors serves ERROR_REFERENCE so players can
     look up any code - what it means and what to do.
  3. Discord webhooks: friendly_error_text() rewrites a technical
     "API error {rc} on {ep}: ..." string into the friendly line before it
     is sent to the user's channel.

Wording rules: action-first (the fix matters more than the code), one line
for the log console, no jargon ("state mismatch" -> "your account's state
changed since Icarus last looked").
"""

import re

# endpoint suffix -> what the player would call the action. Checked in order,
# first match wins, so put the more specific fragments first.
_ACTIVITY = (
    ("continue", "using an alarm clock"),
    ("gain_skills", "buying skills"),
    ("multi_item_exchange", "buying shop items"),
    ("multi_item_use", "using items"),
    ("check_event", "answering an event"),
    ("race_entry", "entering a race"),
    ("reserve_race", "scheduling a race"),
    ("team_race", "a team race"),
    ("race_start", "running a race"),
    ("race_end", "running a race"),
    ("race_out", "running a race"),
    ("race_reward", "collecting race rewards"),
    ("exec_command", "a training turn"),
    ("finish", "finishing the career"),
    ("/start", "starting the career"),
    ("load/index", "loading your account"),
    ("home/index", "loading your account"),
    ("trained_chara", "managing veterans"),
    ("mission", "collecting missions"),
    ("present", "collecting presents"),
)

# code -> {title, meaning, fix}. `meaning` and `fix` are full sentences,
# written for players. {doing} in `meaning` is replaced with the endpoint's
# activity name when a live error is being explained (the Help page shows the
# generic "this action" instead).
ERROR_REFERENCE = {
    102: {
        "title": "Account state changed",
        "meaning": (
            "The game refused {doing} because your account's state changed since "
            "Icarus last looked - for example a career is already active, a selected "
            "parent uma is gone, or a borrowed parent is used up for today."
        ),
        "fix": (
            "Refresh the dashboard, re-select your parents, and resume or delete any "
            "active career. If you borrowed a parent, it may have hit the daily borrow "
            "limit - pick your own parents or set a backup parent."
        ),
    },
    201: {
        "title": "Session expired",
        "meaning": "Your game session expired while {doing}.",
        "fix": (
            "Icarus signs in again automatically - if it doesn't, log in again from "
            "the dashboard."
        ),
    },
    205: {
        "title": "Data out of step",
        "meaning": (
            "The game rejected {doing} because its data and Icarus's got out of step "
            "for a moment - usually harmless, Icarus re-syncs and continues."
        ),
        "fix": (
            "Nothing, normally. If the SAME action keeps failing, refresh the "
            "dashboard and re-select before starting again."
        ),
    },
    208: {
        "title": "Servers busy",
        "meaning": "The game servers are busy - Icarus already waited and retried several times.",
        "fix": "Give it a minute and try again.",
    },
    204: {
        "title": "App update required",
        "meaning": (
            "The Umamusume game client got a full version update - the game itself "
            "needs updating (unlike 214, a routine data refresh Icarus handles on "
            "its own)."
        ),
        "fix": (
            "Update the Umamusume game in Steam to the latest version, then restart "
            "Icarus and sign in again."
        ),
    },
    214: {
        "title": "Game data update",
        "meaning": "The game just shipped a data update.",
        "fix": (
            "Icarus adopts it automatically; if things look stuck afterwards, restart "
            "Icarus."
        ),
    },
    217: {
        "title": "Session couldn't be verified",
        "meaning": "The game couldn't verify this session.",
        "fix": "Sign in again from the dashboard; if it repeats, restart Icarus and the game.",
    },
    391: {
        "title": "Session expired",
        "meaning": "Your game session expired while {doing}.",
        "fix": (
            "Icarus signs in again automatically - if it doesn't, log in again from "
            "the dashboard."
        ),
    },
    394: {
        "title": "Session expired",
        "meaning": "Your game session expired while {doing}.",
        "fix": (
            "Icarus signs in again automatically - if it doesn't, log in again from "
            "the dashboard."
        ),
    },
    525: {
        "title": "Career start refused (TP / boost)",
        "meaning": (
            "The game refused the career start - usually not enough TP, or an event "
            "boost that's no longer valid."
        ),
        "fix": "Check your TP and boost settings, then retry.",
    },
    709: {
        "title": "Account id refreshed",
        "meaning": "The game refreshed your account id mid-session.",
        "fix": "Icarus adopts it and continues automatically - nothing to do.",
    },
    1053: {
        "title": "Daily already done",
        "meaning": "Today's daily race is already completed.",
        "fix": "Nothing to do until the daily reset.",
    },
    1453: {
        "title": "Borrowed parent needs a follow",
        "meaning": (
            "The borrowed parent can't be used because you're not following its owner "
            "in-game."
        ),
        "fix": "Follow that trainer in the game, or pick a different parent.",
    },
    2051: {
        "title": "Veteran storage full",
        "meaning": "Your veteran (trained uma) storage is full.",
        "fix": "Delete some veterans on the Veterans page, then try again.",
    },
    2502: {
        "title": "Running-style change declined",
        "meaning": "The game declined a running-style change.",
        "fix": "Harmless - the race continues with the current style.",
    },
    2511: {
        "title": "Veteran storage full",
        "meaning": "Your veteran (trained uma) storage is full.",
        "fix": "Delete some veterans on the Veterans page, then try again.",
    },
    9001: {
        "title": "No active story event",
        "meaning": (
            "You asked to start a career with Event Boost, but there's no story "
            "event running right now, so the boost can't be applied."
        ),
        "fix": (
            "Turn off Event Boost in your preset (or wait for a story event to be "
            "live), then start the career again."
        ),
    },
}

_GENERIC = {
    "title": "Request declined",
    "meaning": "The game declined {doing}.",
    "fix": (
        "This usually clears up on its own - if it keeps happening, export your logs "
        "(Logs page → Export) and report it."
    ),
}


def describe_endpoint(ep):
    """Human name for what an endpoint does ('starting the career', ...)."""
    ep = str(ep or "")
    for frag, label in _ACTIVITY:
        if frag in ep:
            return label
    return "a request"


def _entry(rc):
    try:
        rc = int(rc)
    except (TypeError, ValueError):
        rc = 0
    return rc, ERROR_REFERENCE.get(rc, _GENERIC)


def explain_api_error(rc, ep=None):
    """One friendly line for a live game API error: what happened + what to do.

    Always returns a string (unknown codes get the generic entry). The code
    stays in the line so support can match reports to the DEBUG detail in
    exported logs.
    """
    rc, entry = _entry(rc)
    meaning = entry["meaning"].replace("{doing}", describe_endpoint(ep))
    return f"{meaning} (code {rc}) Fix: {entry['fix']}"


# Matches the technical message uma_api.client raises ("API error 102 on
# single_mode/start: ..."), which also ends up in runner last_error fields.
_TECH_ERR_RE = re.compile(r"API error (\d+) on ([^\s:]+)")


def friendly_error_text(text):
    """Rewrite a technical error string for player-facing surfaces (Discord
    webhooks, status lines). If it contains the client's "API error {rc} on
    {ep}" pattern, return the friendly explanation; otherwise return the text
    unchanged."""
    m = _TECH_ERR_RE.search(str(text or ""))
    if not m:
        return text
    return explain_api_error(m.group(1), m.group(2))


def help_reference():
    """The Help page payload: every known code with player-facing wording
    (generic phrasing - no live endpoint context)."""
    out = []
    for rc in sorted(ERROR_REFERENCE):
        entry = ERROR_REFERENCE[rc]
        out.append({
            "code": rc,
            "title": entry["title"],
            "meaning": entry["meaning"].replace("{doing}", "this action"),
            "fix": entry["fix"],
        })
    return out


# Common issues that AREN'T a specific game error code - the questions players
# actually ask. Each is (question, answer). Keep answers concrete and short.
HELP_TOPICS = [
    (
        "Icarus won't let me sign in",
        "Make sure the Umamusume game (Steam) is installed and running before your "
        "first login, and that Steam itself is signed in. On the very first launch, "
        "Windows SmartScreen may block the app - click More info then Run anyway. If "
        "login still fails, close the game and Icarus and try once more.",
    ),
    (
        "Windows says \"unknown publisher\" / won't open the app",
        "Icarus is code-signed as \"Icarus Network\", but not through a paid "
        "certificate authority, so Windows doesn't recognize it yet. Click More info "
        "on the SmartScreen prompt, then Run anyway. This is expected and safe.",
    ),
    (
        "My careers end early with a low grade",
        "That used to happen when a fan goal was missed. Icarus now races in the final "
        "turns to hit fan goals automatically. If it still ends early, check the Logs "
        "page - the reason is written there in plain language.",
    ),
    (
        "A career won't start (error 102 / \"account state changed\")",
        "Usually a career is already active, or a selected parent uma is gone or a "
        "borrowed parent hit its daily limit. Refresh the dashboard, re-select your "
        "parents, and resume or delete any active career. With a borrowed parent, set "
        "a backup own parent so Icarus can swap it in automatically.",
    ),
    (
        "My TP or RP is empty",
        "Use the + Refill TP button in the top bar to top up TP with recovery drinks "
        "or carats. RP (race points) refills in the game itself for now.",
    ),
    (
        "The bot looks stuck or isn't doing anything",
        "Open the Logs page - if it's waiting (server busy, a break, or between runs) "
        "it will say so. Public builds pace every action at 1.5-3.0 seconds to look "
        "human, so a full career takes several minutes. If it's truly frozen, Stop and "
        "Start it again.",
    ),
    (
        "An update won't apply",
        "Use the one-click Update Icarus.bat in your Icarus folder, or the Update Now "
        "button on the dashboard. Both keep your login, presets, config and data. If a "
        "file is locked, fully close Icarus first, then run the updater.",
    ),
    (
        "Is this safe? Can I get banned?",
        "Automating the game is against the game's Terms of Service and carries real "
        "account risk. Icarus paces actions to look human and offers breaks, but "
        "nothing makes automation undetectable. Use in moderation, at your own risk.",
    ),
]

# Short glossary of terms the logs and pages use.
GLOSSARY = [
    ("TP", "Training Points - spent to start a career. Refill with drinks or carats."),
    ("RP", "Race Points - spent on daily/Team Trials races."),
    ("Carats", "The game's premium currency (jewels). Can buy TP recovery."),
    ("Manes / Gold", "The game's soft currency, earned from races and events."),
    ("Alarm Clocks", "Let you retry a race you didn't win. Icarus can use them if you allow it."),
    ("URA", "URA Finals - the standard career scenario."),
    ("MANT", "Make a New Track - the race-heavy career scenario."),
    ("Unity Cup", "The team-based career scenario (Aoharu-style)."),
    ("Grand Live", "The Grand Concert career scenario (activates fully once live on global)."),
    ("Veterans", "Your stored trained umas - usable as inheritance parents."),
    ("Fan goals", "Career objectives to reach a fan count by a certain turn."),
    ("Borrowed parent", "An inheritance parent rented from another player (limited uses per day)."),
]


def help_topics():
    return [{"title": q, "body": a} for q, a in HELP_TOPICS]


def help_glossary():
    return [{"term": t, "definition": d} for t, d in GLOSSARY]
