#!/usr/bin/env python3
"""Describe all fixed campaign cells; never infer semantic correctness from overlap."""
import argparse
import json
import math
import statistics
from collections import Counter
from pathlib import Path

CONDITIONS = ("terminal", "scout", "diagnose")


def numbers(rows, field):
    return [r[field] for r in rows if isinstance(r.get(field), (int, float))
            and not isinstance(r[field], bool) and math.isfinite(r[field])]


def describe(values):
    if not values:
        return {"reported": 0, "median": None, "mean": None, "sum": None}
    return {"reported": len(values), "median": statistics.median(values),
            "mean": statistics.mean(values), "sum": sum(values),
            "min": min(values), "max": max(values)}


def summarize(data, reviews=None):
    rows, meta = data["runs"], data["metadata"]
    cases = {t["id"] for t in data["tasks"]}
    expected = {(case, c, rep) for case in cases for c in CONDITIONS
                for rep in range(1, meta["repetitions"] + 1)}
    keys = [(r["task_id"], r["condition"], r["rep"]) for r in rows]
    if len(keys) != len(set(keys)) or set(keys) != expected:
        raise ValueError("Missing or duplicate campaign cells")
    for flag in ("experiment_valid", "datasets_unchanged", "gold_unchanged", "tool_availability_verified"):
        if meta.get(flag) is not True:
            raise ValueError("Invalid experiment: " + flag)
    result = {"source_commit": meta["kiro_commit"], "date": meta["date"],
              "sessions": len(rows), "cases": len(cases), "conditions": {},
              "quality_scope": meta["quality_scope"],
              "paired_scope": "One observation per case/configuration. Ratios are descriptive; no significance or causal claim."}
    for c in CONDITIONS:
        group = [r for r in rows if r["condition"] == c]
        tools = Counter()
        for r in group:
            tools.update(r["tools"])
        result["conditions"][c] = {
            "sessions": len(group), "complete": sum(r["complete"] for r in group),
            "parsed_answers": sum(r["parsed_answer"] for r in group),
            "path_at5_hits": sum(r["path_at5_hit"] is True for r in group),
            "region_overlap_at5_hits": sum(r["region_overlap_at5"] is True for r in group),
            "file_recall_at5": describe(numbers(group, "file_recall_at5")),
            **{f: describe(numbers(group, f)) for f in ("wall_seconds", "turns", "tokens", "cost_usd")},
            "tool_calls": dict(sorted(tools.items())),
            "sessions_using_kiro": sum(any(r["tools"].get(t, 0) for t in ("run", "scout", "diagnose")) for r in group),
            "sessions_using_diagnose": sum(r["tools"].get("diagnose", 0) > 0 for r in group),
            "sessions_with_kiro_error_results": sum(r["kiro_error_results"] > 0 for r in group),
            "sessions_with_permission_denials": sum((r["permission_denials"] or 0) > 0 for r in group),
            "invalid_region_count": sum(r["invalid_regions"] for r in group),
        }
    index = {(r["task_id"], r["condition"], r["rep"]): r for r in rows}
    result["paired_comparisons"] = {}
    for candidate, baseline in (("scout", "terminal"), ("diagnose", "terminal"), ("diagnose", "scout")):
        pairs = []
        for case in sorted(cases):
            for rep in range(1, meta["repetitions"] + 1):
                a, b = index[case, candidate, rep], index[case, baseline, rep]
                pairs.append({"task_id": case, "rep": rep, "candidate_seconds": a["wall_seconds"],
                              "baseline_seconds": b["wall_seconds"], "time_ratio": a["wall_seconds"] / b["wall_seconds"]})
        result["paired_comparisons"][candidate + "_vs_" + baseline] = {
            "pairs": pairs, "candidate_faster_cases": sum(p["time_ratio"] < 1 for p in pairs),
            "median_paired_time_ratio": statistics.median(p["time_ratio"] for p in pairs),
            "ratio_of_condition_medians": result["conditions"][candidate]["wall_seconds"]["median"] / result["conditions"][baseline]["wall_seconds"]["median"],
        }
    if reviews is not None:
        review_keys = [(r["case_id"], r["condition"], r.get("rep", 1)) for r in reviews]
        if len(review_keys) != len(set(review_keys)) or set(review_keys) != expected:
            raise ValueError("Qualitative review must cover every campaign cell exactly once")
        if any(r["label"] not in ("supported", "mixed", "insufficient") for r in reviews):
            raise ValueError("Unknown qualitative label")
        result["qualitative_review"] = {
            "scope": "AI-assisted source-based review, condition names hidden. Not human-certified accuracy or validated repairs.",
            "counts": {c: {label: sum(r["condition"] == c and r["label"] == label for r in reviews)
                           for label in ("supported", "mixed", "insufficient")} for c in CONDITIONS}}
    return result


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--input", required=True, type=Path)
    p.add_argument("--reviews", type=Path)
    p.add_argument("--output", required=True, type=Path)
    a = p.parse_args()
    summary = summarize(json.loads(a.input.read_text()), json.loads(a.reviews.read_text()) if a.reviews else None)
    a.output.write_text(json.dumps(summary, indent=2) + "\n")
