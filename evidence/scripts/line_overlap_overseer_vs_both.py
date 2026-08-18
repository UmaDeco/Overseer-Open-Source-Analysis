import re
from pathlib import Path

# Overseer's source is included in this repo at the root.
OVERSEER = Path(__file__).resolve().parents[2] / "native"
# Heaven and Hachimi are NOT redistributed in this repo — clone them yourself
# and point these at the clones.
HEAVEN = Path("../Heaven-clone/native")      # https://github.com/Nighty3333/Heaven-Internal-Public-Version-
HACHIMI = Path("../Hachimi-clone")

def norm_line(l):
    return l.strip()

def is_trivial(l):
    if len(l) < 12:
        return True
    if l in ("}", "};", "{"):
        return True
    return False

def load_lines(path):
    try:
        text = path.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return []
    lines = []
    for l in text.splitlines():
        n = norm_line(l)
        if n and not is_trivial(n):
            lines.append(n)
    return lines

def collect_rs_files(root):
    return [p for p in root.rglob("*.rs") if "vendor" not in str(p)]

def build_index(root, files):
    idx = {}
    for p in files:
        for l in load_lines(p):
            idx.setdefault(l, []).append(str(p.relative_to(root)))
    return idx

overseer_files = collect_rs_files(OVERSEER)
heaven_files = collect_rs_files(HEAVEN)
hachimi_files = collect_rs_files(HACHIMI)

print(f"Overseer .rs files: {len(overseer_files)}")
print(f"Heaven .rs files:   {len(heaven_files)}")
print(f"Hachimi .rs files:  {len(hachimi_files)}")

heaven_idx = build_index(HEAVEN, heaven_files)
hachimi_idx = build_index(HACHIMI, hachimi_files)

def analyze(target_files, target_root, idx, label):
    total_lines = 0
    total_matched = 0
    results = []
    for p in target_files:
        lines = load_lines(p)
        if not lines:
            continue
        matched = 0
        match_files = {}
        for l in lines:
            if l in idx:
                matched += 1
                for f in idx[l]:
                    match_files[f] = match_files.get(f, 0) + 1
        total_lines += len(lines)
        total_matched += matched
        pct = 100.0 * matched / len(lines)
        top = sorted(match_files.items(), key=lambda x: -x[1])[:2]
        results.append((pct, matched, len(lines), str(p.relative_to(target_root)), top))
    results.sort(key=lambda x: -x[0])
    print(f"\n=== Overseer file overlap vs {label} ===")
    for pct, matched, total, name, top in results:
        if matched == 0:
            continue
        topstr = ", ".join(f"{f}({c})" for f, c in top)
        print(f"{pct:5.1f}%  ({matched:4d}/{total:4d})  {name}   <- {topstr}")
    print(f"\n--- TOTAL vs {label}: {total_matched}/{total_lines} = {100.0*total_matched/total_lines:.2f}% ---")
    return total_matched, total_lines

m_h, t_h = analyze(overseer_files, OVERSEER, heaven_idx, "HEAVEN")
m_c, t_c = analyze(overseer_files, OVERSEER, hachimi_idx, "HACHIMI")

print("\n\n======= SUMMARY =======")
print(f"Overseer vs Heaven:  {100.0*m_h/t_h:.2f}% of Overseer's lines found verbatim in Heaven")
print(f"Overseer vs Hachimi: {100.0*m_c/t_c:.2f}% of Overseer's lines found verbatim in Hachimi")

# filename overlap
oset = {str(p.relative_to(OVERSEER)).replace("\\","/").split("/",1)[-1] for p in overseer_files}
hset = {str(p.relative_to(HEAVEN)).replace("\\","/").split("/",1)[-1] for p in heaven_files}
cset = {str(p.relative_to(HACHIMI)).replace("\\","/") for p in hachimi_files}
obase = {p.name for p in overseer_files}
hbase = {p.name for p in heaven_files}
cbase = {p.name for p in hachimi_files}
print(f"\nFilename overlap (basename): Overseer&Heaven = {len(obase & hbase)}/{len(obase)} ({100*len(obase&hbase)/len(obase):.1f}%)")
print(f"Filename overlap (basename): Overseer&Hachimi = {len(obase & cbase)}/{len(obase)} ({100*len(obase&cbase)/len(obase):.1f}%)")
print("\nShared basenames Overseer&Heaven:", sorted(obase & hbase))
