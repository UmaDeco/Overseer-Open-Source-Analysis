import re
from pathlib import Path

# Heaven and Hachimi are NOT redistributed in this repo — clone them yourself
# and point these at the clones.
HEAVEN = Path("../Heaven-clone")      # https://github.com/Nighty3333/Heaven-Internal-Public-Version-
HACHIMI = Path("../Hachimi-clone")

STR_RE = re.compile(r'"([^"]{8,})"')

def extract_strings(root):
    out = {}
    for p in root.rglob("*.rs"):
        if "vendor" in str(p):
            continue
        try:
            text = p.read_text(encoding="utf-8", errors="ignore")
        except Exception:
            continue
        for m in STR_RE.finditer(text):
            s = m.group(1)
            out.setdefault(s, []).append(str(p.relative_to(root)))
    return out

h_strings = extract_strings(HEAVEN)
c_strings = extract_strings(HACHIMI)

common = sorted(set(h_strings) & set(c_strings))
print(f"Heaven unique strings (len>=8): {len(h_strings)}")
print(f"Hachimi unique strings (len>=8): {len(c_strings)}")
print(f"Shared exact strings: {len(common)}")
print(f"% of Heaven's strings also in Hachimi: {100.0*len(common)/max(1,len(h_strings)):.2f}%")
print()
for s in common:
    print(repr(s), "  <-  ", h_strings[s][:2], "  ==  ", c_strings[s][:2])
