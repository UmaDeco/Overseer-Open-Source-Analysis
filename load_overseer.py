"""
Quick test loader for the unified Overseer DLL (Phase 1 web UI).

Attaches to the running game via Frida and Module.load()s the freshly-built
overseer_overlay.dll. On DLL attach, hudhook installs its D3D11 hook and — on the
next rendered frame — runs new_with_engine(), which starts:
  - boot::spawn()  (the IL2CPP engine: FPS, graphics, SuperSkip, race reader, ...)
  - webui::spawn() (the Overseer SPA on http://127.0.0.1:1620)

IMPORTANT: the DLL's web server needs port 1620. Stop the Overseer Python server
first (that also serves 1620) or the DLL can't bind it and you'll see the old UI.

Run:  py -3 load_overseer.py      (or your system Python that has `frida`)
"""

import json
import os
import sys
import time
import urllib.request

PROC = "UmamusumePrettyDerby.exe"
# Resolved from this file's own location so any clone works — Frida's Module.load() needs an
# absolute path, but it does not need to be *this* machine's absolute path.
DLL = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "native", "target", "release", "overseer_overlay.dll",
)
URL = "http://127.0.0.1:1620/api/advisor/state"


def port_1620_owner():
    """Return a short hint about who currently answers on 1620, or None."""
    try:
        with urllib.request.urlopen(URL, timeout=1.0) as r:
            body = r.read(400).decode("utf-8", "replace")
            return body
    except Exception:
        return None


def main():
    import frida  # imported here so a clear message shows if it's missing

    # Pre-flight: is 1620 already taken (likely the Overseer Python server)?
    pre = port_1620_owner()
    if pre is not None:
        print("!! Port 1620 is ALREADY answering before we load the DLL.")
        print("   That's almost certainly the Overseer Python server. STOP it first")
        print("   (POST http://127.0.0.1:1620/api/shutdown, or close that process),")
        print("   otherwise the DLL cannot bind 1620 and you'll see the old UI.")
        print("   Current 1620 response snippet:", pre[:160])
        ans = input("   Continue anyway? [y/N] ").strip().lower()
        if ans != "y":
            print("Aborted.")
            return 1

    print(f"Attaching to {PROC} ...")
    try:
        session = frida.attach(PROC)
    except Exception as e:
        print(f"!! Could not attach to {PROC}: {e}")
        print("   Is the game running? Is it the exact process name above?")
        return 1

    js = "try { Module.load(%s); send('loaded'); }" \
         " catch (e) { send('error: ' + e.message); }" % json.dumps(DLL)
    script = session.create_script(js)

    result = {"msg": None}

    def on_message(message, data):
        result["msg"] = message
        print("frida:", message)

    script.on("message", on_message)
    print(f"Loading DLL: {DLL}")
    script.load()

    # Give hudhook a couple frames + the web server a moment to bind.
    print("Waiting for the in-process web server to come up on 1620 ...")
    ok = False
    for i in range(20):  # up to ~10s
        time.sleep(0.5)
        body = port_1620_owner()
        if body is not None:
            try:
                j = json.loads(body)
                cap = j.get("capture") or {}
                mod = cap.get("mod") or {}
                print(f"  [{i*0.5:.1f}s] 1620 is UP. hooked={cap.get('hooked')} "
                      f"installed={mod.get('installed')} targetFps={mod.get('targetFps')}")
                ok = True
                # keep polling a bit so you can watch 'installed' flip true as boot finishes
                if cap.get("hooked"):
                    break
            except Exception:
                print(f"  [{i*0.5:.1f}s] 1620 answered but not JSON yet:", body[:80])
    if ok:
        print("\nOK — the DLL is serving the web UI. Open http://127.0.0.1:1620")
        print("  • 'attached' in the top-right turns green once the engine boots (~5-8s).")
        print("  • Graphics & FPS page now drives Overseer's real FPS unlock / quality.")
        print("  • Translation page shows 'not attached' — that's Phase 2 (next).")
        return 0
    print("\n!! 1620 never came up. Check the Overseer log for a bind error")
    print("   (overseer-logs next to the DLL). Most likely port 1620 was still taken.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
