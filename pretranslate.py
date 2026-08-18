"""Bulk pre-translation of the game's text corpus into the MTL cache.

Reads Umamusume's own text DB (master.mdb → text_data), NLLB-translates every useful string offline
(same 600M model the DLL uses), and merges the results into the per-language mtl.json cache. The DLL
then auto-loads it at launch, so skills / UI / menus / missions are translated INSTANTLY from day one
— no on-the-fly work. Proper names (names.json), junk, placeholders and rich-text tags are handled
exactly like the in-DLL path so the cached entries match what the DLL would produce.

Run with the GAME CLOSED (so the DLL isn't also writing mtl.json):
    python pretranslate.py            # French (default)
    python pretranslate.py de         # German, etc.
"""
import json
import os
import re
import sqlite3
import sys
import time

import ctranslate2
from tokenizers import Tokenizer

# ── config ──────────────────────────────────────────────────────────────────────────────────────
LANG = sys.argv[1] if len(sys.argv) > 1 else "fr"
FLORES = {
    "es": "spa_Latn", "fr": "fra_Latn", "de": "deu_Latn", "pt": "por_Latn", "it": "ita_Latn",
    "ru": "rus_Cyrl", "ja": "jpn_Jpan", "zh": "zho_Hans", "ko": "kor_Hang", "nl": "nld_Latn",
    "pl": "pol_Latn", "tr": "tur_Latn", "ar": "arb_Arab", "hi": "hin_Deva", "id": "ind_Latn",
    "vi": "vie_Latn", "uk": "ukr_Cyrl", "cs": "ces_Latn", "sv": "swe_Latn", "ro": "ron_Latn",
    "th": "tha_Thai", "tl": "tgl_Latn", "ms": "zsm_Latn", "lo": "lao_Laoo",
}
TGT = FLORES.get(LANG)
if not TGT:
    print(f"unsupported language {LANG}")
    sys.exit(1)

OVERSEER = os.path.dirname(os.path.abspath(__file__))
MODEL = os.path.join(OVERSEER, "nllb-1.3b-model" if os.path.exists(os.path.join(OVERSEER, "nllb-1.3b-model", "model.bin")) and False else "nllb-model")
PLUGINS = r"C:\Program Files (x86)\Steam\steamapps\common\UmamusumePrettyDerby\UmamusumePrettyDerby_Data\Plugins\x86_64"
OUT = os.path.join(PLUGINS, "glossary", LANG, "mtl.json")
NAMES_JSON = os.path.join(PLUGINS, "glossary", "names.json")
MDB_SRCS = [
    os.path.expandvars(r"%USERPROFILE%\AppData\LocalLow\Cygames\Umamusume\master\master.mdb"),
    r"C:\Program Files (x86)\Steam\steamapps\common\UmamusumePrettyDerby\UmamusumePrettyDerby_Data\Persistent\master\master.mdb",
]

BATCH = 64
INTRA_THREADS = int(os.environ.get("PRETL_THREADS", "6"))


def log(m):
    print(m, flush=True)


# ── filtering (mirrors the DLL's is_junk / digit_heavy / name protection) ───────────────────────
def load_names():
    try:
        return set(n.strip() for n in json.load(open(NAMES_JSON, encoding="utf-8")))
    except Exception:
        return set()


def is_junk(t):
    t = t.strip()
    if len(t) < 3 or len(t) > 400:
        return True
    if t.startswith("#"):
        return True
    if not re.search(r"[A-Za-z]{3,}", t):
        return True
    d = sum(ch.isdigit() for ch in t)
    l = sum(ch.isalpha() for ch in t)
    if d and d * 2 >= l:
        return True
    return False


TAG_RE = re.compile(r"<[^>]*>")


def strip_tags(s):
    # Strip rich-text tags (color/size/b) like the DLL — they mangle NMT; content survives.
    return re.sub(r"\s+", " ", TAG_RE.sub(" ", s)).strip()


def clean_out(txt):
    for sp in ("<unk>", "</s>", "<pad>", "<s>"):
        txt = txt.replace(sp, "")
    return txt.strip()


def main():
    mdb = next((p for p in MDB_SRCS if os.path.exists(p)), None)
    if not mdb:
        log("master.mdb not found")
        return
    log(f"[pretl] lang={LANG} tgt={TGT} model={os.path.basename(MODEL)} mdb={mdb}")

    db = sqlite3.connect(mdb)
    texts = [r[0] for r in db.execute(
        "SELECT DISTINCT text FROM text_data WHERE text IS NOT NULL AND text != ''").fetchall()]
    names = load_names()

    # existing cache (merge — never lose learned/played translations)
    cache = {}
    if os.path.exists(OUT):
        try:
            cache = json.load(open(OUT, encoding="utf-8"))
        except Exception:
            cache = {}
    log(f"[pretl] {len(texts)} distinct texts, {len(cache)} already cached")

    todo = [t for t in texts if t not in cache and t not in names and not is_junk(t)]
    log(f"[pretl] {len(todo)} to translate")
    if not todo:
        log("[pretl] nothing to do")
        return

    tok = Tokenizer.from_file(os.path.join(MODEL, "tokenizer.json"))
    translator = ctranslate2.Translator(MODEL, device="cpu", inter_threads=1, intra_threads=INTRA_THREADS)
    log(f"[pretl] model loaded (intra_threads={INTRA_THREADS}); translating…")

    t0 = time.time()
    done = 0
    added = 0
    for i in range(0, len(todo), BATCH):
        batch = todo[i:i + BATCH]
        encs = [tok.encode(strip_tags(s)).tokens for s in batch]
        res = translator.translate_batch(
            encs, target_prefix=[[TGT]] * len(encs), beam_size=1, max_batch_size=BATCH)
        for raw, r in zip(batch, res):
            toks = r.hypotheses[0]
            if toks and toks[0] == TGT:
                toks = toks[1:]
            ids = [tok.token_to_id(x) for x in toks if tok.token_to_id(x) is not None]
            txt = clean_out(tok.decode(ids, skip_special_tokens=True))
            if txt and txt != raw:
                cache[raw] = txt
                added += 1
        done += len(batch)
        if (i // BATCH) % 20 == 0:
            rate = done / max(0.1, time.time() - t0)
            eta = (len(todo) - done) / max(0.1, rate)
            log(f"[pretl] {done}/{len(todo)}  (+{added})  {rate:.0f}/s  ETA {eta/60:.1f}m")
            # periodic checkpoint write
            _write(cache)

    _write(cache)
    log(f"[pretl] DONE: +{added} translations, cache now {len(cache)} entries, {time.time()-t0:.0f}s")


def _write(cache):
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    tmp = OUT + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(cache, f, ensure_ascii=False, indent=1)
    os.replace(tmp, OUT)


if __name__ == "__main__":
    main()
