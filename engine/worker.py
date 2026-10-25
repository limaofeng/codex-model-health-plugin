"""离线双引擎适配，模型流量与凭据保留在 Codex Proxy。"""
from __future__ import annotations

import json
import sys
import traceback
from collections import Counter, defaultdict
from pathlib import Path

ROOT = Path(getattr(sys, "_MEIPASS", Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(ROOT / "vendor" / "meow"))

from gpt56_vnext import BUNDLED_BASELINES, __version__
from gpt56_vnext.benchmark import cells_by_id, load_package
from gpt56_vnext.detector import build_single_jobs, calibration_matches
from gpt56_vnext.errors import AppError, RequestError
from gpt56_vnext.normalizers import normalize_answer
from gpt56_vnext.probability_model import score_counts
from gpt56_vnext.predictive import UNKNOWN
from gpt56_vnext.protocol import build_payload, parse_stream
from gpt56_vnext.security import SecretGuard
import modeltrace_adapter

BASELINES = ROOT / "vendor/meow/gpt56_vnext/baselines" / BUNDLED_BASELINES
MANIFEST = json.loads((BASELINES / "manifest.json").read_text())
PACKAGE = load_package((BASELINES / next(p["file"] for p in MANIFEST["packages"] if p["mode"] == "gpt")).read_text())
CELLS = cells_by_id(PACKAGE)
MAX_LINE = 2 * 1024 * 1024


def catalog():
    return {
        "engine_version": __version__, "id": PACKAGE["id"],
        "version": PACKAGE["version"], "hash": PACKAGE["content_sha256"],
        "models": [m for m in PACKAGE["models"] if not m.get("reference_only") and m.get("request_model") != "reference-only:other"],
        "tiers": [{"id": tier, "count": sum(value["counts"].values())}
                  for tier, value in PACKAGE["tiers"].items()],
    }


def check_plan(plan):
    if plan["hash"] != PACKAGE["content_sha256"]:
        raise AppError("baseline_changed")
    if plan["tier"] not in PACKAGE["tiers"]:
        raise AppError("invalid_tier")
    if plan["claimed_model"] not in [m["id"] for m in catalog()["models"]]:
        raise AppError("invalid_claimed_model")


def prepare(data):
    plan = {**catalog(), "method": "meow", "tier": data["tier"], "claimed_model": data["claimed_model"],
            "request_model": data["request_model"]}
    check_plan(plan)
    if not isinstance(plan["request_model"], str) or not 1 <= len(plan["request_model"].strip()) <= 256:
        raise AppError("invalid_request_model")
    jobs = build_single_jobs(PACKAGE, plan["tier"], plan["request_model"])
    jobs.sort(key=lambda j: (int(j["job_id"].rsplit(":", 1)[1]), j["cell_id"]))
    plan["jobs"] = jobs
    plan["retry_budget"] = (len(jobs) + 1) // 2
    del plan["models"], plan["tiers"]
    return plan


def score(plan, results):
    check_plan(plan)
    jobs = {j["job_id"]: j for j in build_single_jobs(PACKAGE, plan["tier"], plan["request_model"])}
    counts = defaultdict(Counter)
    seen = set()
    for result in results:
        if result["status"] != "ok":
            continue
        identity = result["job_id"]
        if identity not in jobs or identity in seen:
            raise AppError("duplicate_or_unknown_sample")
        seen.add(identity)
        counts[jobs[identity]["cell_id"]][result["category"]] += 1
    tier = PACKAGE["tiers"][plan["tier"]]
    return score_counts(PACKAGE["fitted"], dict(counts), tier["counts"], tier["thresholds"],
                        calibrated=calibration_matches(PACKAGE, plan["tier"]),
                        claimed_model=plan["claimed_model"], completion_ratio=.6)


def excerpt(text, limit=256):
    return text.encode("utf-8")[:limit].decode("utf-8", errors="ignore")


def handle(data):
    op = data["op"]
    if op == "catalog":
        return {"methods": [{"method": "meow", "label": "Meow", **catalog()}, modeltrace_adapter.catalog()]}
    method = (data if op == "prepare" else data["plan"]).get("method", "meow")
    if method not in {"meow", "modeltrace"}:
        raise AppError("unknown_detection_method")
    if op == "prepare":
        return modeltrace_adapter.prepare(data) if method == "modeltrace" else prepare(data)
    plan = data["plan"]
    if method == "modeltrace":
        modeltrace_adapter.check_plan(plan)
    else:
        check_plan(plan)
    if op == "score":
        return modeltrace_adapter.score(plan, data["results"]) if method == "modeltrace" else score(plan, data["results"])
    jobs = plan["jobs"] if method == "modeltrace" else build_single_jobs(PACKAGE, plan["tier"], plan["request_model"])
    job = next((j for j in jobs if j["job_id"] == data["job_id"]), None)
    if job is None:
        raise AppError("unknown_sample")
    if op == "payload":
        if method == "modeltrace":
            # 原项目使用供应商默认采样参数，不套用 Meow 的短回答限制和 reasoning 配置。
            return {"model": plan["request_model"], "stream": True, "store": False,
                    "input": [{"role": "user", "content": job["prompt"]}]}
        return build_payload("gpt", plan["request_model"], CELLS[job["cell_id"]])
    if op == "evaluate":
        try:
            parsed = parse_stream(data["sse"], "gpt", SecretGuard([]))
            if method == "modeltrace":
                return {**modeltrace_adapter.evaluate(parsed["answer"], job),
                        "answer": excerpt(parsed["answer"]),
                        "answer_truncated": len(parsed["answer"].encode("utf-8")) > 256,
                        "response_id": excerpt(str(parsed["response_id"]), 128) if parsed["response_id"] else None,
                        "usage": parsed["usage"]}
            category = normalize_answer(parsed["answer"], CELLS[job["cell_id"]]["normalizer"])
            # 原评分器也将未入基线的类别合并到 UNKNOWN；避免状态保存任意长文本。
            if category != "__INVALID_OUTPUT__" and category not in PACKAGE["fitted"]["cells"][job["cell_id"]]["categories"]:
                category = UNKNOWN
            return {"status": "ok" if category != "__INVALID_OUTPUT__" else "error",
                    "error": None if category != "__INVALID_OUTPUT__" else "invalid_answer",
                    "category": category, "answer": excerpt(parsed["answer"]),
                    "answer_truncated": len(parsed["answer"].encode("utf-8")) > 256,
                    "response_id": excerpt(str(parsed["response_id"]), 128) if parsed["response_id"] else None,
                    "usage": parsed["usage"]}
        except RequestError as exc:
            return {"status": "error", "error": exc.code, "category": None, "answer": "",
                    "answer_truncated": False, "response_id": exc.evidence.get("response_id"),
                    "usage": exc.evidence.get("usage")}
    raise AppError("unknown_operation")


def failure(exc):
    """返回给宿主的错误码；意外异常保留堆栈到 stderr，并为 KeyError 带上缺失的键名。"""
    code = getattr(exc, "code", None)
    if code is not None:
        return code
    if sys.exc_info()[0] is not None:
        traceback.print_exc(file=sys.stderr)
    sys.stderr.flush()
    if isinstance(exc, KeyError):
        return f"KeyError: {exc.args[0]!r}"[:96] if exc.args else "KeyError"
    return type(exc).__name__


def main():
    while line := sys.stdin.buffer.readline(MAX_LINE + 1):
        if len(line) > MAX_LINE:
            raise ValueError("worker request exceeds 2 MiB")
        try:
            result = {"ok": True, "data": handle(json.loads(line))}
        except (AppError, RequestError, KeyError, TypeError, ValueError) as exc:
            result = {"ok": False, "error": failure(exc)}
        print(json.dumps(result, ensure_ascii=False, allow_nan=False, separators=(",", ":")), flush=True)


if __name__ == "__main__":
    main()
