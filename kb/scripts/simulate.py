#!/usr/bin/env python3
"""multi-generation simulation of a kb knowledge base with live models.

each generation is one simulated working day, run with KB_NOW and git dates fixed to that day:

1. a human writes a daily note and edits an existing note, committing on main (the pre-commit
   hook fills frontmatter and calls the classify model).
2. a capture lands in the inbox; the agent curates it (`kb curate captures`).
3. the agent rewrites the same notes again (`kb curate propose`), the stress test for drift.
4. every agent session goes through `kb merge`; a simulated person approves it unless the
   content reviewer requested changes.
5. three fixed questions are answered (`kb answer ask`).
6. the clock moves to 02:00 (eod sweep and its merge loop), 03:00 (nightly curation, with the
   simulated person approving its queue except deletes and reverts), and 04:00 (full backup).
7. every note is snapshotted and measured: words, lexical diversity, similarity to generation 0
   and to the previous generation, retention of the facts it started with, repeated lines.

output, under --out: simulation.log (every command, every landed diff, every review verdict and
answer), metrics.tsv (one row per note per generation), report.md (summary and drift flags),
gen_NN/ (note snapshots), and kb/ (the simulated knowledge base, transcripts in
kb/.kb/state.sqlite).
"""

from __future__ import annotations

import argparse
import datetime as dt
import difflib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

FIXTURES_DEFAULT = Path(__file__).resolve().parents[2] / ".tmp" / "fixtures" / "notes"

# notes the agent rewrites: the first every generation, the rest in rotation.
REFINE_EVERY = "static_ip"
REFINE_ROTATION = ["rf_switcher_power_debug", "self_hosted_runner_transition", "CUAS_ins_validity_guards", "versioning_guide"]
PROMOTE_TO_CANON = ["static_ip", "versioning_guide", "self_hosted_runner_transition", "world_frame_transforms"]

QUESTIONS = [
	"what static ip does the launcher use, and how do I set a static ip between two linux devices?",
	"how much attenuation does the rf switcher have at 5.8 GHz on port 4?",
	"how do we move CI to the self-hosted runner?",
]

DAILY_NOTES = [
	"# {date} daily\n\n- [x] flashed the launcher jetson, kept 192.168.50.10\n- [ ] check rf switcher port 4 loss at 5.8 GHz again\n- standup: Ryan owns the INS refactor branch\n",
	"# {date} daily\n\n- [ ] bump config version to 0.4.0 before the field test\n- [x] reviewed PR #101 (track integration test)\n- note: nmcli con mod needs ipv4.method manual\n",
	"# {date} daily\n\n- [ ] move ci.yml to runs-on: [self-hosted, linux, x64]\n- [x] cleaned docker build cache, freed 41 GB\n- idea: cache cargo registry on the runner disk\n",
	"# {date} daily\n\n- [x] pico pin 21 (GP16) wired to the trigger relay\n- [ ] KF NIS test with near-singular S\n- remember: INS staleness bound is 200 ms\n",
	"# {date} daily\n\n- [ ] world frame: confirm ENU vs NED in the bridge\n- [x] dome flight test moved to thursday\n- rf: mutual coupling worst at 5.6 GHz\n",
	"# {date} daily\n\n- [x] tagged v0.4.0, release notes in the PR\n- [ ] runner: register second runner as ts-build-02\n- jetson static ip unchanged\n",
	"# {date} daily\n\n- [ ] kf: add test for non-PSD P\n- [x] launcher wall eth port relabeled\n- versioning: tags are vX.Y.Z only, no suffixes\n",
	"# {date} daily\n\n- [x] runner disk at 70 percent, prune weekly\n- [ ] rf switcher: swap SMA cable on port 4\n- INS: alignment flag ignored by acoustic node\n",
	"# {date} daily\n\n- [ ] pico: document GP17 as spare\n- [x] static ip guide tested on ubuntu 24.04\n- meeting: data pipeline part I lands first\n",
	"# {date} daily\n\n- [x] port 4 loss after cable swap: 13 dB at 5.8 GHz\n- [ ] update rf switcher note\n- standup: field test friday\n",
	"# {date} daily\n\n- [ ] ci: cache docker layers on /var/lib/runner-cache\n- [x] ENU confirmed in the bridge\n- reminder: launcher ip stays 192.168.50.10\n",
	"# {date} daily\n\n- [x] versioning guide shared with the team\n- [ ] kf near-singular S raises nothing; add a check\n- dome test passed\n",
]

