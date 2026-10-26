#!/usr/bin/env python3
"""Reproduce the frozen r1 vocabulary and its tokenizer measurements.

Run with: uv run --no-project --with tiktoken==0.14.0 scripts/generate-tag-words.py --check --measure
Without --check, print the dictionary to stdout. Nothing is downloaded at runtime
by tilth. Tokenizer code/data provenance: https://github.com/openai/tiktoken
MIT notice: scripts/licenses/tiktoken.txt. Entries may be word fragments.
"""
import argparse
import base64
import hashlib
import importlib.metadata
import json
from pathlib import Path
import random
import re
import statistics
import sys
import tiktoken

SHA256 = "e015e58627c364ee033b14f80bcfe6d609c5864bf8cc3ba159a4ba7bffa4f1c6"
ENCODINGS = ("cl100k_base", "o200k_base")

def candidates(encoding):
    result = set()
    for token in range(encoding.n_vocab):
        try:
            value = encoding.decode_single_token_bytes(token)
        except KeyError:
            continue
        if re.fullmatch(rb" [a-z]{3,12}", value):
            result.add(value.decode("ascii")[1:])
    return result

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--measure", action="store_true")
    args = parser.parse_args()
    if importlib.metadata.version("tiktoken") != "0.14.0":
        parser.error("reproduction requires tiktoken==0.14.0")
    encodings = [tiktoken.get_encoding(name) for name in ENCODINGS]
    words = sorted(set.intersection(*(candidates(e) for e in encodings)))[:8192]
    assert len(words) == 8192
    data = ("\n".join(words) + "\n").encode("ascii")
    assert hashlib.sha256(data).hexdigest() == SHA256, "vocabulary changed; do not silently change r1"
    if args.check:
        path = Path(__file__).resolve().parents[1] / "src/read/tag_words.txt"
        assert path.read_bytes() == data, "checked-in dictionary differs"
        print("r1 vocabulary verified: 8192 entries; sha256=" + SHA256)
    elif not args.measure:
        sys.stdout.write(data.decode("ascii"))
    if args.measure:
        rng = random.Random(20261025)
        values = [rng.getrandbits(65) for _ in range(10000)]
        forms = {
            "five-word-65": ["r1:" + "".join(" " + words[(n >> shift) & 8191] for shift in (52,39,26,13,0)) for n in values],
            "base64url-65": ["r1:" + base64.urlsafe_b64encode(n.to_bytes(9,"big")).decode().rstrip("=") for n in values],
        }
        for encoding in encodings:
            for form, texts in forms.items():
                counts = sorted(len(encoding.encode(text)) for text in texts)
                print(json.dumps(dict(encoding=encoding.name, form=form, samples=len(counts), seed=20261025,
                    mean=statistics.mean(counts), minimum=counts[0], p95=counts[9500], maximum=counts[-1])))

if __name__ == "__main__":
    main()
