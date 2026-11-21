#!/usr/bin/env python3
"""mtseek benchmark harness.

Runs every enabled contender (ripgrep, later mtseek / zoekt / csearch) over the
same corpus with the same queries, checks they agree on the results, and
writes raw samples (results.json) plus a readable report (report.md).

The harness is deliberately written in a different language from the tool it
judges, and it only talks to contenders through their command line.
"""
from __future__ import annotations

import argparse
import ctypes
import datetime as dt
import hashlib
import json
import math
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
# Contenders' caches. Outside every scope's root, so they are never part of the corpus.
SCRATCH = (Path(tempfile.gettempdir()) / "mtseek-bench").resolve()


# --------------------------------------------------------------------------- #
# Environment
# --------------------------------------------------------------------------- #

def cpu_name() -> str:
    if sys.platform == "win32":
        try:
            import winreg

            key = winreg.OpenKey(
                winreg.HKEY_LOCAL_MACHINE,
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
            )
            return winreg.QueryValueEx(key, "ProcessorNameString")[0].strip()
        except OSError:
            pass
    return platform.processor() or "unknown"


def ram_gb() -> float | None:
    if sys.platform != "win32":
        return None

    class MEMORYSTATUSEX(ctypes.Structure):
        _fields_ = [
            ("dwLength", ctypes.c_ulong),
            ("dwMemoryLoad", ctypes.c_ulong),
            ("ullTotalPhys", ctypes.c_ulonglong),
            ("ullAvailPhys", ctypes.c_ulonglong),
            ("ullTotalPageFile", ctypes.c_ulonglong),
            ("ullAvailPageFile", ctypes.c_ulonglong),
            ("ullTotalVirtual", ctypes.c_ulonglong),
            ("ullAvailVirtual", ctypes.c_ulonglong),
            ("sullAvailExtendedVirtual", ctypes.c_ulonglong),
        ]

    stat = MEMORYSTATUSEX()
    stat.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
    ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat))
    return round(stat.ullTotalPhys / 1024**3, 1)


def on_ac_power() -> bool | None:
    """Laptops throttle on battery; results taken on battery are not comparable."""
    if sys.platform != "win32":
        return None

    class SYSTEM_POWER_STATUS(ctypes.Structure):
        _fields_ = [
            ("ACLineStatus", ctypes.c_ubyte),
            ("BatteryFlag", ctypes.c_ubyte),
            ("BatteryLifePercent", ctypes.c_ubyte),
            ("SystemStatusFlag", ctypes.c_ubyte),
            ("BatteryLifeTime", ctypes.c_ulong),
            ("BatteryFullLifeTime", ctypes.c_ulong),
        ]

    status = SYSTEM_POWER_STATUS()
    if not ctypes.windll.kernel32.GetSystemPowerStatus(ctypes.byref(status)):
        return None
    return {0: False, 1: True}.get(status.ACLineStatus)


def environment() -> dict:
    return {
        "timestamp": dt.datetime.now().isoformat(timespec="seconds"),
        "os": platform.platform(),
        "cpu": cpu_name(),
        "logical_cpus": os.cpu_count(),
        "ram_gb": ram_gb(),
        "ac_power": on_ac_power(),
        "python": platform.python_version(),
    }


# --------------------------------------------------------------------------- #
# Contenders
# --------------------------------------------------------------------------- #

def resolve_exe(exe: str) -> str | None:
    """An executable on PATH, or a path relative to bench/."""
    found = shutil.which(exe)
    if found:
        return found
    p = (HERE / exe).resolve()
    return str(p) if p.exists() else None