CAPTURES = [
	"meeting notes {date}: the launcher keeps 192.168.50.10. the laptop side should use 192.168.50.20 with prefix /24. no gateway is needed on a direct link.",
	"meeting notes {date}: rf switcher retest. port 4 attenuation at 5.8 GHz measured 16 dB including about 6 dB of cable loss. mutual coupling at 5.6 GHz still dominates direction finding error.",
	"meeting notes {date}: CI moves to the self-hosted runner ts-build-01. the workflow keeps docker/Dockerfile as the single source of truth. caches live on the runner disk.",
	"meeting notes {date}: INS staleness bound agreed at 200 ms. consumers check ins_status bits rather than a derived enum.",
	"meeting notes {date}: release tags follow vX.Y.Z. config files carry a version field that must match the tag.",
	"meeting notes {date}: after swapping the SMA cable, port 4 loss at 5.8 GHz dropped to 13 dB. keep the old 16 dB figure as the pre-swap baseline.",
	"meeting notes {date}: second runner ts-build-02 registered with labels self-hosted, linux, x64. prune docker images weekly.",
	"meeting notes {date}: for a direct link, set ipv4.method manual and ipv4.addresses on both ends with nmcli, then bring the connection up.",
	"meeting notes {date}: pico GP16 drives the trigger relay; GP17 is a spare.",
	"meeting notes {date}: kalman filter near-singular S returns an ill-conditioned result without raising; add a condition-number check.",
	"meeting notes {date}: the bridge publishes in ENU; NED appears only inside the INS driver.",
	"meeting notes {date}: launcher static ip confirmed again; document the wall port label LNCH-ETH-1.",
]

FACT_PATTERNS = [
	r"\b\d{1,3}(?:\.\d{1,3}){3}(?:/\d{1,2})?\b",  # ip addresses
	r"\b\d+(?:\.\d+)?\s?(?:GHz|MHz|dB|DB|ms|GB|MB|%)",  # measurements
	r"`[^`\n]{2,60}`",  # inline code
	r"\bv\d+\.\d+\.\d+\b",  # versions
	r"\bGP\d{1,2}\b",  # pico pins
	r"(?m)^\s*(?:sudo|nmcli|ping|ssh|git|docker|systemctl|ip) [^\n]+$",  # whole command lines, fenced or not
]

GENERIC_PHRASES = ["in summary", "in conclusion", "it is important to note", "overall,", "this document provides", "comprehensive", "robust", "seamless", "leverage"]


