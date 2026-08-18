from pathlib import Path

# Overseer's source is included in this repo at the root.
OVERSEER = Path(__file__).resolve().parents[2]
# Heaven, Hachimi, and umamusume-sweepy are NOT redistributed in this repo (they're each
# someone else's code) — clone them yourself and point these at the clones.
HEAVEN = Path("../Heaven-clone/native")      # https://github.com/Nighty3333/Heaven-Internal-Public-Version-
HACHIMI = Path("../Hachimi-clone")
SWEEPY = Path("../umamusume-sweepy-clone")           # https://github.com/SweepTosher/umamusume-sweepy

def norm(l): return l.strip()
def trivial(l): return len(l) < 12 or l in ("}", "};", "{")

def load_lines(p):
    try:
        text = p.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return []
    return [norm(l) for l in text.splitlines() if norm(l) and not trivial(norm(l))]

def collect(root, exts):
    excl = {"vendor", ".git", "target", "node_modules", "__pycache__", "pyembed"}
    return [p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in exts and not any(x in excl for x in p.parts)]

# Build separate indexes so we know WHICH source matched
heaven_idx = set()
for p in collect(HEAVEN, {".rs"}):
    heaven_idx.update(load_lines(p))
hachimi_idx = set()
for p in collect(HACHIMI, {".rs"}):
    hachimi_idx.update(load_lines(p))
sweepy_idx = set()
for p in collect(SWEEPY, {".py",".js",".css",".html",".ts",".tsx"}):
    sweepy_idx.update(load_lines(p))

# Feature groups: name -> list of relative paths (from OVERSEER root)
FEATURES = {
    "Translation / localization engine": [
        "native/src/loc_db.rs","native/src/loc_font.rs","native/src/loc_settext.rs","native/src/loc_story.rs",
        "native/src/loc_text.rs","native/src/loc_texture.rs","native/src/loc_ui.rs","native/src/localize.rs",
        "native/src/glossary.rs","native/src/mtl.rs","native/src/nllb.rs","native/src/template.rs",
        "native/src/plurals.rs","native/src/sql.rs","native/src/wrap.rs",
    ],
    "Skip & speed": [
        "native/src/skip/mod.rs","native/src/skip/event.rs","native/src/skip/inspire.rs","native/src/skip/result.rs",
        "native/src/skip/rival.rs","native/src/skip/shop.rs","native/src/skip/skill.rs","native/src/skip/train.rs",
        "native/src/ui_tempo.rs","native/src/followers.rs",
    ],
    "Career tracking & guidance (advisor)": [
        "advisor/career_bot/ai_dataset.py","advisor/career_bot/error_help.py","advisor/career_bot/events.py",
        "advisor/career_bot/master_data.py","advisor/career_bot/presets.py","advisor/career_bot/races.py",
        "advisor/career_bot/runner.py","advisor/uma_sidecar/__init__.py","advisor/uma_sidecar/__main__.py",
        "native/src/career.rs","native/src/event_audit.rs","native/src/event_reveal.rs","native/src/race_field.rs",
        "native/src/race_reveal.rs",
    ],
    "Legacy & inheritance (affinity)": [
        "native/src/affinity.rs","native/src/legacy.rs",
    ],
    "Team Trials": [
        "native/src/hunter.rs",
    ],
    "Performance & visuals": [
        "native/src/performance/cyspring.rs","native/src/performance/display.rs","native/src/performance/fps.rs",
        "native/src/performance/graphics.rs","native/src/performance/mod.rs",
    ],
    "Race telemetry / free camera": [
        "native/src/freecam.rs","native/src/race.rs","native/src/race_director.rs","native/src/race_export.rs",
        "native/src/uma_bridge.rs","native/src/umas.rs",
    ],
    "Custom title intro": [
        "native/src/intro_player.rs","native/src/audio.rs","native/src/bgm.rs",
    ],
    "Self-updater": [
        "native/src/selfupdate.rs","native/src/update.rs",
    ],
    "Web dashboard (control panel)": [
        "native/src/webui.rs","native/src/web/index.html","native/src/web/overseer.css","native/src/webhook.rs",
        "native/src/watchdog.rs","native/src/crashlog.rs","native/src/exctrace.rs","native/src/settings.rs",
    ],
    "Core overlay UI framework": [
        "native/src/overlay.rs","native/src/menu_model.rs","native/src/padder.rs","native/src/ui_input.rs",
    ],
    "IL2CPP hooking / plugin-SDK core": [
        "native/src/il2cpp.rs","native/src/il2cpp_json.rs","native/src/htt.rs","native/src/htt_il2cpp.rs",
        "native/src/tt_il2cpp.rs","native/src/hooks.rs","native/src/response_hook.rs","native/src/http.rs",
        "native/src/ipc.rs","native/src/overseer_compat/il2cpp_api.rs","native/src/overseer_compat/init.rs",
        "native/src/overseer_compat/interceptor.rs","native/src/overseer_compat/mod.rs",
        "native/src/overseer_compat/services.rs","native/src/overseer_compat/vtable.rs",
        "native/src/boot.rs","native/src/lib.rs","native/src/paths.rs","native/src/reset.rs",
        "native/src/mainthread.rs","native/src/runtime.rs","native/src/startup_probe.rs","native/src/diag.rs",
        "native/src/arbiter.rs","native/src/data.rs","native/src/clipboard.rs","native/src/names.rs",
        "native/src/friendlyplugins.rs","native/src/loadprof.rs","native/src/sidecar.rs","native/src/proxy.rs",
        "native/src/cvd.rs","native/src/guard.rs","native/src/msgpack.rs","native/src/tools/log.rs",
        "native/src/tools/mod.rs","native/src/tools/time.rs","native/src/assets/bindings.rs",
        "native/src/assets/mod.rs","native/src/assets/texture.rs",
    ],
}

results = []
for feat, paths in FEATURES.items():
    total = 0
    m_heaven = 0
    m_hachimi = 0
    m_sweepy = 0
    m_any = 0
    missing = []
    for rel in paths:
        p = OVERSEER / rel
        if not p.exists():
            missing.append(rel)
            continue
        lines = load_lines(p)
        total += len(lines)
        for l in lines:
            in_h = l in heaven_idx
            in_c = l in hachimi_idx
            in_u = l in sweepy_idx
            if in_h: m_heaven += 1
            if in_c: m_hachimi += 1
            if in_u: m_sweepy += 1
            if in_h or in_c or in_u: m_any += 1
    if missing:
        print("MISSING FILES:", missing)
    results.append((feat, total, m_any, m_heaven, m_hachimi, m_sweepy))

print(f"{'Feature':<38} {'Lines':>7} {'Copied':>7} {'%':>6}  {'Heaven%':>8} {'Hachimi%':>9} {'sweepy%':>9}")
grand_total = 0
grand_copied = 0
for feat, total, m_any, m_h, m_c, m_u in results:
    if total == 0:
        continue
    grand_total += total
    grand_copied += m_any
    pct = 100.0*m_any/total
    ph = 100.0*m_h/total
    pc = 100.0*m_c/total
    pu = 100.0*m_u/total
    print(f"{feat:<38} {total:>7} {m_any:>7} {pct:>5.1f}%  {ph:>7.1f}% {pc:>8.1f}% {pu:>8.1f}%")

print(f"\nGRAND TOTAL (these feature groups only): {grand_copied}/{grand_total} = {100.0*grand_copied/grand_total:.2f}%")