class Contender:
    """A search tool driven purely through its command line.

    argv is a template: "{flags}" expands to the flag list for the cell,
    "{pattern}" and "{root}" are substituted in place.
    """

    def __init__(self, name: str, spec: dict):
        self.name = name
        self.argv: list[str] = spec["argv"]
        self.flags: dict[str, list[str]] = spec.get("flags", {})
        self.literal: str = spec.get("literal", "flag")  # "flag" | "escape"
        self.ok_codes: list[int] = spec.get("ok_codes", [0])
        self.version_args: list[str] = spec.get("version_args", ["--version"])
        self.note: str = spec.get("note", "")
        # File-name search ("names" mode) has its own command line, environment, and
        # directories (caches) to empty before each cell. Contenders without one sit it out.
        self.names_argv: list[str] | None = spec.get("names_argv")
        # Values are paths under the harness's scratch directory, which lives in the system
        # temp folder: a cache inside a searched root would be part of the corpus, and would
        # differ between contenders' runs.
        self.names_env: dict[str, str] = {
            k: str(SCRATCH / v) for k, v in spec.get("names_env", {}).items()
        }
        self.names_reset: list[Path] = [SCRATCH / p for p in spec.get("names_reset", [])]
        # The same two for the content modes (files, lines): an index-backed contender keeps
        # its index in the scratch directory and has it emptied before each cell.
        self.content_env: dict[str, str] = {
            k: str(SCRATCH / v) for k, v in spec.get("env", {}).items()
        }
        self.content_reset: list[Path] = [SCRATCH / p for p in spec.get("reset", [])]

    def supports(self, mode: str) -> bool:
        return mode != "names" or self.names_argv is not None

    def env(self, mode: str) -> dict[str, str] | None:
        extra = self.names_env if mode == "names" else self.content_env
        if not extra:
            return None
        return {**os.environ, **extra}

    def reset(self, mode: str) -> None:
        """Empties the contender's caches, so the cell's first run starts cold."""
        for d in self.names_reset if mode == "names" else self.content_reset:
            if d.resolve().is_relative_to(SCRATCH):  # never delete outside the scratch dir
                shutil.rmtree(d, ignore_errors=True)

    @property
    def exe(self) -> str | None:
        return resolve_exe(self.argv[0])

    def version(self) -> str:
        if not self.exe:
            return "not installed"
        try:
            out = subprocess.run(
                [self.exe, *self.version_args], capture_output=True, timeout=30
            )
            text = (out.stdout or out.stderr).decode("utf-8", "replace").strip()
            return text.splitlines()[0] if text else "unknown"
        except (OSError, subprocess.TimeoutExpired):
            return "unknown"

    def build(self, query: dict, scope: dict, mode: str) -> list[str]:
        if mode == "names":
            assert self.names_argv is not None
            exe = self.names_argv[0]
            argv = [resolve_exe(exe) or exe]
            argv += [
                part.replace("{pattern}", query["pattern"]).replace("{root}", scope["root"])
                for part in self.names_argv[1:]
            ]
            return argv
        flags = list(self.flags.get(mode, []))
        pattern = query["pattern"]
        if query["kind"] == "literal":
            if self.literal == "escape":
                pattern = re.escape(pattern)
            else:
                flags += self.flags.get("literal", [])
        else:
            flags += self.flags.get("regex", [])
        if query.get("ignore_case"):
            flags += self.flags.get("ignore_case", [])
        flags += self.flags.get(f"scope:{scope['name']}", [])

        argv: list[str] = []
        for i, part in enumerate(self.argv):
            if i == 0:
                argv.append(self.exe or part)
            elif part == "{flags}":
                argv.extend(flags)
            else:
                argv.append(part.replace("{pattern}", pattern).replace("{root}", scope["root"]))
        return argv


# --------------------------------------------------------------------------- #
# Corpus
# --------------------------------------------------------------------------- #

def corpus_stats(scope: dict, cache_dir: Path, refresh: bool) -> dict:
    """Size of what is in scope, defined as the file list `rg --files` yields
    with the scope's flags. Binary files are listed but rg skips their
    contents, so bytes is an upper bound on bytes actually searched."""
    key = hashlib.sha1(
        json.dumps([scope["root"], scope.get("list_flags", [])]).encode()
    ).hexdigest()[:12]
    cache = cache_dir / f"corpus-{scope['name']}-{key}.json"
    if cache.exists() and not refresh:
        return json.loads(cache.read_text())

    print(f"  sizing corpus '{scope['name']}' (cached afterwards)...", flush=True)
    t0 = time.perf_counter()
    out = subprocess.run(
        ["rg", "--no-config", "--files", *scope.get("list_flags", []), scope["root"]],
        capture_output=True,
    )
    list_s = time.perf_counter() - t0
    paths = out.stdout.decode("utf-8", "surrogateescape").splitlines()

    def size(p: str) -> int:
        try:
            return os.stat(p).st_size
        except OSError:
            return 0

    with ThreadPoolExecutor(max_workers=32) as pool:
        total = sum(pool.map(size, paths, chunksize=512))

    stats = {
        "files": len(paths),
        "bytes": total,
        "rg_files_listing_s": round(list_s, 3),
        "measured_at": dt.datetime.now().isoformat(timespec="seconds"),
    }
    cache_dir.mkdir(parents=True, exist_ok=True)
    cache.write_text(json.dumps(stats, indent=2))
    return stats


