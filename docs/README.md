# greplm documentation

greplm is a fast, offline code search and code intelligence tool built for LLM agents. It indexes your project locally and returns compact, token-efficient results — search, symbols, call graphs, go-to-definition, AST search, git history, and context packs.

## Guides

| Guide | Description |
|-------|-------------|
| [Getting started](getting-started.md) | Install, index your project, add agent files |
| [Usage](usage.md) | Common workflows, daemon setup, semantic search |
| [Code intelligence](code-intelligence.md) | Call graph, go-to-definition, AST search, context packs |
| [Token efficiency](token-efficiency.md) | How greplm saves agent context, benchmarks |

## Reference

| Reference | Description |
|-----------|-------------|
| [Commands](commands.md) | Full CLI command reference |
| [MCP server](mcp.md) | Model Context Protocol tools and client setup |
| [Configuration](configuration.md) | `.greplm/config.toml` options |

## More

| Topic | Description |
|-------|-------------|
| [Features & comparison](features.md) | Feature list and comparison with ripgrep / LSP |
| [Build from source](getting-started.md#build-from-source) | Compile `greplm` and `greplm-mcp` locally |
| [Benchmarks](../bench/README.md) | Search and context-pack efficiency methodology |
