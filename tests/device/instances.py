"""Two (or more) instances tested side by side (docs/dev/multi-instance-plan.md 7): each gets its own MCP server,
screenshots directory and expected device, and run.py (or scenarios.py) runs for all of them at once.

    python tests/device/instances.py default=<UDID> ipad=<UDID>                 # run.py's default steps on both
    python tests/device/instances.py --scenarios default=<UDID> ipad=<UDID>     # scenarios.py's default set
    python tests/device/instances.py --repeat 3 --json-dir out default=<UDID> ipad=<UDID>
    EXE=target/release/iphone-control python tests/device/instances.py ...      # a build of your branch

Each instance's MCP server is `<EXE> [--instance <name>] mcp --local`: its own socket only, whatever the settings
file says about another viewer or instance. Before any step each run checks it controls the device given for it
(EXPECT_UDID): a run on the wrong device stops at once. Screenshots go to SHOTS_DIR/<name> (default shots/<name>).

The output of each run is printed as it comes, every line prefixed with the instance's name; the exit code is 1 if
any run failed. To judge latency, compare each instance's timings (--json-dir) with a run of the same steps on the
same machine with one instance only (compare.py), against thresholds set beforehand.
"""

import argparse
import asyncio
import os
import shlex
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


async def one(name: str, udid: str, script: str, extra: list[str], exe: str, shots: str) -> int:
    cmd = [exe] + ([] if name == "default" else ["--instance", name]) + ["mcp", "--local"]
    env = dict(os.environ, MCP_CMD=shlex.join(cmd), EXPECT_UDID=udid, SHOTS_DIR=os.path.join(shots, name))
    # the instance is the MCP command's: a stray one from the environment would make every run the same one
    env.pop("IPHONE_CONTROL_INSTANCE", None)
    p = await asyncio.create_subprocess_exec(
        sys.executable, os.path.join(HERE, script), *extra, env=env,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    assert p.stdout
    async for line in p.stdout:
        print(f"[{name}] {line.decode(errors='replace').rstrip()}", flush=True)
    return await p.wait()


async def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("pairs", nargs="+", metavar="NAME=UDID", help="an instance (default for the default one) and its device")
    ap.add_argument("--scenarios", action="store_true", help="scenarios.py instead of run.py")
    ap.add_argument("--repeat", type=int, default=1, metavar="N", help="run.py's --repeat")
    ap.add_argument("--json-dir", metavar="DIR", help="run.py's timings, one file per instance, for compare.py")
    a = ap.parse_args()
    pairs = []
    for p in a.pairs:
        name, sep, udid = p.partition("=")
        if not sep or not name or not udid:
            ap.error(f"{p}: NAME=UDID")
        pairs.append((name, udid))
    if len({u.upper() for _, u in pairs}) != len(pairs):
        ap.error("each instance needs a device of its own")
    exe = os.environ.get("EXE", "iphone-control")
    shots = os.environ.get("SHOTS_DIR", "shots")
    runs = []
    for name, udid in pairs:
        extra: list[str] = []
        if not a.scenarios:
            extra += ["--repeat", str(a.repeat)]
            if a.json_dir:
                os.makedirs(a.json_dir, exist_ok=True)
                extra += ["--json", os.path.join(a.json_dir, f"{name}.json")]
        runs.append(one(name, udid, "scenarios.py" if a.scenarios else "run.py", extra, exe, shots))
    codes = await asyncio.gather(*runs)
    for (name, _), code in zip(pairs, codes):
        print(f"=== {name}: {'ok' if code == 0 else f'failed ({code})'}", flush=True)
    return 1 if any(codes) else 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
