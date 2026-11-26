"""Score a labelled set with one guard and report precision and recall per question.

Usage: python -I evaluate.py <kind> <model_dir|-> <dataset.jsonl> <out_prefix>
kind: laya-onnx | laya-torch | protectai | patterns. The output holds ids and scores only.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

THRESHOLDS = (0.3, 0.5, 0.8)


def main():
    kind, model_dir, data, out = sys.argv[1:5]
    rows = [json.loads(line) for line in open(data)]
    qids = json.loads(os.environ.get("SPIKE_QIDS", "null"))
    if kind == "patterns":
        import patterns
        score = lambda texts, qs: [patterns.judge(t) for t in texts]  # noqa: E731
        questions = {k: "" for k in ("attack", "secret", "pii", "injection")}
    else:
        import guards
        g = guards.load(kind, model_dir)
        score = g.score
        questions = dict(guards.QUESTIONS)
        extra = json.loads(os.environ.get("SPIKE_QUESTIONS", "{}"))
        questions.update(extra)
        if qids:
            questions = {k: questions[k] for k in qids}
    t0 = time.perf_counter()
    scores = []
    for r in rows:
        s = score([r["text"]], questions)[0]
        scores.append({"id": r["id"], "cat": r["cat"], "labels": r["labels"], "scores": s})
    dt = time.perf_counter() - t0
    with open(out + ".scores.jsonl", "w") as f:
        for s in scores:
            f.write(json.dumps(s, sort_keys=True) + "\n")
    labelled = [q for q in questions if any(q in s["labels"] for s in scores) or q in ("attack", "secret", "pii", "injection")]
    report = {"kind": kind, "samples": len(rows), "questions": list(questions), "seconds": round(dt, 1), "per_question": {}}
    for q in labelled:
        if q not in questions:
            continue
        per = {}
        for th in THRESHOLDS:
            tp = sum(1 for s in scores if s["scores"][q] >= th and q in s["labels"])
            fp = sum(1 for s in scores if s["scores"][q] >= th and q not in s["labels"])
            fn = sum(1 for s in scores if s["scores"][q] < th and q in s["labels"])
            per[str(th)] = {"tp": tp, "fp": fp, "fn": fn,
                            "precision": round(tp / (tp + fp), 2) if tp + fp else None,
                            "recall": round(tp / (tp + fn), 2) if tp + fn else None}
        report["per_question"][q] = per
    by_cat = {}
    for s in scores:
        c = by_cat.setdefault(s["cat"], {})
        for q in questions:
            c.setdefault(q, []).append(s["scores"][q])
    report["mean_score_by_category"] = {c: {q: round(sum(v) / len(v), 2) for q, v in qs.items()} for c, qs in sorted(by_cat.items())}
    with open(out + ".report.json", "w") as f:
        json.dump(report, f, indent=1, sort_keys=True)
    print(json.dumps(report["per_question"], sort_keys=True))


if __name__ == "__main__":
    main()
