# Configuration

`.greplm/config.toml` is created by `greplm init` or on the first `greplm index`. It controls
the walk and indexing:

```toml
include = []                       # glob whitelist (empty = all text files)
exclude = ["**/.git/**", "**/node_modules/**", "**/target/**", "**/.greplm/**"]
max_file_size = 4194304            # skip files larger than this (bytes); 0 = no limit
respect_gitignore = true
index_hidden = false
index_binary = false               # index NUL-containing (binary) files, like grep -a
index_empty = false                # index zero-byte files
backend = "auto"                   # auto | rayon | io-uring
merge_threshold = 16               # auto-compact once segments exceed this
```

## Environment overrides

These `GREPLM_*` variables override the file for one-off runs (no need to edit `config.toml`):

| Variable | Effect |
|----------|--------|
| `GREPLM_MAX_FILE_SIZE` | Override `max_file_size` (bytes) |
| `GREPLM_RESPECT_GITIGNORE` | `1`/`true` or `0`/`false` |
| `GREPLM_INDEX_HIDDEN` | `1`/`true` or `0`/`false` |
| `GREPLM_INDEX_BINARY` | `1`/`true` or `0`/`false` |
| `GREPLM_INDEX_EMPTY` | `1`/`true` or `0`/`false` |
| `GREPLM_LOG` | Log level (`debug`, `info`, `warn`, …) |
| `GREPLM_NO_SAVINGS` | `1` to disable token-savings recording |
| `GREPLM_SEMANTIC_MODEL` | Path to a Model2Vec model directory (semantic search) |

The `greplm index` flags `--index-binary`, `--index-empty`, and `--max-file-size` set the
corresponding env vars for that run.
