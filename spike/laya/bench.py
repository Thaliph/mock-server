"""Footprint and latency of one guard on CPU.

Usage: taskset -c 0,1 python -I bench.py <kind> <model_dir|-> <dataset.jsonl> <out.json>

1. Cold start: process start to the end of the first answer, and the model load alone.
2. RSS after load and peak RSS (VmHWM).
3. Service time: closed loop, N calls, for 1 question and for 5 questions per call,
   with the short texts of the set and with a long text (~3 KB body).
4. Offered load of 1, 50 and 200 decisions per second: a single FIFO worker with the
   measured service times (Lindley recursion over 10 s of fixed arrivals), with an
   unbounded queue and with a queue of 64 that drops the overflow (the detect-mode sampler).
"""
import json
import os
import random
import sys
import time

T_START = time.perf_counter()
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def rss_kb():
    out = {}
    with open("/proc/self/status") as f:
        for line in f:
            if line.startswith(("VmRSS", "VmHWM")):
                k, v = line.split(":")
                out[k] = int(v.split()[0])
    return out


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def lindley(service, rate, seconds=10.0, cap=None, seed=1):
    """Latency (wait + service) of each accepted arrival, and the drop ratio."""
    rnd = random.Random(seed)
    gap = 1.0 / rate
    n = int(rate * seconds)
    busy_until = 0.0
    in_system = []  # finish times of accepted calls
    lat, dropped = [], 0
    for i in range(n):
        t = i * gap
        in_system = [f for f in in_system if f > t]
        if cap is not None and len(in_system) > cap:
            dropped += 1
            continue
        s = rnd.choice(service)
        start = max(t, busy_until)
        busy_until = start + s
        in_system.append(busy_until)
        lat.append(busy_until - t)
    return lat, dropped / n if n else 0.0


def ms(x):
    return None if x is None else round(x * 1000, 3)


def main():
    kind, model_dir, data, out = sys.argv[1:5]
    n_calls = int(os.environ.get("SPIKE_CALLS", "40"))
    rows = [json.loads(line) for line in open(data)]
    texts = [r["text"] for r in rows]
    long_text = "POST /v1/chat/completions HTTP/1.1\nhost: llm-gateway.ai.svc\ncontent-type: application/json\n\n" + json.dumps(
        {"messages": [{"role": "user", "content": " ".join(texts[i % len(texts)].split("\n")[-1] for i in range(40))[:3000]}]})
    t_import = time.perf_counter()
    if kind == "patterns":
        import patterns
        questions_all = {k: "" for k in ("attack", "secret", "pii", "injection", "harmful")}

        def score(states, qs):
            return [patterns.judge(s) for s in states]
    else:
        import guards
        questions_all = dict(guards.QUESTIONS)
        g = guards.load(kind, model_dir)
        score = g.score
    t_loaded = time.perf_counter()
    score([texts[0]], dict(list(questions_all.items())[:1]))
    t_first = time.perf_counter()
    res = {"kind": kind, "threads": os.environ.get("SPIKE_THREADS", "2"), "cpus": sorted(os.sched_getaffinity(0)),
           "cold_start_ms": {"process_to_first_answer": ms(t_first - T_START), "model_load": ms(t_loaded - t_import),
                             "first_answer": ms(t_first - t_loaded)},
           "rss_after_load_kb": rss_kb(), "service": {}, "offered_load": {}}
    rnd = random.Random(7)
    for nq in (1, 5):
        qs = dict(list(questions_all.items())[:nq])
        for size in ("short", "long"):
            st = []
            for i in range(n_calls if size == "short" else max(3, n_calls // 4)):
                text = rnd.choice(texts) if size == "short" else long_text
                t0 = time.perf_counter()
                score([text], qs)
                st.append(time.perf_counter() - t0)
            key = f"q{nq}_{size}"
            res["service"][key] = {"n": len(st), "p50_ms": ms(pct(st, 50)), "p99_ms": ms(pct(st, 99)),
                                   "mean_ms": ms(sum(st) / len(st)), "max_rps_one_worker": round(len(st) / sum(st), 1)}
            if size == "short":
                for rate in (1, 50, 200):
                    lat, _ = lindley(st, rate)
                    latc, drop = lindley(st, rate, cap=64)
                    res["offered_load"][f"q{nq}_{rate}rps"] = {
                        "unbounded_p50_ms": ms(pct(lat, 50)), "unbounded_p99_ms": ms(pct(lat, 99)),
                        "queue64_p50_ms": ms(pct(latc, 50)), "queue64_p99_ms": ms(pct(latc, 99)),
                        "queue64_drop_ratio": round(drop, 3)}
    res["rss_end_kb"] = rss_kb()
    with open(out, "w") as f:
        json.dump(res, f, indent=1, sort_keys=True)
    print(json.dumps(res, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
