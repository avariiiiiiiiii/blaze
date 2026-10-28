#!/usr/bin/env python3
"""promote.py: a canary release becomes stable once it passed the heavy tests (docs/release.md).

    promote.py which '{"tag": ""}'
        the release promote.yaml tests: `tag`, or the newest dated release when it is empty;
        step outputs `tag`, `sha`, `prerelease`
    promote.py promote '{"tag": "v0.YYYYMMDD.N", "run": "<run URL>"}'
        mark the release stable (not a prerelease) and Latest, unless a newer release is stable
        already; append the line saying so to its notes
    promote.py issue '{"tag": ..., "run": ..., "results": "<dir>", "needs": {<job>: {"result": ...}}}'
        open an issue for a canary that failed, or comment on the open one of that release: the
        jobs of `needs` (the workflow's) that failed, and each failed case of the smoke.py
        results files under `results`, with the command that repeats it
    promote.py pointer '{"channel": "canary", "tag": "v0.YYYYMMDD.N"}'
        the channel's pointer names the release (release.yaml, for canary)

Each channel has a pointer: the asset `stable` or `canary` of the release `channels`, which holds
the channel's version and a newline. A workspace pinned to a channel (`[workspace.metadata.blaze]
version = "stable"`) reads it with one download. Made with the release `channels` the first time,
from what the releases are then.

The JSON is inline, a file's path, or `-` for stdin. The same tag is flipped: nothing is built
or uploaded again, so a version names the same bytes before and after. Needs gh (GH_TOKEN).
Python 3.11+, stdlib.
"""
import datetime, glob, json, os, re, subprocess, sys, tempfile, typing
from dataclasses import dataclass, fields

REPO = os.environ.get("GITHUB_REPOSITORY", "indexable-inc/blaze")
DATED = re.compile(r"v(\d+)\.(\d+)\.(\d+)")


@dataclass(frozen=True)
class Which:
    tag: str = ""


@dataclass(frozen=True)
class Promote:
    tag: str
    run: str


@dataclass(frozen=True)
class Issue:
    tag: str
    run: str
    results: str
    needs: dict


@dataclass(frozen=True)
class Pointer:
    channel: typing.Literal["stable", "canary"]
    tag: str


POINTERS = "channels"


