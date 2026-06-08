# Code intelligence

Beyond search, greplm answers the questions an agent actually asks before editing: *who calls
this, what does it call, what breaks if I change it, where is this defined, and give me exactly
the code for this task.*

```console
$ greplm callers references --limit 3
cmd_refs -> references  crates/greplm-cli/src/main.rs:896:17
dispatch -> references  crates/greplm-core/src/daemon.rs:263:32
definition -> references  crates/greplm-core/src/search.rs:874:24

$ greplm impact add_doc --depth 2 --limit 4
d0  function   add_doc                  crates/greplm-core/src/segment.rs:132-165
d1  function   index_full               crates/greplm-core/src/indexer.rs:187-262
d1  function   index_incremental        crates/greplm-core/src/indexer.rs:265-454
d2  function   compact                  crates/greplm-core/src/indexer.rs:459-467

$ greplm def crates/greplm-cli/src/main.rs 896 57
* function   references               crates/greplm-core/src/search.rs:543-551

$ greplm ast 'fn $NAME() {}' --lang rust --limit 1
crates/greplm-cli/src/agent.rs:86-88:     fn dest(&self, scope_root: &Path) -> PathBuf { NAME=dest

$ greplm pack "how does incremental indexing work" --budget 4000
# context pack for: how does incremental indexing work
# 15 items, ~3489/4000 tokens
## function index_incremental (match)  crates/greplm-core/src/indexer.rs:265-454  [17.9]
  ...
```

## Notes

- The `*` in `def` marks an unambiguous resolution; otherwise candidates are ranked and the agent
  sees the alternatives.
- `impact`, `callers`, and `callees` resolve by name, so treat them as a fast, high-recall guide
  rather than a proof.
- `ast` accepts either a full tree-sitter query S-expression (with `@captures` and `#eq?`/`#match?`
  predicates) or the friendly `$NAME` form.

See the [commands reference](commands.md#code-intelligence) for full flag lists.
