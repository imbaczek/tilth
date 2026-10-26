# Conditional read views

Explicit `tilth read` and MCP `tilth_read` can return a caller-carried validator.
Both remain stateless: the caller keeps the view and tag. Issuance is **opt-in**.
The session-history replay did not support automatic emission as a default:
most recovered reads were one-off, and tag overhead outweighed recovered hits.

```bash
tilth read src/parser.rs --symbol parse_query --tag always
tilth read src/parser.rs --symbol parse_query --if-tag 'r1: ...five words from the first response...'
```

A match returns `View unchanged.` with success (CLI exit 0; MCP not an error).
A miss returns the current view and a replacement tag. Omitting `--if-tag`
always returns content. `--full` chooses a view, not an unconditional refresh.
Implicit reads such as `tilth src/parser.rs` do not issue tags.

## Options and transport

- `--tag always`: issue a tag, even for a tiny view.
- `--tag never`: no tag; the default. Conflicts with `--if-tag` when explicit.
- `--tag auto`: experimental issuance at >=1000 **estimated tokens of the final
  view**, excluding tag metadata. This threshold is policy, not an optimum
  established by the historical sample.
- `--if-tag TAG`: validate a previous tag; implies issuance even below the floor.
- `--json`: emit `status`, optional `tag`/`tag_scope`, and `output` on content
  responses. An unchanged JSON response has no `output` field.

MCP takes `tag: "always"` and `if_tag: "r1: ..."` alongside existing read arguments.
Its text content shows fresh tags even on clients without structured-output
support. `structuredContent` also contains status, tag and `tag_scope: "view"`.
The source body is not duplicated in structured metadata. JSON CLI output keeps
its tag separate from the body. Framing/token overhead varies by transport.

## Equality means the same returned view

One tag validates the whole final budgeted response, including all requested
files/sections/symbols, source locations, edit anchors, headers, related hints,
pagination and omission notices. It does **not** validate hidden content:

- A signature-only view can match after a body-only edit.
- A truncated view can match after an edit beyond its retained content.
- Edits outside a selected section can leave its tag unchanged.
- Shifting source addresses or changing visible omission counts invalidates it.

The tag binds ordered resolved/display paths and selectors, effective mode,
explicit pagination, budget, edit mode and CLI/MCP navigation flavor. Equivalent
root-relative and absolute paths match when they produce the same displayed
path. Different symlink or dot spellings may produce different headers and tags;
we do not change existing path display rules solely to improve hit rates.
Emission policy and the supplied condition are excluded from identity.

Tagging reserves 32 estimated tokens of the output budget for the envelope,
identically for issuance and validation. Budgets <=32 are rejected when tagging
is requested. An `auto` read below its floor returns ordinary untagged output.
Errors, stopped batches, failed slices, missing/ambiguous symbols and detected
symbol parse errors never receive or validate a tag, even if a budget hides the
associated diagnostic. Failures reading/parsing optional related-hint inputs
also suppress tag issuance. Binary/generated summaries can be tagged as summaries. Directory listings are
not tagged because their walkers can tolerate per-entry I/O failures.

Tagged reads capture immutable bytes once per file within the request and use a
fresh parse/outline cache, so preserving an edit's mtime cannot validate stale
cache output. Common concurrent in-place edits are detected and retried once.
This is not a filesystem transaction: files can change immediately afterward,
and a batch is not an atomic repository snapshot. Captured bytes are limited to
64 MiB per request; omit tag options to use the ordinary read path for larger
inputs. Untagged full reads retain their existing mmap behavior.

Only send a condition while retaining the corresponding view. A tag is not a
handle for recovering content forgotten through context compaction. It is not a
write precondition, file checksum, authorization token or security primitive.

## Frozen r1 protocol

Hash the domain bytes `tilth/read-view/r1\0`, then each of these fields prefixed
by its unsigned 64-bit little-endian byte length:

1. Canonical JSON request identity (sorted object keys, UTF-8).
2. Final view UTF-8 bytes, before the tag envelope.

Use MurmurHash3 **x64_128**, seed 0, with the low half (`h1`) in the low 64 bits
and high half (`h2`) in the high 64 bits. Keep its low 65 bits. Encode five
13-bit indices, most significant group first, in the frozen ordered
`src/read/tag_words.txt` dictionary. Prefix with `r1:` and exactly one ASCII
space before each lowercase word. All five-word sequences encode valid values;
there is no padding. Unknown future versions are misses; malformed r1 tags are
argument errors. Current tags have a 256-byte input bound.

The dictionary contains 8192 alphabetic entries, including fragments, selected
as the lexicographically first entries in the intersection of single-token
leading-space spellings in tiktoken 0.14.0's `cl100k_base` and `o200k_base`.
It is compiled in; there is no tokenizer dependency at runtime. Reordering it,
changing the hash or changing identity semantics requires a new tag version.

```bash
uv run --no-project --with tiktoken==0.14.0 scripts/generate-tag-words.py --check --measure
```

The deterministic 10,000-sample measurement (seed 20261025) gives **8 tokens**
per isolated prefixed five-word tag on both tested encodings. This is not a
promise for other/future tokenizers or surrounding JSON/prose boundaries.
Tokenizer provenance: [OpenAI tiktoken](https://github.com/openai/tiktoken).
The reproducible script and its MIT notice are in `scripts/`.

With reasonably uniform hash output, each comparison of different views has
accidental-match probability about 2^-65. This is pairwise validation, not a
birthday-collision lookup across every issued tag. The hash is noncryptographic;
there is no protection against deliberately constructed collisions.