@dataclass
class Sim:
	kb_bin: Path
	out: Path
	kb: Path
	log_path: Path
	writer_model: str
	fast_model: str
	now: dt.datetime
	log_lines: list[str] = field(default_factory=list)
	ids: dict[str, str] = field(default_factory=dict)
	baseline: dict[str, str] = field(default_factory=dict)
	baseline_facts: dict[str, set[str]] = field(default_factory=dict)
	previous: dict[str, str] = field(default_factory=dict)
	counters: dict[str, int] = field(default_factory=dict)

	# -- logging and process plumbing ---------------------------------------------------------

	def log(self, text: str = "") -> None:
		with self.log_path.open("a") as f:
			f.write(text + "\n")
		print(text, flush=True)

	def section(self, title: str) -> None:
		self.log("")
		self.log("=" * 100)
		self.log(f"[{self.now.isoformat()}] {title}")
		self.log("=" * 100)

	def env(self) -> dict[str, str]:
		stamp = self.now.isoformat()
		env = dict(os.environ)
		env.update(
			{
				"KB_NOW": stamp,
				"GIT_AUTHOR_DATE": stamp,
				"GIT_COMMITTER_DATE": stamp,
				"KB_BIN": str(self.kb_bin),
				"PATH": f"{self.kb_bin.parent}:{env.get('PATH', '')}",
			}
		)
		return env

	def run(self, args: list[str], *, cwd: Path | None = None, check: bool = False, quiet: bool = False, extra_env: dict[str, str] | None = None, timeout: int = 1800) -> subprocess.CompletedProcess[str]:
		env = self.env()
		if extra_env:
			env.update(extra_env)
		started = time.monotonic()
		proc = subprocess.run(args, cwd=cwd or self.kb, env=env, text=True, capture_output=True, timeout=timeout, check=False)
		took = time.monotonic() - started
		shown = " ".join(a if " " not in a else repr(a) for a in args)
		self.log(f"$ {shown}   [{took:.1f}s, exit {proc.returncode}]")
		if not quiet:
			for stream in (proc.stdout, proc.stderr):
				text = stream.strip()
				if text:
					self.log("  " + text.replace("\n", "\n  "))
		if check and proc.returncode != 0:
			raise RuntimeError(f"command failed: {shown}\n{proc.stderr}")
		return proc

	def kb_cmd(self, *args: str, **kw) -> subprocess.CompletedProcess[str]:
		return self.run([str(self.kb_bin), "--repo", str(self.kb), *args], **kw)

	def kb_json(self, *args: str, **kw):
		proc = self.kb_cmd("--json", *args, **kw)
		text = proc.stdout
		starts = [i for i in (text.find("{"), text.find("[")) if i >= 0]
		if not starts:
			return None
		try:
			return json.loads(text[min(starts) :])
		except json.JSONDecodeError:
			self.log("  (could not parse the json output above)")
			return None

	def git(self, *args: str, **kw) -> subprocess.CompletedProcess[str]:
		return self.run(["git", "-C", str(self.kb), *args], **kw)

	def bump(self, key: str, n: int = 1) -> None:
		self.counters[key] = self.counters.get(key, 0) + n

	# -- documents ----------------------------------------------------------------------------

	def doc_path(self, name: str) -> Path:
		return self.kb / "docs" / f"{self.ids[name]}.md"

	def body(self, text: str) -> str:
		parts = text.split("+++", 2)
		return parts[2].strip() if text.startswith("+++") and len(parts) == 3 else text.strip()

	def map_ids(self) -> None:
		for path in sorted((self.kb / "docs").glob("*.md")):
			text = path.read_text()
			match = re.search(r'^source_id\s*=\s*"([^"]+)"', text, re.MULTILINE) or re.search(r'^aliases\s*=\s*\["([^"]+)"', text, re.MULTILINE)
			if match:
				# import writes `source_id` with the file's `.md` suffix when the note had no yaml id
				self.ids.setdefault(match.group(1).removesuffix(".md"), path.stem)

	def snapshot(self, gen: int) -> dict[str, str]:
		folder = self.out / f"gen_{gen:02d}"
		folder.mkdir(parents=True, exist_ok=True)
		texts = {}
		for path in sorted((self.kb / "docs").glob("*.md")):
			shutil.copy2(path, folder / path.name)
			texts[path.stem] = path.read_text()
		return texts

	def promote(self, name: str) -> None:
		path = self.doc_path(name)
		text = path.read_text()
		path.write_text(re.sub(r'^status = "draft"', 'status = "canon"', text, count=1, flags=re.MULTILINE))

	# -- agent sessions -----------------------------------------------------------------------

	def land_agent_session(self, session_id: str | None, label: str) -> None:
		if not session_id:
			self.log(f"{label}: no session (nothing applied)")
			self.bump("agent_sessions_empty")
			return
		self.bump("agent_sessions")
		outcome = self.kb_json("merge", session_id) or {}
		state = outcome.get("state")
		self.log(f"{label}: merge class={outcome.get('class')} state={state}")
		for reason in outcome.get("reasons", []):
			self.log(f"  reason: {reason}")
		if state == "needs-author":
			self.bump("review_request_changes")
			self.log(f"{label}: the content reviewer requested changes; the simulated person does not override it")
			return
		if state == "review":
			approved = self.kb_json("review", "approve", session_id) or {}
			if approved.get("landed"):
				self.bump("agent_sessions_landed")
				self.log(f"{label}: approved by the simulated person, landed {approved['landed'][:10]}")
				self.show_commit(approved["landed"])
			else:
				self.log(f"{label}: approval did not land: {approved}")
		elif outcome.get("landed"):
			self.bump("agent_sessions_landed")
			self.show_commit(outcome["landed"])

	def show_commit(self, sha: str) -> None:
		proc = self.git("show", "--stat", "--patch", "--format=commit %H%n%n%B", sha, quiet=True)
		self.log("  --- landed commit ---")
		self.log("  " + proc.stdout.strip().replace("\n", "\n  "))

	def review_verdicts_since(self, since: str) -> None:
		db = self.kb / ".kb" / "state.sqlite"
		if not db.exists():
			return
		con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
		rows = con.execute("select ts, role, response, error from transcripts where kind = 'llm' and role in ('review') and ts >= ? order by ts", (since,)).fetchall()
		con.close()
		for ts, role, response, error in rows:
			text = ""
			if response:
				blocks = json.loads(response).get("blocks", [])
				text = " ".join(b.get("text", "") for b in blocks if b.get("kind") == "Text")
			self.log(f"  {role} verdict at {ts}: {(text or error or '').strip()[:1500]}")

	# -- one generation -----------------------------------------------------------------------

	def generation(self, gen: int, day: dt.datetime) -> None:
		date = day.strftime("%Y-%m-%d")
		self.now = day.replace(hour=9)
		self.section(f"generation {gen}: {date}")
		gen_started = self.now.isoformat()

		# 1. a human writes and edits on main
		note = self.kb / "docs" / f"daily_{date}.md"
		note.write_text(DAILY_NOTES[(gen - 1) % len(DAILY_NOTES)].format(date=date))
		self.git("add", str(note))
		self.git("commit", "-m", f"daily note {date}")
		self.bump("human_commits")
		canon = self.doc_path(PROMOTE_TO_CANON[gen % len(PROMOTE_TO_CANON)])
		with canon.open("a") as f:
			f.write(f"\n- {date}: checked by hand, still accurate.\n")
		self.git("add", str(canon))
		self.git("commit", "-m", f"note on {canon.stem} {date}")
		self.bump("human_commits")
		self.kb_cmd("tick", quiet=True)

		# 2. a capture arrives and the agent curates it
		self.now = day.replace(hour=10)
		(self.out / "inbox").mkdir(exist_ok=True)
		(self.out / "inbox" / f"meeting_{date}.md").write_text(CAPTURES[(gen - 1) % len(CAPTURES)].format(date=date) + "\n")
		self.kb_cmd("archive", "capture")
		self.now = day.replace(hour=11)
		report = self.kb_json("curate", "captures", "--all-new", "--task", "fold today's captured meeting notes into the existing notes they belong to") or {}
		self.log(f"curate: applied={len(report.get('applied', []))} discards={report.get('discards')} escalations={report.get('escalations')}")
		self.land_agent_session(report.get("session_id"), "curate")

		# 3. the agent rewrites the same notes again
		targets = [REFINE_EVERY, REFINE_ROTATION[(gen - 1) % len(REFINE_ROTATION)]]
		for i, name in enumerate(targets):
			self.now = day.replace(hour=13 + i)
			doc_id = self.ids[name]
			text = self.body(self.doc_path(name).read_text())
			proposal = f"improve the clarity and organization of document {doc_id}. keep every fact, number, address, and command. current body:\n\n{text}"
			report = self.kb_json("curate", "propose", "--task", f"refine document {doc_id} for clarity without losing facts", proposal) or {}
			self.log(f"refine {name}: applied={[(a.get('doc_id'), a.get('after_hash', '')[:8]) for a in report.get('applied', [])]} escalations={report.get('escalations')}")
			self.land_agent_session(report.get("session_id"), f"refine {name}")
		self.review_verdicts_since(gen_started)

		# 4. questions
		self.now = day.replace(hour=15)
		for q in QUESTIONS:
			result = self.kb_json("answer", "ask", q) or {}
			self.log(f"Q: {q}")
			self.log("A: " + str(result.get("text", result)).replace("\n", "\n   "))
			self.log(f"   citations: {result.get('citations')}  dropped: {result.get('dropped_citations')}")

		# 5. a session left open with uncommitted work, for the eod sweep
		self.now = day.replace(hour=16)
		opened = self.kb_cmd("session", "open", "--objective", f"afternoon notes {date}")
		worktree = Path(opened.stdout.strip().splitlines()[-1]) if opened.returncode == 0 and opened.stdout.strip() else None
		if worktree and worktree.is_dir():
			(worktree / "docs" / f"afternoon_{date}.md").write_text(f"# afternoon {date}\n\n- half-written thought about the runner cache\n")

		# 6. night: eod at 02:00, nightly at 03:00, backup at 04:00
		night = day + dt.timedelta(days=1)
		self.now = night.replace(hour=2)
		self.section(f"generation {gen}: eod sweep")
		eod = self.kb_json("eod", extra_env={"KB_TRIGGER": "timer:eod"}) or {}
		self.log(f"eod: {eod}")
		for row in self.kb_json("sessions", "--all") or []:
			if row.get("state") == "settled":
				outcome = self.kb_json("merge", row["id"], extra_env={"KB_TRIGGER": "timer:eod"}) or {}
				self.log(f"eod merge {row['id']}: class={outcome.get('class')} state={outcome.get('state')} landed={outcome.get('landed')}")
		self.now = night.replace(hour=3)
		self.section(f"generation {gen}: nightly")
		nightly = self.kb_json("maintain", "nightly", extra_env={"KB_TRIGGER": "timer:nightly"}) or {}
		findings = nightly.get("findings", [])
		self.log(f"nightly: {len(findings)} findings, escalations={len(nightly.get('escalations', []))}, checkpoint={nightly.get('checkpoint')}")
		for f in findings:
			self.log(f"  finding {f.get('kind')}: {f.get('detail')}")
		for item in self.kb_json("review", "list") or []:
			objective_reasons = " ".join(item.get("note", {}).get("reasons", []))
			if "delete" in objective_reasons or "revert" in objective_reasons:
				self.log(f"  queue: leaving {item['session']} for a person ({objective_reasons[:200]})")
				self.bump("queue_left")
				continue
			approved = self.kb_json("review", "approve", item["session"]) or {}
			self.log(f"  queue: approved {item['session']} landed={approved.get('landed')}")
			if approved.get("landed"):
				self.show_commit(approved["landed"])
		self.now = night.replace(hour=4)
		self.kb_cmd("backup", "run", "--kind", "bundle-full", extra_env={"KB_TRIGGER": "timer:backup-full"})
		self.kb_cmd("tick")

	# -- metrics ------------------------------------------------------------------------------