def typed(cls, text):
    """`cls` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"promote.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("promote.py: the input is a JSON object")
    known = {f.name: f for f in fields(cls)}
    if unknown := sorted(set(value) - set(known)):
        sys.exit(f"promote.py: no field {', '.join(unknown)} (fields: {', '.join(known)})")
    hints = typing.get_type_hints(cls)
    for name, given in value.items():
        hint = hints[name]
        if typing.get_origin(hint) is typing.Literal:
            if given not in typing.get_args(hint):
                sys.exit(f"promote.py: field {name} is one of {', '.join(map(repr, typing.get_args(hint)))}, not {given!r}")
        elif not isinstance(given, hint):
            sys.exit(f"promote.py: field {name} is a {hint.__name__}, not {given!r}")
    try:
        return cls(**value)
    except TypeError as e:
        sys.exit(f"promote.py: {e}")


def gh(*args, input=None):
    done = subprocess.run(["gh", *args], capture_output=True, text=True, input=input)
    if done.returncode:
        sys.exit(f"promote.py: `gh {' '.join(args)}` exited {done.returncode}: {done.stderr.strip()}")
    return done.stdout


def key(tag):
    return tuple(int(n) for n in DATED.fullmatch(tag).groups())


def releases():
    """Every dated release, as {tag: {isPrerelease, isLatest, ...}}."""
    found = json.loads(gh("release", "list", "-R", REPO, "--limit", "200", "--json", "tagName,isPrerelease,isLatest,isDraft"))
    return {r["tagName"]: r for r in found if DATED.fullmatch(r["tagName"]) and not r["isDraft"]}


def output(**values):
    lines = "".join(f"{k}={v}\n" for k, v in values.items())
    print(lines, end="")
    if path := os.environ.get("GITHUB_OUTPUT"):
        with open(path, "a") as f:
            f.write(lines)


def which(tag):
    known = releases()
    if not known:
        sys.exit("promote.py: no dated release yet")
    tag = tag or max(known, key=key)
    if tag not in known:
        sys.exit(f"promote.py: no release {tag} (dated releases: {', '.join(sorted(known, key=key)[-5:])})")
    sha = gh("api", f"repos/{REPO}/commits/{tag}", "-q", ".sha").strip()
    output(tag=tag, sha=sha, prerelease=str(known[tag]["isPrerelease"]).lower())


def promote(tag, run_url):
    known = releases()
    if not known.get(tag, {}).get("isPrerelease"):
        print(f"{tag} is stable already: nothing to do")
        return
    stable = [t for t, r in known.items() if not r["isPrerelease"]]
    newest_stable = max(stable, key=key) if stable else None
    # Stable never moves backwards: an older canary that passes late is stable, not Latest.
    latest = newest_stable is None or key(tag) > key(newest_stable)
    today = datetime.datetime.now(datetime.UTC).date().isoformat()
    body = json.loads(gh("release", "view", tag, "-R", REPO, "--json", "body"))["body"].rstrip()
    line = f"Promoted to stable on {today} after {run_url}."
    if not latest:
        line += f" {newest_stable} was stable already, so it stays Latest."
    gh("release", "edit", tag, "-R", REPO, "--prerelease=false", f"--latest={'true' if latest else 'false'}",
       "--notes-file", "-", input=f"{body}\n\n---\n\n{line}\n")
    if latest:
        pointer("stable", tag)
    print(line)


def pointer(channel, tag):
    """The pointer of `channel` names `tag`; the release `channels` is made first when there is none."""
    known = releases()
    if tag not in known:
        sys.exit(f"promote.py: no release {tag} for the {channel} pointer")
    made = subprocess.run(["gh", "release", "view", POINTERS, "-R", REPO], capture_output=True).returncode == 0
    files = {channel: tag}
    if not made:
        stable = [t for t, r in known.items() if not r["isPrerelease"]]
        files = {"stable": max(stable, key=key) if stable else tag, "canary": max(known, key=key), **files}
        gh("release", "create", POINTERS, "-R", REPO, "--prerelease", "--latest=false", "--target", "main",
           "--title", "blaze's channels",
           "--notes", "Which release each channel is (docs/release.md): the assets `stable` and `canary` hold "
                      "its version. Updated by release.yaml and promote.yaml; a pin to a channel reads them.")
    with tempfile.TemporaryDirectory() as dir:
        paths = []
        for name, value in files.items():
            path = os.path.join(dir, name)
            with open(path, "w") as f:
                f.write(value.lstrip("v") + "\n")
            paths.append(path)
        gh("release", "upload", POINTERS, "-R", REPO, "--clobber", *paths)
    print(", ".join(f"{name} -> {value}" for name, value in files.items()))


def issue(tag, run_url, results, needs):
    failed_jobs = [name for name, need in needs.items() if need.get("result") not in ("success", "skipped")]
    lines = [f"Canary [{tag}](https://github.com/{REPO}/releases/tag/{tag}) failed the gate to stable, "
             f"so it stays a prerelease ([run]({run_url})). docs/release.md says what the gate is.", ""]
    if failed_jobs:
        lines += ["Jobs that did not pass: " + ", ".join(f"`{j}`" for j in failed_jobs), ""]
    for path in sorted(glob.glob(os.path.join(results, "**", "*.json"), recursive=True)):
        data = json.load(open(path))
        platform = os.path.splitext(os.path.basename(path))[0]
        bad = [r for r in data["results"] if not r["ok"] and not r["open"]]
        if not bad:
            continue
        lines += [f"### smoke corpus, {platform} ({data['blaze']})", ""]
        for r in bad:
            lines += [f"**{r['what']}**: {r['why']}", "", f"Install it and repeat:", "", "```sh",
                      f"curl -fsSL https://ix.dev/blaze | sh -s -- --version {tag.lstrip('v')}", r["repro"], "```", ""]
            if r["stderr"]:
                lines += ["<details><summary>stderr</summary>", "", "```", r["stderr"][-4000:], "```", "", "</details>", ""]
    body = "\n".join(lines)[:60000]
    title = f"Canary {tag} is not promoted to stable"
    found = json.loads(gh("issue", "list", "-R", REPO, "--state", "open", "--search", f"in:title \"{title}\"",
                          "--json", "number,title"))
    same = [i for i in found if i["title"] == title]
    if same:
        gh("issue", "comment", str(same[0]["number"]), "-R", REPO, "--body-file", "-", input=body)
        print(f"commented on #{same[0]['number']}")
    else:
        print(gh("issue", "create", "-R", REPO, "--title", title, "--label", "bug", "--body-file", "-", input=body).strip())


def main():
    args = sys.argv[1:]
    step, given = (args + [""])[:2] if len(args) <= 2 else ("", "")
    if step == "which":
        which(typed(Which, given).tag)
    elif step == "promote":
        it = typed(Promote, given)
        promote(it.tag, it.run)
    elif step == "issue":
        it = typed(Issue, given)
        issue(it.tag, it.run, it.results, it.needs)
    elif step == "pointer":
        it = typed(Pointer, given)
        pointer(it.channel, it.tag)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
