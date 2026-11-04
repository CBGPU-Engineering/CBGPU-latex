# CBGPU LaTeX

[![CI](https://github.com/CBGPU-Engineering/CBGPU-latex/actions/workflows/ci.yml/badge.svg)](https://github.com/CBGPU-Engineering/CBGPU-latex/actions/workflows/ci.yml)
[![License: AGPL 3.0](https://img.shields.io/badge/License-AGPL%203.0-blue)](LICENSE)

Overleaf Community Edition integration for CBGPU: authenticated project access,
ZIP import, main-document/compiler selection, trash synchronization and a
separate resource-limited TeX compile broker. This is not Overleaf Server Pro.

## Tests

```bash
git clone https://github.com/CBGPU-Engineering/CBGPU-latex.git
cd CBGPU-latex
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python -m unittest discover -s . -p test_zip_import.py
.venv/bin/python -m unittest discover -s . -p test_http_dependencies.py
```

The ZIP and HTTP dependency tests are local. The compiler/restore integration
checks require Docker and dedicated test infrastructure; they are not part of
the credential-free PR suite.

## Deployment

[CBGPU-backend](https://github.com/CBGPU-Engineering/CBGPU-backend) pins this repository as its `latex/`
submodule. [CBGPU-platform](https://github.com/CBGPU-Engineering/CBGPU-platform) assembles the full layout,
placing it at `backend/latex`. Existing setup scripts expect that integration
layout and private state under `backend/private/latex`.

Read [integration, compile limits, source offer and backup details](docs/INTEGRATION.md)
before deploying. Do not copy private project files or credentials into Git.

## License

This integration retains [AGPL-3.0](LICENSE). Overleaf CE and TeX packages retain
their own licenses; keep the corresponding source offer and notices. It is not
relicensed by the Apache-2.0 license used for other CBGPU components.
