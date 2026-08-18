"""Fetch the higher-quality NLLB-200 1.3B CTranslate2 model into <repo>/nllb-1.3b-model (~1.4 GB).

Tries both known mirrors of the int8 conversion. The destination comes from this file's own location,
so it lands correctly in any clone on any machine; install.ps1 defaults -HqModelSrc to the same path.
"""

import os

from huggingface_hub import snapshot_download

DEST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nllb-1.3b-model")

for repo in ["OpenNMT/nllb-200-distilled-1.3B-ct2-int8","JustFrederik/nllb-200-distilled-1.3B-ct2-int8"]:
    try:
        p = snapshot_download(repo_id=repo,
            local_dir=DEST,
            allow_patterns=["model.bin","config.json","shared_vocabulary*","tokenizer.json","special_tokens_map.json","tokenizer_config.json"])
        print("DONE via", repo, "->", p); break
    except Exception as e:
        print("FAILED", repo, str(e)[:120])
