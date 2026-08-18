"""Fetch the NLLB-200 600M CTranslate2 model into <repo>/nllb-model (~647 MB).

The destination comes from this file's own location, so it lands correctly in any clone on any
machine; install.ps1 defaults -ModelSrc to the same path.
"""

import os

from huggingface_hub import snapshot_download

DEST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "nllb-model")

p = snapshot_download(
    repo_id="JustFrederik/nllb-200-distilled-600M-ct2-int8",
    local_dir=DEST,
    allow_patterns=["model.bin","config.json","shared_vocabulary.txt","tokenizer.json","special_tokens_map.json","tokenizer_config.json"],
)
print("DONE:", p)
