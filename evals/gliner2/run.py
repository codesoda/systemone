#!/usr/bin/env python3
"""Run the gliner2 held-out set through a real `s1 run --jsonl` and score it.

Usage:
    python3 evals/gliner2/run.py --s1 target/release/s1 --config-dir /path/with/systemone.config.toml

The config directory must hold a `systemone.config.toml` whose default backend
is a `kind = "gliner2"` instance. Nothing here knows where models live; that
is the operator's config. Output is a markdown table on stdout.

Metrics:
- Choice: argmax accuracy.
- Noul: accuracy of `probability_true >= 0.5`, plus mean P(true) on
  expected-true and expected-false items (separation).
- Score: argmax accuracy, within-one accuracy, mean absolute error of the
  expected score against the expected level index.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_items() -> dict:
    return json.loads((HERE / "heldout.json").read_text())


def request_lines(items: dict) -> list[str]:
    lines = []
    for item in items["choice"]:
        lines.append(json.dumps({
            "state": item["state"],
            "questions": {item["id"]: {
                "type": "choice",
                "instructions": item["instructions"],
                "criteria": item["options"],
            }},
        }, ensure_ascii=False))
    for item in items["noul"]:
        lines.append(json.dumps({
            "state": item["state"],
            "questions": {item["id"]: {"type": "noul", "instructions": item["instructions"]}},
        }, ensure_ascii=False))
    for item in items["score"]:
        lines.append(json.dumps({
            "state": item["state"],
            "questions": {item["id"]: {
                "type": "score",
                "instructions": item["instructions"],
                "criteria": item["levels"],
            }},
        }, ensure_ascii=False))
    return lines


def run_s1(s1: Path, config_dir: Path, lines: list[str], extra: list[str]) -> list[dict]:
    proc = subprocess.run(
        [str(s1), "run", "--jsonl", "--quiet", *extra],
        input="\n".join(lines) + "\n",
        capture_output=True,
        text=True,
        cwd=config_dir,
        check=False,
    )
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr)
        raise SystemExit(f"s1 run failed with exit code {proc.returncode}")
    rows = [json.loads(line) for line in proc.stdout.splitlines() if line.strip()]
    if len(rows) != len(lines):
        raise SystemExit(f"expected {len(lines)} rows, got {len(rows)}")
    for row in rows:
        if "error" in row:
            raise SystemExit(f"row {row.get('line')} failed: {row['error']}")
    return rows


def answer(rows: list[dict], item_id: str) -> dict:
    for row in rows:
        if item_id in row.get("answers", {}):
            return row["answers"][item_id]
    raise SystemExit(f"no answer for {item_id}")


def score(items: dict, rows: list[dict]) -> tuple[dict, list[str]]:
    misses: list[str] = []
    choice_ok = 0
    for item in items["choice"]:
        got = answer(rows, item["id"])["choice"]
        if got == item["expected"]:
            choice_ok += 1
        else:
            misses.append(f"choice {item['id']}: expected {item['expected']!r}, got {got!r}")
    noul_ok = 0
    p_true: list[float] = []
    p_false: list[float] = []
    for item in items["noul"]:
        p = answer(rows, item["id"])["noul"]
        (p_true if item["expected"] else p_false).append(p)
        if (p >= 0.5) == item["expected"]:
            noul_ok += 1
        else:
            misses.append(f"noul {item['id']}: expected {item['expected']}, P(true)={p}")
    score_exact = 0
    score_within_one = 0
    abs_error = 0.0
    for item in items["score"]:
        got = answer(rows, item["id"])
        probabilities = [got["probabilities"][str(index)] for index in range(len(item["levels"]))]
        argmax = max(range(len(probabilities)), key=probabilities.__getitem__)
        if argmax == item["expected"]:
            score_exact += 1
        else:
            misses.append(f"score {item['id']}: expected level {item['expected']}, argmax {argmax}, score {got['score']}")
        if abs(argmax - item["expected"]) <= 1:
            score_within_one += 1
        abs_error += abs(got["score"] - item["expected"])
    mean = lambda values: sum(values) / len(values) if values else float("nan")  # noqa: E731
    summary = {
        "choice_accuracy": (choice_ok, len(items["choice"])),
        "noul_accuracy": (noul_ok, len(items["noul"])),
        "noul_mean_p_true_when_true": mean(p_true),
        "noul_mean_p_true_when_false": mean(p_false),
        "score_exact": (score_exact, len(items["score"])),
        "score_within_one": (score_within_one, len(items["score"])),
        "score_mae": abs_error / len(items["score"]),
    }
    return summary, misses


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--s1", required=True, type=Path)
    parser.add_argument("--config-dir", required=True, type=Path)
    parser.add_argument("--label", default="")
    parser.add_argument("--set", action="append", default=[], help="forwarded to s1 --set")
    args = parser.parse_args()
    items = load_items()
    extra = [flag for value in args.set for flag in ("--set", value)]
    rows = run_s1(args.s1.resolve(), args.config_dir.resolve(), request_lines(items), extra)
    summary, misses = score(items, rows)
    model = rows[0].get("model", "?")
    print(f"### {args.label or model} ({model})\n")
    print("| metric | value |")
    print("| --- | --- |")
    for key, value in summary.items():
        if isinstance(value, tuple):
            print(f"| {key} | {value[0]}/{value[1]} ({100 * value[0] / value[1]:.0f}%) |")
        else:
            print(f"| {key} | {value:.3f} |")
    print()
    if misses:
        print("Misses:\n")
        for miss in misses:
            print(f"- {miss}")
        print()


if __name__ == "__main__":
    main()