# --------------------------------------------------------------------------- #
# Measurement
# --------------------------------------------------------------------------- #

def norm_path(p: str) -> str:
    return os.path.normcase(os.path.normpath(p.strip()))


def run_once(argv: list[str], timeout: float, env: dict[str, str] | None = None) -> dict:
    t0 = time.perf_counter()
    try:
        proc = subprocess.run(
            argv, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, env=env
        )
    except subprocess.TimeoutExpired:
        return {"seconds": time.perf_counter() - t0, "timeout": True}
    secs = time.perf_counter() - t0
    return {
        "seconds": secs,
        "rc": proc.returncode,
        "stdout": proc.stdout,
        "stderr_lines": len(proc.stderr.splitlines()),
        "timeout": False,
    }


def spawn_floor(runs: int = 15) -> float:
    """Cost of starting a trivial process. Every CLI contender pays it, so it
    is the floor no tool can go under when invoked this way."""
    samples = [run_once(["rg", "--version"], 30)["seconds"] for _ in range(runs)]
    return statistics.median(samples)


def summarize(samples: list[float]) -> dict:
    return {
        "n": len(samples),
        "median": statistics.median(samples),
        "min": min(samples),
        "max": max(samples),
        "mean": statistics.fmean(samples),
        "stdev": statistics.stdev(samples) if len(samples) > 1 else 0.0,
    }


def digest(stdout: bytes, mode: str) -> dict:
    text = stdout.decode("utf-8", "surrogateescape")
    lines = [l for l in text.splitlines() if l.strip()]
    if mode in ("files", "names"):
        files = sorted({norm_path(l) for l in lines})
        return {"count": len(files), "files": files}
    return {"count": len(lines)}


def is_dir(path: str) -> bool:
    # Paths past MAX_PATH need the verbatim prefix on Windows.
    if sys.platform == "win32" and len(path) >= 240 and not path.startswith("\\\\"):
        path = "\\\\?\\" + path
    return os.path.isdir(path)


def parity(ref: dict, other: dict, mode: str) -> dict:
    if mode == "names":
        # rg --files lists files only; a name search also finds directories. Extra
        # directories are expected, anything else extra or missing is a failure.
        a, b = set(ref["files"]), set(other["files"])
        missing = sorted(a - b)
        extra_all = sorted(b - a)
        extra = [p for p in extra_all if not is_dir(p)]
        return {
            "ok": not missing and not extra,
            "missing": len(missing),
            "extra": len(extra),
            "extra_dirs": len(extra_all) - len(extra),
            "missing_sample": missing[:5],
            "extra_sample": extra[:5],
        }
    if mode == "files":
        a, b = set(ref["files"]), set(other["files"])
        missing, extra = sorted(a - b), sorted(b - a)
        return {
            "ok": not missing and not extra,
            "missing": len(missing),
            "extra": len(extra),
            "missing_sample": missing[:5],
            "extra_sample": extra[:5],
        }
    return {"ok": ref["count"] == other["count"], "ref_count": ref["count"], "count": other["count"]}


def bench_cell(contenders, query, scope, mode, runs, warmup, timeout, reference) -> dict:
    argvs = {c.name: c.build(query, scope, mode) for c in contenders}
    samples = {c.name: [] for c in contenders}
    first = {}
    digests = {}
    status = {c.name: "ok" for c in contenders}

    envs = {c.name: c.env(mode) for c in contenders}

    # Warmup: first touch, also used for the result digest. Caches are emptied first, so
    # for an indexing tool the first run includes building its index.
    for c in contenders:
        c.reset(mode)
        for w in range(max(warmup, 1)):
            r = run_once(argvs[c.name], timeout, envs[c.name])
            if w == 0:
                first[c.name] = r["seconds"]
            if r["timeout"]:
                status[c.name] = "timeout"
                break
            if r["rc"] not in c.ok_codes:
                status[c.name] = f"rc={r['rc']} ({r['stderr_lines']} stderr lines)"
            digests[c.name] = digest(r["stdout"], mode)

    # Timed runs, round-robin across contenders so background noise is shared.
    for _ in range(runs):
        for c in contenders:
            if status[c.name] == "timeout":
                continue
            r = run_once(argvs[c.name], timeout, envs[c.name])
            if r["timeout"]:
                status[c.name] = "timeout"
                continue
            samples[c.name].append(r["seconds"])

    cell = {
        "query": query["id"],
        "scope": scope["name"],
        "mode": mode,
        "results": {},
    }
    ref_digest = digests.get(reference)
    for c in contenders:
        entry = {
            "argv": argvs[c.name],
            "status": status[c.name],
            "first_run_s": first.get(c.name),
            "samples_s": samples[c.name],
            "count": digests.get(c.name, {}).get("count"),
        }
        if samples[c.name]:
            entry["stats"] = summarize(samples[c.name])
        if c.name != reference and ref_digest and c.name in digests:
            entry["parity"] = parity(ref_digest, digests[c.name], mode)
        cell["results"][c.name] = entry
    return cell


