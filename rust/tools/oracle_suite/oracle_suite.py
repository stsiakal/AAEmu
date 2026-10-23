#!/usr/bin/env python3
"""Side-by-side oracle suite: original ArcheAge 1.2 client vs Bevy `aa-client`.

Run from anywhere; the lane must exist (`Scripts/lane.sh new <name>`).  The tool
starts the lane stack, launches both clients against it, plays the scenario files
in `scenarios/`, and writes a report with packet-field and screenshot evidence.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from oracle.runner import Suite, SuiteOptions, load_scenario, scenario_paths


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", nargs="?", default="run",
                        choices=["run", "list", "check"])
    parser.add_argument("scenarios", nargs="*",
                        help="scenario stems (default: all in scenarios/)")
    parser.add_argument("--lane", default="oracle-suite")
    parser.add_argument("--out", type=Path,
                        help="report directory (default D:/aaemu-oracle-runs/<lane>-<time>)")
    parser.add_argument("--bevy-client", type=Path,
                        default=Path("D:/aaemu-target/bevy/debug/aa-client.exe"))
    parser.add_argument("--original-client", type=Path,
                        default=Path("D:/AA1.2/bin32/archeage.exe"))
    parser.add_argument("--build", choices=["auto", "always", "never"], default="auto")
    parser.add_argument("--keep-stack", action="store_true",
                        help="leave the lane stack running after the run")
    parser.add_argument("--reuse-stack", action="store_true",
                        help="keep an already running lane stack instead of restarting it")
    parser.add_argument("--bevy-size", default="1280x720")
    args = parser.parse_args(argv)

    root = Path(__file__).resolve().parent
    paths = scenario_paths(root)
    if args.command == "list":
        for path in paths:
            scenario = load_scenario(path)
            print(f"{path.stem}: {scenario.get('description', '')}")
        return 0
    if args.command == "check":
        for path in paths:
            load_scenario(path)
            print(f"{path.stem}: ok")
        return 0

    unknown = [name for name in args.scenarios if name not in {path.stem for path in paths}]
    if unknown:
        parser.error("unknown scenario(s): " + ", ".join(unknown))
    options = SuiteOptions(
        lane=args.lane,
        scenarios=args.scenarios,
        out=args.out,
        bevy_client=args.bevy_client,
        original_client=args.original_client,
        build=args.build,
        keep_stack=args.keep_stack,
        restart_stack=not args.reuse_stack,
        bevy_size=args.bevy_size,
    )
    return Suite(options).run()


if __name__ == "__main__":
    sys.exit(main())