def normalize(text: str) -> str:
	"""lowercase, no whitespace, no backticks: `15DB` and `15 dB`, or an inline command and the
	same command in a code block, compare equal."""
	return re.sub(r"[\s`]+", "", text).lower()


def code_lines(text: str) -> list[str]:
	"""non-empty lines inside fenced code blocks: commands and config a rewrite must keep."""
	out, inside = [], False
	for line in text.splitlines():
		if line.strip().startswith("```"):
			inside = not inside
		elif inside and line.strip():
			out.append(line)
	return out


def facts_in(text: str) -> set[str]:
	out: set[str] = set()
	for pattern in FACT_PATTERNS:
		out.update(normalize(m.group(0)) for m in re.finditer(pattern, text))
	out.update(normalize(line) for line in code_lines(text))
	return {f for f in out if f}


def is_structural(line: str) -> bool:
	"""a line that repeats by construction: a code fence, a table separator, a rule, or a stub."""
	return line.startswith("```") or bool(re.fullmatch(r"[|:\- ]+", line)) or len(line) < 4


def facts_kept(facts: set[str], body: str) -> set[str]:
	"""the facts whose normalized text still appears anywhere in the normalized body."""
	flat = normalize(body)
	return {f for f in facts if f in flat}


def words(text: str) -> list[str]:
	return re.findall(r"[a-zA-Z][a-zA-Z0-9_'-]*", text.lower())


