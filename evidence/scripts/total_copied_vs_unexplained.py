import re
from pathlib import Path

# Overseer's source is included in this repo at the root.
OVERSEER = Path(__file__).resolve().parents[2]
# Heaven, Hachimi, and umamusume-sweepy are NOT redistributed in this repo — clone them
# yourself and point these at the clones.
HEAVEN = Path("../Heaven-clone/native")      # https://github.com/Nighty3333/Heaven-Internal-Public-Version-
HACHIMI = Path("../Hachimi-clone")
SWEEPY = Path("../umamusume-sweepy-clone")           # https://github.com/SweepTosher/umamusume-sweepy

OV_DIRS = ["native/src", "advisor", "launcher"]
EXTS = {".rs", ".py", ".html", ".css", ".js", ".ts", ".tsx", ".mjs"}
# "msgpack" added: advisor/msgpack/* is the vendored third-party PyPI `msgpack` package
# (docstring: "Fallback pure Python implementation of msgpack"), not Overseer's own code.
EXCLUDE = {"vendor", ".git", "target", "node_modules", "__pycache__", "pyembed", "msgpack"}

def norm(l): return l.strip()
def trivial(l): return len(l) < 12 or l in ("}", "};", "{")

def load_lines(p):
    try:
        text = p.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return []
    return [norm(l) for l in text.splitlines() if norm(l) and not trivial(norm(l))]

def collect(root, exts=EXTS):
    return [p for p in root.rglob("*") if p.is_file() and p.suffix.lower() in exts and not any(x in EXCLUDE for x in p.parts)]

union = set()
for p in collect(HEAVEN, {".rs"}):
    union.update(load_lines(p))
for p in collect(HACHIMI, {".rs"}):
    union.update(load_lines(p))
for p in collect(SWEEPY):
    union.update(load_lines(p))

ov_files = []
for d in OV_DIRS:
    ov_files += collect(OVERSEER / d)

# A "low-utility" line for the refined metric: import/module declarations, attributes,
# and raw HTML markup (native/src/web/index.html) -- present in the source but not
# representative of copied *logic*.
IMPORTLIKE = re.compile(r'^(use |mod |pub mod |pub use |extern crate |import |from .* import |#!\[|#\[)')

tot_all = matched_all = 0
tot_useful = matched_useful = 0

for p in ov_files:
    lines = load_lines(p)
    is_html = p.suffix.lower() == ".html"
    for l in lines:
        tot_all += 1
        m = l in union
        if m:
            matched_all += 1
        if not IMPORTLIKE.match(l) and not is_html:
            tot_useful += 1
            if m:
                matched_useful += 1

print(f"[excluding advisor/msgpack third-party vendor code]")
print(f"ALL non-trivial lines:    {matched_all}/{tot_all} = {100*matched_all/tot_all:.2f}% copied  |  {tot_all-matched_all} ({100*(tot_all-matched_all)/tot_all:.2f}%) no match")
print(f"USEFUL lines only (excl. imports/mod/attrs/html markup): {matched_useful}/{tot_useful} = {100*matched_useful/tot_useful:.2f}% copied  |  {tot_useful-matched_useful} ({100*(tot_useful-matched_useful)/tot_useful:.2f}%) no match")
