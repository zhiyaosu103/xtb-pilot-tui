# xTB-Pilot-TUI

[![CI](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml/badge.svg)](https://github.com/zhiyaosu103/xtb-pilot-tui/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust 1.97.1](https://img.shields.io/badge/rust-1.97.1-orange.svg)](Cargo.toml)

**English** · [中文](README.zh-CN.md)

xTB-Pilot-TUI is a Rust-based computational orchestration tool with a TUI frontend for scheduling and managing the **xtb computational chemistry toolchain** (xtb, CREST, xTB4sTDA, sTDA) on WSL / native Linux.

It follows a daemon architecture: users interact and monitor through a terminal TUI, while external agents / scripts can submit jobs in batch and extract results through a TCP interface (JSON-RPC 2.0).

## Highlights

- **Decoupled client/server**: a background daemon (`tokio`) handles scheduling and child-process management. Quitting the TUI or closing the terminal does not interrupt running calculations.
- **Two ways to interact**:
  - **Interactive TUI** (`ratatui`): real-time convergence curves, output log tailing, queue monitoring, and keyboard-driven interaction.
  - **Automation agent interface**: TCP/NDJSON (default `127.0.0.1:7700`), with JSON-Schema introspection and event-stream subscriptions.
- **Automatic component discovery**: on startup the daemon scans PATH, conda environments, and local directories for toolchain binaries and parameter files (e.g. sTDA parameters). Multiple versions can be registered.
- **Self-contained job directories**: each job gets its own working directory (inputs, workflow snapshot, and an executable `cmd.txt` script), so any job can be reproduced or debugged outside the system.

## Architecture

```
xtbp-tui (ratatui) ◄── UDS ──► xtbp-daemon (tokio) ◄── TCP 127.0.0.1:7700 ── Agent / Script
                                  │ scheduler (xtbp-sched) / workflow engine (xtbp-workflow)
                                  │ job assembly (xtbp-assemble) / output parsing (xtbp-parse)
                                  │ storage & execution (xtbp-store / xtbp-runner)
                                  ▼
                     xtb / CREST / xTB4sTDA / sTDA / RDKit
```

## Environment Requirements

- **OS**: WSL2 (Ubuntu 22.04+) or native Linux. The working directory must live on the Linux filesystem (e.g. under `~`); mounted drives such as `/mnt/c` are rejected.
- **Rust toolchain**: 1.97.1+
- **Computational components (conda environment recommended)**:
  - `xtb` 6.7.1+
  - `crest` 3.0.2+
  - `rdkit`
  - `xtb4stda` / `stda` (requires parameter files `.param_stda1.xtb` and `.param_stda2.xtb`)

## Installation

### Option A: Prebuilt binaries (recommended)

Download `xtbpilot-linux-x86_64.tar.gz` from the latest [GitHub Release](https://github.com/zhiyaosu103/xtb-pilot-tui/releases), then:

```bash
tar -xzf xtbpilot-linux-x86_64.tar.gz
cp -r bin/. ~/.local/bin/
mkdir -p ~/.local/share/xtbpilot && cp -r templates ~/.local/share/xtbpilot/
```

### Option B: Build from source

```bash
# 1) Create the conda environment (xtb / crest / rdkit)
conda env create -f environment.yml

# 2) Optional: xtb4stda / stda binaries + parameter files (~/opt/xtb4stda-1.0)
./scripts/install-components.sh

# 3) Build and install xtbp-tui / xtbp-daemon to ~/.local/bin
./scripts/install.sh
```

## Quick Start

### 1. Launch the TUI

Just run `xtbp-tui`. If the daemon is not running, the TUI starts it automatically:

```bash
xtbp-tui
```

**Common key bindings:**

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Switch tab pages |
| `j` / `k` (or arrow keys) | Move up / down the list |
| `Space` | View details of the selected job: log tail and energy convergence curve |
| `s` | Open the submit dialog (enter SMILES, choose a workflow) |
| `/` | Filter / search the current list |
| `c` | Cancel the selected job |
| `?` | Open the help panel |
| `q` | Quit the TUI (background jobs keep running) |

*Note: append `xtbp-tui --no-spawn` to disable automatic daemon startup.*

### 2. Agent / script access (TCP JSON-RPC)

After the daemon starts, it listens on the local port (default `127.0.0.1:7700`); the auth token is stored in `~/.xtbpilot/agent.json`.

```python
# Minimal Python client (test suite provides an implementation)
python3 tests/agent_smoke.py <token>
```

Print the full API JSON-Schema exported by the daemon:

```bash
xtbp-daemon api-schema
```

## Built-in Workflows

The following semi-empirical workflows are pre-installed (defined in `templates/*.toml`). Duplicate submissions with identical content are de-duplicated automatically via content hashing:

| ID | Description | Outputs of interest |
|---|---|---|
| `opt` | 3D conformation generation → GFN2-xTB geometry optimization | optimized coordinates, ground-state energy |
| `sp` | Single-point energy (`--sp`) | total energy, orbital levels |
| `opt-freq` | Geometry optimization + harmonic frequencies (`--ohess`) | ZPE, vibrational frequencies, thermal corrections |
| `conformer` | CREST conformer search | conformer ensemble |
| `excited` | Optimization → xtb4stda → sTDA | excitation energies, oscillator strengths, broadened UV spectrum |
| `redox` | Three-state optimization (neutral / cation / anion) | adiabatic/vertical IP and EA |
| `reorg-4pt` | Four-point reorganization energy | hole/electron reorganization energies ($\lambda_h$, $\lambda_e$) |
| `solv-series` | ALPB single points across multiple solvents | solvation free-energy change series |

## Development & Testing

```bash
# Workspace unit tests (domain model, storage, parsing, ...)
cargo test --workspace

# RDKit helper protocol tests
conda run -n xtbp python python/rdkit_helper/test_helper.py

# End-to-end tests (require a real computational environment)
python3 tests/agent_smoke.py <token>       # agent interface + real calculations
python3 tests/tui_interaction.py          # PTY-based TUI interaction sessions
python3 tests/batch_parallel.py           # high-concurrency batch scheduling
```

## Documentation

- [Architecture & design notes](docs/architecture-and-design-notes.md)
- [Engineering spec (interaction & implementation conventions)](docs/engineering-spec.md)
- [Contributing guide](CONTRIBUTING.md)
- [Changelog](CHANGELOG.md)

## License

Licensed under either of [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), at your option.