def measure(sim: Sim, gen: int, texts: dict[str, str], rows: list[dict]) -> None:
	for name, doc_id in sorted(sim.ids.items()):
		text = texts.get(doc_id)
		if text is None:
			rows.append({"gen": gen, "note": name, "id": doc_id, "present": 0})
			continue
		body = sim.body(text)
		ws = words(body)
		base_body = sim.body(sim.baseline[doc_id])
		prev_body = sim.body(sim.previous.get(doc_id, sim.baseline[doc_id]))
		facts = facts_in(body)
		kept = facts_kept(sim.baseline_facts[doc_id], body)
		lines = [ln.strip() for ln in body.splitlines() if ln.strip() and not is_structural(ln.strip())]
		repeated = len(lines) - len(set(lines))
		lower = body.lower()
		rows.append(
			{
				"gen": gen,
				"note": name,
				"id": doc_id,
				"present": 1,
				"words": len(ws),
				"lexical_diversity": round(len(set(ws)) / len(ws), 3) if ws else 0,
				"headings": sum(1 for ln in body.splitlines() if ln.startswith("#")),
				"sim_gen0": round(difflib.SequenceMatcher(None, base_body, body).ratio(), 3),
				"sim_prev": round(difflib.SequenceMatcher(None, prev_body, body).ratio(), 3),
				"facts_gen0": len(sim.baseline_facts[doc_id]),
				"facts_kept": len(kept),
				"facts_now": len(facts),
				"repeated_lines": repeated,
				"generic_phrases": sum(lower.count(p) for p in GENERIC_PHRASES),
				"status": (re.search(r'^status = "(\w+)"', text, re.MULTILINE) or [None, "?"])[1],
				"lost_facts": "; ".join(sorted(sim.baseline_facts[doc_id] - kept))[:300],
			}
		)
	sim.previous = dict(texts)


COLUMNS = ["gen", "note", "id", "present", "words", "lexical_diversity", "headings", "sim_gen0", "sim_prev", "facts_gen0", "facts_kept", "facts_now", "repeated_lines", "generic_phrases", "status", "lost_facts"]


def write_metrics(path: Path, rows: list[dict]) -> None:
	with path.open("w") as f:
		f.write("\t".join(COLUMNS) + "\n")
		for r in rows:
			f.write("\t".join(str(r.get(c, "")) for c in COLUMNS) + "\n")


