#!/usr/bin/env python3
"""Entrypoint: exec the bundled forge server binary.

The whole forge — git smart-HTTP, REST, GraphQL and MCP over one
in-memory store — is a single self-contained Rust binary named `forge`
sitting next to this file. Python only finds it and hands the process
over with exec, so the process the environment talks to is the Rust
server itself and there is nothing to install.
"""

import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BINARY = os.path.join(HERE, "forge")


def main() -> None:
    if not os.path.isfile(BINARY):
        sys.stderr.write("forge binary missing beside main.py\n")
        sys.exit(1)
    os.execv(BINARY, [BINARY])


if __name__ == "__main__":
    main()
