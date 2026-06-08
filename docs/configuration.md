# Configuration

`.greplm/config.toml` (created on first index) controls the walk and indexing:

```toml
include = []                       # glob whitelist (empty = all text files)
exclude = ["**/.git/**", "**/node_modules/**", "**/target/**", "**/.greplm/**"]
max_file_size = 4194304            # skip files larger than this (bytes)
respect_gitignore = true
index_hidden = false
backend = "auto"                   # auto | rayon | io-uring
merge_threshold = 16               # auto-compact once segments exceed this
```