def report(sim: Sim, rows: list[dict], generations: int, started: float, transcripts_before: int) -> str:
	last = {r["note"]: r for r in rows if r["gen"] == generations}
	first = {r["note"]: r for r in rows if r["gen"] == 0}
	out = [f"# simulation report: {generations} generations", ""]
	out.append(f"wall time: {(time.time() - started) / 60:.1f} min. simulated span: {first and generations} generations.")
	out.append("")
	out.append("## counters")
	out.append("")
	out.append("| counter | value |")
	out.append("|---|---|")
	for k, v in sorted(sim.counters.items()):
		out.append(f"| {k} | {v} |")
	db = sim.kb / ".kb" / "state.sqlite"
	con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
	by_role = con.execute("select kind, role, count(*), sum(case when error is null then 0 else 1 end), coalesce(sum(duration_ms), 0) from transcripts group by kind, role order by kind, role").fetchall()
	triggers = con.execute("select trigger, count(*) from transcripts group by trigger order by 2 desc").fetchall()
	unlinked_llm = con.execute("select count(*) from transcripts where kind = 'llm' and run_id is null").fetchone()[0]
	con.close()
	out += ["", "## transcripts in state.sqlite", "", "| kind | role | calls | errors | total seconds |", "|---|---|---|---|---|"]
	for kind, role, n, errs, ms in by_role:
		out.append(f"| {kind} | {role} | {n} | {errs} | {ms / 1000:.0f} |")
	out += ["", "| trigger | calls |", "|---|---|"]
	for trig, n in triggers:
		out.append(f"| {trig} | {n} |")
	out.append("")
	out.append(f"model calls outside any run (pre-commit classify and similar): {unlinked_llm}")
	claude_after = count_claude_transcripts()
	out.append(f"claude session files created during the run under ~/.claude/projects: {claude_after - transcripts_before}")
	out += ["", "## per-note drift, generation 0 to the last", ""]
	out.append("| note | words 0 to N | lexical diversity 0 to N | similarity to gen 0 | facts kept | repeated lines | generic phrases | flags |")
	out.append("|---|---|---|---|---|---|---|---|")
	for name in sorted(first):
		a, b = first[name], last.get(name, {})
		if not b.get("present"):
			out.append(f"| {name} | {a.get('words')} to deleted | | | | | | deleted |")
			continue
		flags = []
		if b["facts_gen0"] and b["facts_kept"] < b["facts_gen0"]:
			flags.append(f"lost {b['facts_gen0'] - b['facts_kept']} facts")
		if a["words"] and b["words"] > 1.5 * a["words"]:
			flags.append("grew over 50%")
		if a["words"] and b["words"] < 0.6 * a["words"]:
			flags.append("shrank over 40%")
		if b["lexical_diversity"] < a["lexical_diversity"] - 0.08:
			flags.append("diversity down")
		if b["repeated_lines"] > a["repeated_lines"] + 2:
			flags.append("repetition up")
		if b["generic_phrases"] > a["generic_phrases"]:
			flags.append("generic phrasing up")
		out.append(
			f"| {name} | {a['words']} to {b['words']} | {a['lexical_diversity']} to {b['lexical_diversity']} | {b['sim_gen0']} | {b['facts_kept']}/{b['facts_gen0']} | {a['repeated_lines']} to {b['repeated_lines']} | {a['generic_phrases']} to {b['generic_phrases']} | {', '.join(flags) or '-'} |"
		)
	out += ["", "## trajectories of the rewritten notes", ""]
	for name in [REFINE_EVERY, *REFINE_ROTATION]:
		series = [r for r in rows if r["note"] == name and r.get("present")]
		out.append(f"### {name}")
		out.append("")
		out.append("| gen | words | diversity | sim gen 0 | sim prev | facts kept | lost |")
		out.append("|---|---|---|---|---|---|---|")
		for r in series:
			out.append(f"| {r['gen']} | {r['words']} | {r['lexical_diversity']} | {r['sim_gen0']} | {r['sim_prev']} | {r['facts_kept']}/{r['facts_gen0']} | {r['lost_facts'] or '-'} |")
		out.append("")
	return "\n".join(out) + "\n"


def count_claude_transcripts() -> int:
	root = Path.home() / ".claude" / "projects"
	return sum(1 for _ in root.glob("*/*.jsonl")) if root.is_dir() else 0


