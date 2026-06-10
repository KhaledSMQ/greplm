# greplm — real-world benchmarks

Token cost of finding code two ways, on three large real codebases:

- **baseline** — ripgrep finds the matching files, the agent reads each file *in full* (what an agent does today with grep + read).
- **greplm** — `greplm search` / `greplm pack`; the agent consumes only the compact payload (matched lines / budgeted snippets).

`tokens ~= characters / 4`, applied identically to both engines. ripgrep runs in literal mode (`-F`) so both engines look for the same string. `recall` = fraction of ripgrep's files greplm also surfaced (search runs `--max-per-file 1`, so greplm returns the same files without a hit explosion). Latency in the headline is the warm daemon — index built once, then queried hot, which is how an agent actually uses it.

## Headline

| Project | Lang | Files | Index once | Search saved | Recall | Pack saved | Warm query ms | ripgrep ms |
|---|---|---|---|---|---|---|---|---|
| React (facebook/react) | JavaScript / TypeScript | 6 723 | 2.63s | 99.7% | 100% | 97.4% | 8.4 | 106 |
| Odoo 18 (ERP) | Python / JS / XML | 41 142 | 4.12s | 99.9% | 100% | 99.3% | 15.8 | 1241 |
| Linux kernel | C | 93 362 | 26.47s | 99.9% | 100% | 98.4% | 18.8 | 1802 |

*Warm query ms = median per-query latency against the always-on daemon (the real agent-loop scenario: index built once, stays hot). ripgrep ms re-scans the whole tree on every single query.*

**Aggregate** — content search: 218.7M → 280.5k tokens (**99.9% fewer**). context packs: 9.7M → 120.4k tokens (**98.8% fewer**).

## React (facebook/react)

Index: 6,723 files · 22,491 symbols · built in 2.63s · 30M on disk.

**Content search** (`greplm search` vs ripgrep + read whole files)

| query | files | recall | baseline | greplm | saved |
|---|---|---|---|---|---|
| `useState` | 719 | 100% | 1.7M | 11.3k | 99.3% |
| `useEffect` | 443 | 100% | 1.6M | 5.7k | 99.6% |
| `useMemo` | 488 | 100% | 960.8k | 5.5k | 99.4% |
| `useContext` | 213 | 100% | 945.6k | 3.7k | 99.6% |
| `useReducer` | 84 | 100% | 556.6k | 1.1k | 99.8% |
| `useTransition` | 60 | 100% | 526.8k | 836 | 99.8% |
| `Suspense` | 294 | 100% | 2.2M | 3.5k | 99.8% |
| `createElement` | 407 | 100% | 2.0M | 6.4k | 99.7% |
| `flushSync` | 92 | 100% | 870.1k | 1.1k | 99.9% |
| `Scheduler` | 211 | 100% | 1.6M | 1.7k | 99.9% |

**Context packs** (`greplm pack` vs read every file the pack drew from)

| task | items | files | baseline | pack | saved |
|---|---|---|---|---|---|
| use-state-internals | 189 | 53 | 469.1k | 8.1k | 98.3% |
| effect-scheduling | 44 | 14 | 178.9k | 8.0k | 95.5% |
| reconcile-children | 28 | 21 | 271.8k | 8.0k | 97.1% |
| suspense-pending | 103 | 73 | 411.2k | 8.1k | 98.0% |
| scheduler-priority | 57 | 25 | 215.6k | 8.0k | 96.3% |

## Odoo 18 (ERP)

Index: 41,142 files · 97,844 symbols · built in 4.12s · 143M on disk.

**Content search** (`greplm search` vs ripgrep + read whole files)

| query | files | recall | baseline | greplm | saved |
|---|---|---|---|---|---|
| `search_read` | 250 | 100% | 2.3M | 4.7k | 99.8% |
| `TransientModel` | 427 | 100% | 656.2k | 5.3k | 99.2% |
| `AbstractModel` | 459 | 100% | 994.0k | 5.3k | 99.5% |
| `ondelete` | 591 | 100% | 24.1M | 10.0k | 100.0% |
| `onchange` | 762 | 100% | 8.1M | 11.4k | 99.9% |
| `api.depends` | 902 | 100% | 3.2M | 10.7k | 99.7% |
| `fields.Many2one` | 1093 | 100% | 4.2M | 20.9k | 99.5% |
| `registry` | 1784 | 100% | 36.8M | 24.7k | 99.9% |
| `models.Model` | 2302 | 100% | 5.3M | 20.2k | 99.6% |
| `res.partner` | 1830 | 100% | 36.1M | 31.9k | 99.9% |

**Context packs** (`greplm pack` vs read every file the pack drew from)

| task | items | files | baseline | pack | saved |
|---|---|---|---|---|---|
| computed-fields | 35 | 25 | 1.5M | 8.0k | 99.5% |
| search-read | 59 | 43 | 1.0M | 8.0k | 99.2% |
| onchange | 37 | 29 | 488.3k | 8.0k | 98.4% |
| model-inherit | 33 | 25 | 1.7M | 8.0k | 99.5% |
| record-rules | 38 | 33 | 859.0k | 8.0k | 99.1% |

## Linux kernel

Index: 93,362 files · 3,297,692 symbols · built in 26.47s · 760M on disk.

**Content search** (`greplm search` vs ripgrep + read whole files)

| query | files | recall | baseline | greplm | saved |
|---|---|---|---|---|---|
| `try_to_wake_up` | 26 | 100% | 438.2k | 426 | 99.9% |
| `vfs_read` | 41 | 100% | 484.1k | 573 | 99.9% |
| `kobject_init` | 119 | 100% | 1.2M | 1.7k | 99.9% |
| `schedule_timeout` | 418 | 100% | 5.7M | 4.6k | 99.9% |
| `register_netdev` | 569 | 100% | 8.1M | 5.7k | 99.9% |
| `dma_alloc_coherent` | 842 | 100% | 12.8M | 12.8k | 99.9% |
| `copy_to_user` | 1164 | 100% | 10.5M | 16.2k | 99.8% |
| `file_operations` | 1517 | 100% | 12.3M | 20.9k | 99.8% |
| `rcu_read_lock` | 1712 | 99% | 22.1M | 10.8k | 100.0% |
| `task_struct` | 1734 | 100% | 10.4M | 20.9k | 99.8% |

**Context packs** (`greplm pack` vs read every file the pack drew from)

| task | items | files | baseline | pack | saved |
|---|---|---|---|---|---|
| scheduler-pick-next | 44 | 24 | 552.4k | 8.0k | 98.5% |
| skb-alloc | 269 | 49 | 593.4k | 8.1k | 98.6% |
| copy-to-user | 72 | 35 | 396.4k | 8.0k | 98.0% |
| workqueue | 112 | 46 | 682.6k | 8.0k | 98.8% |
| kmalloc | 42 | 28 | 322.5k | 8.0k | 97.5% |

