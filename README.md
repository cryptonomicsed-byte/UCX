# UCX — Universal Compute Exchange

Routes compute job requests to external GPU providers (GPU.ai, Akash, Vast.ai) through a scored matching engine.

**Port:** 7790 | **Security:** medium | **ARP:** compute receipts

## Architecture

```
UCX workspace
├── crates/ucx-core/        # JobRequest, AllocationResult, scored matching
├── crates/ucx-types/       # shared types
├── adapters/
│   ├── gpu-ai/             # GPU.ai REST adapter
│   ├── akash/              # Akash Network adapter
│   └── vast/               # Vast.ai adapter
└── MANIFEST.toml
```

## Events

| Emits | Description |
|-------|-------------|
| `job.created` | New compute job accepted |
| `job.allocated` | Provider selected, instance created |
| `job.completed` | Job done, receipt issued |
| `job.cancelled` | Job cancelled or timed out |

## Environment

| Variable | Description |
|----------|-------------|
| `GPUAI_API_KEY` | GPU.ai API key (live: `gpuai_live_yxOFo45n...`) |
| `AKASH_NODE_URL` | Akash RPC endpoint |
| `VAST_API_KEY` | Vast.ai API key |
| `UCX_PORT` | HTTP server port (default: 7790) |

## Quick Start

```bash
cd UCX
cargo build --release
UCX_PORT=7790 GPUAI_API_KEY=... ./target/release/ucx-server
```

## ARP Integration

Issues `compute` receipts upon job completion. Conforms to ARP envelope format with:
- `principal`: requesting agent
- `capability`: compute job spec
- `action`: allocate / run / cancel
- `evidence`: provider invoice + output hash
- `receipt`: signed ARP envelope