# --------------------------------------------------------------------------- #
# Reporting
# --------------------------------------------------------------------------- #

def fmt_s(s: float | None) -> str:
    if s is None:
        return "–"
    if s < 1:
        return f"{s * 1000:.0f} ms"
    return f"{s:.2f} s"


def fmt_bytes(n: int) -> str:
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.1f} {unit}" if unit != "B" else f"{n} B"
        n /= 1024
    return f"{n} B"


def geomean(xs: list[float]) -> float | None:
    xs = [x for x in xs if x > 0]
    return math.exp(sum(math.log(x) for x in xs) / len(xs)) if xs else None


def write_report(path: Path, data: dict) -> None:
    env, cfg = data["environment"], data["config"]
    ref = cfg["reference"]
    names = [c["name"] for c in data["contenders"]]
    others = [n for n in names if n != ref]
    target = cfg.get("target_ms", 100) / 1000
    queries = {q["id"]: q for q in cfg["queries"]}

    L: list[str] = []
    L.append("# mtseek benchmark report\n")
    L.append(f"Run {env['timestamp']} · {env['cpu']} · {env['logical_cpus']} logical CPUs · "
             f"{env['ram_gb']} GB RAM · {env['os']}  ")
    power = {True: "on AC power", False: "**on battery (numbers not comparable)**", None: "power state unknown"}[env["ac_power"]]
    L.append(f"{power} · {data['runs']} timed runs per cell after {data['warmup']} warmup · "
             f"process spawn floor {fmt_s(data['spawn_floor_s'])} · target {fmt_s(target)}\n")
    L.append("Times are wall-clock medians, warm OS file cache (the best case for a scanning tool). "
             "`first` is the first run of the cell, which is not guaranteed to be a cold cache.\n")
    if any(c["mode"] == "names" for c in data["cells"]):
        L.append("`names` mode is file-name search, over everything on disk under the root (`-uu`), "
                 "whatever the scope's flags. rg's equivalent is `rg --files -uu --iglob *pattern*`. "
                 "Contenders' caches are emptied before each cell, so `first` includes building any "
                 "index; the timed runs reuse it. Parity allows extra directories (rg lists only files).\n")

    L.append("## Contenders\n")
    L.append("| name | version | note |\n|---|---|---|")
    for c in data["contenders"]:
        L.append(f"| {c['name']}{' (reference)' if c['name'] == ref else ''} | {c['version']} | {c['note']} |")
    L.append("")

    L.append("## Corpus\n")
    L.append("| scope | root | rg flags | files | bytes in scope | `rg --files` alone |\n|---|---|---|---:|---:|---:|")
    for s in data["scopes"]:
        st = s["corpus"]
        L.append(f"| {s['name']} | `{s['root']}` | `{' '.join(s.get('list_flags', [])) or '(defaults)'}` | "
                 f"{st['files']:,} | {fmt_bytes(st['bytes'])} | {fmt_s(st['rg_files_listing_s'])} |")
    L.append("")

    L.append("## Queries\n")
    L.append("| id | pattern | kind | what it tests |\n|---|---|---|---|")
    for q in cfg["queries"]:
        kind = q["kind"] + (", ignore case" if q.get("ignore_case") else "")
        L.append(f"| {q['id']} | `{q['pattern']}` | {kind} | {q.get('note', '')} |")
    L.append("")

    for s in data["scopes"]:
        cells = [c for c in data["cells"] if c["scope"] == s["name"]]
        if not cells:
            continue
        nbytes = s["corpus"]["bytes"]
        L.append(f"## Results: `{s['name']}`\n")
        head = ["query", "mode", "matches"]
        for n in names:
            head += [f"{n} median", f"{n} min–max"]
        head += ["rg throughput", "first"]
        for n in others:
            head += [f"{n} speedup", f"{n} parity"]
        L.append("| " + " | ".join(head) + " |")
        L.append("|" + "|".join(["---"] * 2 + ["---:"] * (len(head) - 2 - 2 * len(others)) + ["---:", "---"] * len(others)) + "|")
        ref_medians = []
        under_target = 0
        for c in cells:
            r = c["results"]
            rr = r.get(ref, {})
            unit = {"files": "files", "lines": "lines", "names": "files"}[c["mode"]]
            row = [c["query"], c["mode"], f"{rr.get('count', '–'):,} {unit}" if isinstance(rr.get('count'), int) else "–"]
            for n in names:
                if n not in r:
                    row += ["–", "–"]
                    continue
                st = r[n].get("stats")
                med = st["median"] if st else None
                mark = "" if r[n]["status"] == "ok" else f" ⚠ {r[n]['status']}"
                row += [fmt_s(med) + mark, f"{fmt_s(st['min'])}–{fmt_s(st['max'])}" if st else "–"]
            rst = rr.get("stats")
            if rst:
                ref_medians.append(rst["median"])
                under_target += rst["median"] <= target
            # Throughput means bytes searched; a name search reads no file contents.
            if rst and c["mode"] != "names":
                row.append(f"{nbytes / rst['median'] / 1024**3:.2f} GB/s")
            else:
                row.append("–")
            row.append(" · ".join(f"{n} {fmt_s(r[n].get('first_run_s'))}" for n in names if n in r))
            for n in others:
                if n not in r:
                    row += ["–", "–"]
                    continue
                ost = r[n].get("stats")
                row.append(f"{rst['median'] / ost['median']:.1f}×" if rst and ost else "–")
                p = r[n].get("parity")
                if not p:
                    row.append("–")
                elif p["ok"]:
                    row.append("✓" + (f" (+{p['extra_dirs']:,} dirs)" if p.get("extra_dirs") else ""))
                else:
                    row.append(f"✗ (-{p.get('missing', '?')} +{p.get('extra', '?')})")
            L.append("| " + " | ".join(row) + " |")
        g = geomean(ref_medians)
        L.append("")
        L.append(f"**{ref} geomean: {fmt_s(g)}** · {under_target}/{len(ref_medians)} cells under the "
                 f"{fmt_s(target)} target.\n")

    path.write_text("\n".join(L), encoding="utf-8")


# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #

def pick(all_items: dict, wanted: str | None, default: list[str]) -> list[str]:
    if not wanted:
        return default
    names = [w.strip() for w in wanted.split(",") if w.strip()]
    unknown = [n for n in names if n not in all_items]
    if unknown:
        sys.exit(f"unknown: {', '.join(unknown)} (have: {', '.join(all_items)})")
    return names


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", default=str(HERE / "bench.json"))
    ap.add_argument("--scopes", help="comma list (default: scopes with default=true)")
    ap.add_argument("--queries", help="comma list of query ids (default: all)")
    ap.add_argument("--modes", help="comma list: files,lines (default from config)")
    ap.add_argument("--contenders", help="comma list (default: every installed contender)")
    ap.add_argument("--runs", type=int, help="timed runs per cell (overrides config and scope)")
    ap.add_argument("--warmup", type=int, default=1)
    ap.add_argument("--timeout", type=float, default=900)
    ap.add_argument("--out", default=str(HERE / "results"))
    ap.add_argument("--refresh-corpus", action="store_true", help="re-measure corpus size")
    ap.add_argument("--list", action="store_true", help="show scopes, queries, contenders and exit")
    args = ap.parse_args()

    # utf-8-sig: configs saved by Windows PowerShell 5 / Notepad carry a BOM.
    cfg = json.loads(Path(args.config).read_text(encoding="utf-8-sig"))
    ref = cfg["reference"]
    scopes_all = {s["name"]: s for s in cfg["scopes"]}
    queries_all = {q["id"]: q for q in cfg["queries"]}
    contenders_all = {n: Contender(n, spec) for n, spec in cfg["contenders"].items()}

    if args.list:
        print("scopes:     ", ", ".join(f"{n}{'*' if s.get('default') else ''}" for n, s in scopes_all.items()), "(* = default)")
        print("queries:    ", ", ".join(queries_all))
        print("contenders: ", ", ".join(f"{n}{'' if c.exe else ' (not installed)'}" for n, c in contenders_all.items()))
        return

    scope_names = pick(scopes_all, args.scopes, [n for n, s in scopes_all.items() if s.get("default")])
    query_ids = pick(queries_all, args.queries, list(queries_all))
    modes = [m.strip() for m in (args.modes or ",".join(cfg.get("modes", ["files"]))).split(",")]
    wanted = pick(contenders_all, args.contenders, [n for n, c in contenders_all.items() if c.exe])
    contenders = [contenders_all[n] for n in wanted]
    missing = [c.name for c in contenders if not c.exe]
    if missing:
        sys.exit(f"not installed: {', '.join(missing)}")
    if ref not in wanted:
        sys.exit(f"reference contender '{ref}' must be included (parity is checked against it)")
    if not contenders_all[ref].supports("names") and "names" in (args.modes or ""):
        sys.exit(f"reference contender '{ref}' has no names_argv")
    contenders.sort(key=lambda c: c.name != ref)

    out_dir = Path(args.out) / dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out_dir.mkdir(parents=True, exist_ok=True)

    env = environment()
    print(f"{env['cpu']} · {env['logical_cpus']} CPUs · {env['ram_gb']} GB · AC power: {env['ac_power']}")
    if env["ac_power"] is False:
        print("WARNING: running on battery; CPU may be throttled and numbers will not be comparable.")

    scopes = []
    for n in scope_names:
        s = dict(scopes_all[n])
        s["corpus"] = corpus_stats(s, Path(args.out) / "cache", args.refresh_corpus)
        print(f"  scope {n}: {s['corpus']['files']:,} files, {fmt_bytes(s['corpus']['bytes'])}")
        scopes.append(s)

    floor = spawn_floor()
    print(f"  spawn floor: {fmt_s(floor)}")

    data = {
        "environment": env,
        "config": cfg,
        "contenders": [{"name": c.name, "version": c.version(), "note": c.note, "argv": c.argv} for c in contenders],
        "scopes": scopes,
        "spawn_floor_s": floor,
        "warmup": args.warmup,
        "runs": args.runs or cfg.get("runs", 5),
        "cells": [],
    }

    # A scope may limit itself to some queries/modes (e.g. slow whole-tree scopes).
    plan = []
    for s in scopes:
        s_queries = [q for q in query_ids if q in s.get("queries", query_ids)]
        s_modes = [m for m in modes if m in s.get("modes", modes)]
        # Name queries only make sense in names mode, and content queries only outside it.
        plan += [
            (s, q, m)
            for m in s_modes
            for q in s_queries
            if (queries_all[q]["kind"] == "name") == (m == "names")
        ]
    if not plan:
        sys.exit("nothing to run: names mode takes kind=name queries, files/lines take the rest")

    total = len(plan)
    i = 0
    for s, qid, mode in plan:
        runs = args.runs or s.get("runs") or cfg.get("runs", 5)
        i += 1
        print(f"[{i}/{total}] {s['name']:<6} {qid:<12} {mode:<6}", end="", flush=True)
        in_cell = [c for c in contenders if c.supports(mode)]
        cell = bench_cell(in_cell, queries_all[qid], s, mode, runs, args.warmup, args.timeout, ref)
        data["cells"].append(cell)
        parts = []
        for c in in_cell:
            r = cell["results"][c.name]
            med = r.get("stats", {}).get("median")
            p = r.get("parity")
            tag = "" if not p else (" ✓" if p["ok"] else " ✗parity")
            st = "" if r["status"] == "ok" else f" [{r['status']}]"
            parts.append(f"{c.name}={fmt_s(med)}{tag}{st}")
        count = cell["results"][ref].get("count")
        print(f" matches={count!s:<8} " + "  ".join(parts), flush=True)
        # Write as we go so an interrupted long run still leaves data.
        (out_dir / "results.json").write_text(json.dumps(data, indent=2, default=str), encoding="utf-8")
        write_report(out_dir / "report.md", data)

    write_report(out_dir / "report.md", data)
    print(f"\nreport:  {out_dir / 'report.md'}\nraw:     {out_dir / 'results.json'}")


if __name__ == "__main__":
    main()