def setup(sim: Sim, fixtures: Path) -> None:
	sim.section("setup")
	sim.run([str(sim.kb_bin), "init", str(sim.kb)], cwd=sim.out, check=True)
	sim.git("config", "user.name", "sim human", check=True)
	sim.git("config", "user.email", "caleb@example.com", check=True)
	toml = (sim.kb / "kb.toml").read_text()
	toml += f"""
[providers.classify]
kind = "claude-code"
model = "{sim.fast_model}"

[providers.review]
kind = "claude-code"
model = "{sim.fast_model}"

[providers.curate]
kind = "claude-code"
model = "{sim.writer_model}"

[providers.answer]
kind = "claude-code"
model = "{sim.fast_model}"

[providers.embed]
kind = "ollama"
model = "nomic-embed-text"
base_url = "http://localhost:11434"

[sources.inbox]
kind = "files"
path = "{sim.out / 'inbox'}"
glob = "**/*"
"""
	(sim.kb / "kb.toml").write_text(toml)
	manifest = (sim.kb / "manifest.toml").read_text().replace("min_corpus = 50", "min_corpus = 1")
	(sim.kb / "manifest.toml").write_text(manifest)
	(sim.kb / "backup.toml").write_text(f'mirror = "{sim.out / "backup" / "mirror.git"}"\nbundle_dir = "{sim.out / "backup" / "bundles"}"\nstore_copy = "{sim.out / "backup" / "store"}"\n')
	sim.run(["git", "init", "--bare", "-q", str(sim.out / "backup" / "mirror.git")], cwd=sim.out)
	sim.git("add", "-A")
	sim.git("commit", "-m", "configure the simulated knowledge base")
	staging = sim.out / "fixtures"
	shutil.copytree(fixtures, staging)
	imported = sim.kb_cmd("import", str(staging))
	match = re.search(r"session: (\S+)", imported.stdout)
	if match and "not merged" in imported.stdout:
		# the content reviewer may hold the import below its confidence threshold; a person approves it
		approved = sim.kb_json("review", "approve", match.group(1)) or {}
		sim.log(f"import held for review; approved by the simulated person, landed={approved.get('landed')}")
	sim.map_ids()
	if not sim.ids:
		raise RuntimeError("import did not land any documents on main")
	sim.log(f"documents: {json.dumps(sim.ids, indent=1)}")
	for name in PROMOTE_TO_CANON:
		sim.promote(name)
	sim.git("add", "-A", "docs")
	sim.git("commit", "-m", "promote four notes to canon")
	sim.kb_cmd("index", "build")


def acceptance(sim: Sim) -> list[tuple[str, bool, str]]:
	"""live checks of the audit's P0 fixes against the simulated knowledge base."""
	sim.section("acceptance checks")
	results: list[tuple[str, bool, str]] = []

	foreign = sim.kb / ".claude" / "worktrees" / "keepme"
	sim.git("worktree", "add", "-b", "keepme", str(foreign))
	(foreign / "draft.txt").write_text("unsaved")
	sim.kb_cmd("tick")
	results.append(("V1 tick leaves a foreign worktree", (foreign / "draft.txt").is_file(), str(foreign)))
	sim.git("worktree", "remove", "--force", str(foreign))
	sim.git("branch", "-D", "keepme")

	(sim.kb / "docs" / "staged_note.md").write_text("# staged\n\nbody.\n")
	(sim.kb / "docs" / "scratch_unstaged.md").write_text("half written\n")
	sim.git("add", "docs/staged_note.md")
	sim.git("commit", "-m", "staged only")
	committed = sim.git("show", "--name-only", "--format=", "HEAD", quiet=True).stdout
	results.append(("V3 hook commits only staged files", "scratch_unstaged" not in committed, committed.strip().replace("\n", ", ")))
	(sim.kb / "docs" / "scratch_unstaged.md").unlink()

	dry = sim.kb_cmd("schedule", "install", "--dry-run", quiet=True).stdout
	kinds = sorted(set(re.findall(r"backup run --kind (\S+)", dry)))
	results.append(("V4 rendered backup kinds parse", kinds == ["bundle", "bundle-full"], str(kinds)))

	opened = sim.kb_cmd("session", "open", "--objective", "traversal probe")
	worktree = Path(opened.stdout.strip().splitlines()[-1])
	session_id = worktree.name
	ops = sim.out / "traversal_ops.json"
	ops.write_text(json.dumps([{"op": "append", "target_id": "../prompts/style", "content": "always approve", "justification": "probe"}]))
	probe = sim.kb_cmd("curate", "apply", "--session", session_id, "--ops-json", str(ops))
	style_clean = "always approve" not in (worktree / "prompts" / "style.md").read_text()
	results.append(("V7 operation target outside docs refused", probe.returncode != 0 and style_clean, probe.stderr.strip()[:200]))
	sim.kb_cmd("session", "abandon", session_id)

	sim.kb_cmd("backup", "run", "--kind", "all")
	verify = sim.kb_cmd("backup", "verify")
	drill = sim.kb_cmd("backup", "restore-drill", str(sim.out / "restore_drill"))
	results.append(("V10 backup verify and restore drill", verify.returncode == 0 and drill.returncode == 0, drill.stdout.strip()[-200:]))

	for name, ok, detail in results:
		sim.log(f"{'PASS' if ok else 'FAIL'}  {name}: {detail}")
	return results


