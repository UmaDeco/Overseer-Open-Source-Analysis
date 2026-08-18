# Raw output notes

Most files here are the direct output of the scripts in `../scripts/`, run against a clone of each reference project sitting next to a clone of this repo (see the path placeholders at the top of each script).

`overseer_vs_SweepTosher-Icarus.txt` is the exception: [SweepTosher/Icarus](https://github.com/SweepTosher/Icarus) has a `node_modules/` tree with paths too long for a normal Windows checkout, so the 5 tracked source files (`client.py`, `crypto.py`, `main.py`, `steam_auth.py`, `ticket_gen.js`) were fetched individually via `gh api repos/SweepTosher/Icarus/contents/<file>` instead of a full clone before running the comparison script against them.
