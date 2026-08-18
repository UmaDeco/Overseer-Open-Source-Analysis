import hashlib
from pathlib import Path

# The Umamusume-Icarus GitHub repo ships no source (see docs/COMPARISON-vs-umamusume-sweepy.md);
# this compares Icarus's own distributed application bundle (the `public/` folder every
# install writes next to the compiled executable) against umamusume-sweepy's public repo.
ICARUS_PUBLIC = Path("../Icarus-install/public")        # a local Icarus installation's public/ folder
SWEEPY_PUBLIC = Path("../umamusume-sweepy-clone/public")  # https://github.com/SweepTosher/umamusume-sweepy

def norm(l): return l.strip()
def trivial(l): return len(l) < 12 or l in ("}", "};", "{")

def load_lines(p):
    try:
        text = p.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return []
    return [norm(l) for l in text.splitlines() if norm(l) and not trivial(norm(l))]

def md5(p):
    return hashlib.md5(p.read_bytes()).hexdigest()

print("=== Byte-identical asset check ===")
for fname in ["broom.png", "sweep.png"]:
    a, b = ICARUS_PUBLIC / fname, SWEEPY_PUBLIC / fname
    if a.exists() and b.exists():
        ha, hb = md5(a), md5(b)
        print(f"{fname}: Icarus={ha}  sweepy={hb}  {'IDENTICAL' if ha == hb else 'differs'}")

print("\n=== Line-overlap check (CSS/HTML) ===")
for fname in ["styles.css", "index.html"]:
    ilines = load_lines(ICARUS_PUBLIC / fname)
    slines = set(load_lines(SWEEPY_PUBLIC / fname))
    if not ilines:
        continue
    matched = sum(1 for l in ilines if l in slines)
    print(f"{fname}: {matched}/{len(ilines)} = {100*matched/len(ilines):.1f}% of Icarus's lines found verbatim in sweepy's version")

print("\n=== races/ folder ===")
ir, sr = ICARUS_PUBLIC / "races", SWEEPY_PUBLIC / "races"
if ir.exists() and sr.exists():
    ni, ns = len(list(ir.glob("*.png"))), len(list(sr.glob("*.png")))
    print(f"Icarus: {ni} files, sweepy: {ns} files")
