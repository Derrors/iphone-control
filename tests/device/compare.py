"""Compares the step times of two `run.py --json` runs on the same device: is the branch slower than main?

    MCP_CMD='iphone-control mcp' python tests/device/run.py --repeat 5 --json main.json      # main's daemon running
    MCP_CMD="$PWD/target/release/iphone-control mcp" python tests/device/run.py --repeat 5 --json branch.json
    python tests/device/compare.py main.json branch.json

Per step it takes the median over the runs, without the time an automatic unlock took (the answer says how long:
a phone that locked itself while idle would otherwise make one step 8 to 15 seconds slower). A step counts as
slower when it takes over 20 % and over 0.1 s longer, or over 0.5 s longer (CONTRIBUTING.md, "Required tests",
part 2): those are marked SLOWER and need a reason in the pull request. Exit code 1 if any step is slower or the
two runs didn't do the same steps. No dependencies beyond the standard library.
"""

import json
import statistics
import sys


def medians(path: str):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    runs = [r for r in data["runs"] if r]
    steps = [(s["tool"], json.dumps(s["args"], ensure_ascii=False, sort_keys=True)) for s in runs[0]]
    same = [r for r in runs if [(s["tool"], json.dumps(s["args"], ensure_ascii=False, sort_keys=True)) for s in r] == steps]
    times = [statistics.median(max(r[i]["seconds"] - r[i].get("unlock_seconds", 0), 0) for r in same) for i in range(len(steps))]
    failed = sum(not s["ok"] for r in runs for s in r)
    return data.get("device", "?"), steps, times, len(same), len(runs), failed


def slower(base: float, new: float) -> bool:
    d = new - base
    return d > 0.5 or (d > 0.1 and new > base * 1.2)


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    (dev_a, steps_a, t_a, n_a, all_a, bad_a), (dev_b, steps_b, t_b, n_b, all_b, bad_b) = map(medians, sys.argv[1:])
    print(f"base: {sys.argv[1]}  {dev_a}  ({n_a} of {all_a} runs, {bad_a} failed steps)")
    print(f"new:  {sys.argv[2]}  {dev_b}  ({n_b} of {all_b} runs, {bad_b} failed steps)")
    wrong = 0
    if dev_a != dev_b:
        print("NOT THE SAME DEVICE or control mode: the times don't compare")
        wrong += 1
    if steps_a != steps_b:
        print("NOT THE SAME STEPS: run both with the same steps")
        return 1
    print(f"{'step':<44} {'base':>7} {'new':>7} {'diff':>7}")
    for (tool, args), a, b in zip(steps_a, t_a, t_b):
        mark = "  SLOWER" if slower(a, b) else ""
        wrong += bool(mark)
        name = f"{tool} {args}"
        print(f"{name[:43]:<44} {a:7.2f} {b:7.2f} {b - a:+7.2f}{mark}")
    print(f"{'total':<44} {sum(t_a):7.2f} {sum(t_b):7.2f} {sum(t_b) - sum(t_a):+7.2f}")
    return 1 if wrong else 0


if __name__ == "__main__":
    sys.exit(main())
