import re, sys
from pathlib import Path

# Overseer's source is included in this repo at the root.
OVERSEER = Path(__file__).resolve().parents[2]
EXTS = {".rs", ".py", ".html", ".css", ".js", ".ts", ".tsx", ".mjs", ".java", ".cs"}
EXCLUDE_DIRS = {"vendor", ".git", "target", "node_modules", "icons", "courseimages", "fonts"}

def norm_line(l):
    return l.strip()

def is_trivial(l):
    if len(l) < 12:
        return True
    if l in ("}", "};", "{", "):", "):{", "});"):
        return True
    return False

def load_lines(path):
    try:
        text = path.read_text(encoding="utf-8", errors="ignore")
    except Exception:
        return []
    out = []
    for l in text.splitlines():
        n = norm_line(l)
        if n and not is_trivial(n):
            out.append(n)
    return out

def collect(root):
    files = []
    for p in root.rglob("*"):
        if not p.is_file():
            continue
        if p.suffix.lower() not in EXTS:
            continue
        if any(part in EXCLUDE_DIRS for part in p.parts):
            continue
        files.append(p)
    return files

def build_index(root, files):
    idx = {}
    for p in files:
        for l in load_lines(p):
            idx.setdefault(l, []).append(str(p.relative_to(root)))
    return idx

def main(overseer_subdirs, ref_root, ref_name, min_report_pct=3.0):
    ov_files = []
    for sub in overseer_subdirs:
        ov_files += collect(OVERSEER / sub)
    ref_root = Path(ref_root)
    ref_files = collect(ref_root)

    print(f"Overseer source files considered: {len(ov_files)}")
    print(f"{ref_name} source files considered: {len(ref_files)}")

    ref_idx = build_index(ref_root, ref_files)

    total_lines = 0
    total_matched = 0
    results = []
    for p in ov_files:
        lines = load_lines(p)
        if not lines:
            continue
        matched = 0
        match_files = {}
        for l in lines:
            if l in ref_idx:
                matched += 1
                for f in ref_idx[l]:
                    match_files[f] = match_files.get(f, 0) + 1
        total_lines += len(lines)
        total_matched += matched
        pct = 100.0 * matched / len(lines)
        top = sorted(match_files.items(), key=lambda x: -x[1])[:3]
        results.append((pct, matched, len(lines), str(p.relative_to(OVERSEER)), top))

    results.sort(key=lambda x: -x[0])
    print(f"\n=== Overseer vs {ref_name}: files with >= {min_report_pct}% verbatim overlap ===")
    for pct, matched, total, name, top in results:
        if matched == 0 or pct < min_report_pct:
            continue
        topstr = ", ".join(f"{f}({c})" for f, c in top)
        print(f"{pct:5.1f}%  ({matched:4d}/{total:4d})  {name}   <- {topstr}")

    print(f"\n--- TOTAL vs {ref_name}: {total_matched}/{total_lines} = {100.0*total_matched/max(1,total_lines):.3f}% ---")

if __name__ == "__main__":
    subdirs = sys.argv[1].split(",")
    ref_root = sys.argv[2]
    ref_name = sys.argv[3]
    main(subdirs, ref_root, ref_name)