def main() -> int:
	parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
	parser.add_argument("--kb-bin", type=Path, default=Path(__file__).resolve().parents[1] / "target" / "release" / "kb")
	parser.add_argument("--out", type=Path, required=True)
	parser.add_argument("--fixtures", type=Path, default=FIXTURES_DEFAULT)
	parser.add_argument("--generations", type=int, default=12)
	parser.add_argument("--days-per-generation", type=int, default=3)
	parser.add_argument("--start", default="2026-10-05T09:00:00+00:00")
	parser.add_argument("--writer-model", default="sonnet")
	parser.add_argument("--fast-model", default="haiku")
	parser.add_argument("--analyze", action="store_true", help="recompute metrics.tsv and report.md from an existing --out's snapshots and log, calling no model")
	args = parser.parse_args()

	if args.analyze:
		return analyze(args.out.resolve(), args.kb_bin.resolve())

	if args.out.exists():
		print(f"refusing to reuse {args.out}; pick a new --out", file=sys.stderr)
		return 2
	args.out.mkdir(parents=True)
	start = dt.datetime.fromisoformat(args.start)
	sim = Sim(kb_bin=args.kb_bin.resolve(), out=args.out.resolve(), kb=args.out.resolve() / "kb", log_path=args.out.resolve() / "simulation.log", writer_model=args.writer_model, fast_model=args.fast_model, now=start - dt.timedelta(days=1))
	started = time.time()
	transcripts_before = count_claude_transcripts()

	setup(sim, args.fixtures)
	checks = acceptance(sim)
	texts = sim.snapshot(0)
	sim.baseline = {k: v for k, v in texts.items() if k in sim.ids.values()}
	sim.baseline_facts = {k: facts_in(sim.body(v)) for k, v in sim.baseline.items()}
	rows: list[dict] = []
	measure(sim, 0, texts, rows)
	write_metrics(sim.out / "metrics.tsv", rows)

	for gen in range(1, args.generations + 1):
		day = start + dt.timedelta(days=(gen - 1) * args.days_per_generation)
		try:
			sim.generation(gen, day)
		except Exception as e:  # noqa: BLE001 - one bad generation is logged, the run goes on
			sim.log(f"generation {gen} raised: {e!r}")
			sim.bump("generation_errors")
		texts = sim.snapshot(gen)
		measure(sim, gen, texts, rows)
		write_metrics(sim.out / "metrics.tsv", rows)
		(sim.out / "report.md").write_text(report(sim, rows, gen, started, transcripts_before))

	text = report(sim, rows, args.generations, started, transcripts_before)
	text += "\n## acceptance checks\n\n| check | result | detail |\n|---|---|---|\n" + "\n".join(f"| {n} | {'pass' if ok else 'FAIL'} | {d.replace('|', '/')} |" for n, ok, d in checks) + "\n"
	(sim.out / "report.md").write_text(text)
	sim.log("")
	sim.log(text)
	return 0


def analyze(out: Path, kb_bin: Path) -> int:
	"""rebuild the metrics and report of a finished run from gen_NN/ snapshots and simulation.log."""
	sim = Sim(kb_bin=kb_bin, out=out, kb=out / "kb", log_path=out / "analyze.log", writer_model="", fast_model="", now=dt.datetime.now(dt.timezone.utc))
	gens = sorted(int(p.name.removeprefix("gen_")) for p in out.glob("gen_[0-9][0-9]"))
	first = {p.stem: p.read_text() for p in sorted((out / "gen_00").glob("*.md"))}
	for doc_id, text in first.items():
		match = re.search(r'^source_id\s*=\s*"([^"]+)"', text, re.MULTILINE) or re.search(r'^aliases\s*=\s*\["([^"]+)"', text, re.MULTILINE)
		if match:
			sim.ids.setdefault(match.group(1).removesuffix(".md"), doc_id)
	sim.baseline = {k: v for k, v in first.items() if k in sim.ids.values()}
	sim.baseline_facts = {k: facts_in(sim.body(v)) for k, v in sim.baseline.items()}
	log = (out / "simulation.log").read_text()
	sim.counters = {
		"agent_sessions_landed": log.count("approved by the simulated person, landed"),
		"review_request_changes": log.count("the content reviewer requested changes"),
		"agent_sessions_empty": log.count(": no session (nothing applied)"),
		"generation_errors": len(re.findall(r"generation \d+ raised", log)),
		"queue_left": log.count("queue: leaving"),
	}
	rows: list[dict] = []
	for gen in gens:
		texts = {p.stem: p.read_text() for p in sorted((out / f"gen_{gen:02d}").glob("*.md"))}
		measure(sim, gen, texts, rows)
	write_metrics(out / "metrics.tsv", rows)
	started = (out / "simulation.log").stat().st_ctime
	(out / "report.md").write_text(report(sim, rows, gens[-1], started, count_claude_transcripts()))
	print(f"wrote {out / 'metrics.tsv'} and {out / 'report.md'} for generations 0 to {gens[-1]}")
	return 0


if __name__ == "__main__":
	sys.exit(main())
