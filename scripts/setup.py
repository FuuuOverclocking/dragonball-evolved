import argparse
import os
import subprocess
import sys
import venv
from pathlib import Path


ENVIRONMENT = Path(__file__).resolve().parent.parent / "target" / ".venv"


def setup(requirements=()):
    if Path(sys.prefix).resolve() != ENVIRONMENT:
        python = ENVIRONMENT / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
        if not python.is_file():
            venv.create(ENVIRONMENT)
        subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "--python",
                str(python),
                "install",
                "--quiet",
                "--disable-pip-version-check",
                "pip",
            ],
            check=True,
        )
        os.execv(str(python), [str(python), *sys.orig_argv[1:]])

    if requirements:
        subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--quiet",
                "--disable-pip-version-check",
                *requirements,
            ],
            check=True,
        )


def main():
    parser = argparse.ArgumentParser(
        description="Create the shared project Python environment and install dependencies."
    )
    parser.add_argument("requirements", nargs="*", help="pip requirement specifiers")
    args = parser.parse_args()
    try:
        setup(args.requirements)
    except (OSError, subprocess.SubprocessError) as error:
        print(f"Python environment setup failed: {error}", file=sys.stderr)
        return 1
    print(f"Python environment ready: {ENVIRONMENT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
