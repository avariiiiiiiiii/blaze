#!/usr/bin/env python3
"""land.py: lands the current branch on main through the merge queue, and waits for it.

    python3 .github/scripts/land.py "what the branch does" [--no-wait]
    python3 .github/scripts/land.py --wait [PR]

Run it in the lane's worktree, on its branch, with everything committed and the local gate
passed. It pushes the branch, opens its pull request (titled "Merge <branch>: <what it does>",
which with " (#N)" is the merge commit's subject) or takes the one already open, and asks for it
to be queued once its own checks pass: the pull request's run is the merge gate, the build and
the unit tests on Linux x86-64 (about 5 minutes). The queue then runs the gate on the branch
merged onto main and the entries ahead of it, and merges it with a merge commit, as `git merge
--no-ff` would; every harness runs after that, in the release train (train.yaml). Main moves only to
what it tested. land.py waits until then and exits 0 with the commit main is at, or exits 1 and
says why the pull request was not queued or left the queue (a check that failed, a conflict
with main). GitHub
deletes the branch on origin once it is merged.

The waiting is done by a process of its own, detached from the terminal and from whatever ran
land.py, which writes what it sees to <git common dir>/land/pr-N.log; land.py follows that log.
So land.py killed, or run in the background by a tool that ends its process group, leaves the
waiting going. `land.py --wait` (this branch's pull request) or `land.py --wait N` follows it
again, and starts it again if it is gone; running land.py again on the same branch does too.
"""
import json, os, signal, subprocess, sys, time

REPO = "indexable-inc/blaze"
POLL = 20
GIVE_UP = 3 * 3600
# A `gh` call that fails this many times in a row (the network, GitHub) ends the waiting.
TRIES = 15
EXIT = "land.py exit "


def run(*args, check=True):
    done = subprocess.run(args, capture_output=True, text=True)
    if check and done.returncode:
        sys.exit(f"land.py: {' '.join(args)} failed:\n{done.stderr.strip() or done.stdout.strip()}")
    return done.stdout.strip()


class GhFailed(Exception):
    pass


def gh(*args):
    done = subprocess.run(["gh", *args], capture_output=True, text=True)
    if done.returncode:
        raise GhFailed(done.stderr.strip() or done.stdout.strip())
    return done.stdout.strip()


def state(number):
    query = """query($number: Int!) { repository(owner: "indexable-inc", name: "blaze") {
      pullRequest(number: $number) { state mergeable isInMergeQueue url headRefName
        mergeQueueEntry { position state } autoMergeRequest { enabledAt } mergeCommit { oid } } } }"""
    out = gh("api", "graphql", "-f", f"query={query}", "-F", f"number={number}")
    return json.loads(out)["data"]["repository"]["pullRequest"]


def failed_checks(number):
    # `gh pr checks` exits non-zero while checks fail or are pending; its output is still the list.
    out = run("gh", "pr", "checks", str(number), "--repo", REPO, "--required", "--json", "name,bucket,link", check=False)
    try:
        return [c for c in json.loads(out or "[]") if c["bucket"] == "fail"]
    except json.JSONDecodeError:
        return []


def queue_runs(number):
    """The merge-queue runs of this pull request that failed, newest first."""
    try:
        out = gh("run", "list", "--repo", REPO, "--workflow", "ra-jit.yaml", "--event", "merge_group",
                 "--limit", "40", "--json", "headBranch,conclusion,url")
    except GhFailed:
        return []
    runs = json.loads(out or "[]")
    return [r["url"] for r in runs if f"/pr-{number}-" in r["headBranch"] and r["conclusion"] == "failure"]


def files(number):
    """The log and pid file of the waiting for pull request `number`, in the git common
    directory, which outlives the lane's worktree."""
    common = os.path.abspath(run("git", "rev-parse", "--git-common-dir"))
    directory = os.path.join(common, "land")
    os.makedirs(directory, exist_ok=True)
    return os.path.join(directory, f"pr-{number}.log"), os.path.join(directory, f"pr-{number}.pid")


def alive(pid_file):
    try:
        pid = int(open(pid_file).read().strip())
        os.kill(pid, 0)
        return pid
    except (OSError, ValueError):
        return None


def wait(number):
    """The waiting itself, in the detached process: one line to the log for each change, then
    `land.py exit N`."""
    def say(line):
        print(f"{time.strftime('%H:%M:%S')} {line}", flush=True)

    def end(code, line):
        say(line)
        print(f"{EXIT}{code}", flush=True)
        sys.exit(code)

    said, gone, failures, start = None, 0, 0, time.monotonic()
    while time.monotonic() - start < GIVE_UP:
        try:
            pr = state(number)
            failures = 0
        except GhFailed as e:
            failures += 1
            if failures >= TRIES:
                end(1, f"land.py: gh failed {TRIES} times in a row, last: {e}")
            time.sleep(POLL)
            continue
        url, branch = pr["url"], pr["headRefName"]
        if pr["state"] == "MERGED":
            end(0, f"merged: main is at {pr['mergeCommit']['oid'][:10]} or after it")
        if pr["state"] == "CLOSED":
            end(1, f"land.py: {url} was closed")
        if pr["isInMergeQueue"]:
            entry = pr["mergeQueueEntry"] or {}
            now, gone = f"in the merge queue, position {entry.get('position')} ({str(entry.get('state')).lower()})", 0
        elif pr["autoMergeRequest"]:
            if pr["mergeable"] == "CONFLICTING":
                end(1, f"land.py: {branch} conflicts with main: merge origin/main into it, then run land.py again")
            failed = failed_checks(number)
            if failed:
                end(1, "land.py: the pull request's checks failed, so it was not queued:\n"
                    + "\n".join(f"  {c['name']}: {c['link']}" for c in failed))
            now, gone = "waiting for the pull request's checks", 0
        else:
            # Out of the queue and not waiting for it: read twice, as the entry moves between the two.
            gone += 1
            if gone >= 2:
                lines = [f"land.py: {url} left the merge queue without merging."]
                lines += [f"  failed in the queue: {u}" for u in queue_runs(number)[:1]]
                lines += ["  Fix it on the branch, then run land.py again."]
                end(1, "\n".join(lines))
            now = said
        if now != said:
            say(now)
            said = now
        time.sleep(POLL)
    end(1, f"land.py: gave up waiting after {GIVE_UP // 3600} hours; pull request #{number} may still be queued")


def detach(number, log, pid_file):
    """Starts the waiting for `number` in a process of its own session, its parent gone, so
    neither a closed terminal nor the end of the caller's process group or tree ends it."""
    script = os.path.abspath(__file__)
    if not hasattr(os, "fork"):
        flags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.DETACHED_PROCESS
        with open(log, "a") as out:
            child = subprocess.Popen([sys.executable, script, "--waiter", str(number)], stdin=subprocess.DEVNULL,
                                     stdout=out, stderr=subprocess.STDOUT, creationflags=flags)
        open(pid_file, "w").write(str(child.pid))
        return
    sys.stdout.flush()
    if os.fork():
        # The pid file is written once the first child has exited.
        os.wait()
        return
    os.setsid()
    grandchild = os.fork()
    if grandchild:
        open(pid_file, "w").write(str(grandchild))
        os._exit(0)
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    fd = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    null = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null, 0)
    os.dup2(fd, 1)
    os.dup2(fd, 2)
    os.execv(sys.executable, [sys.executable, script, "--waiter", str(number)])


def follow(number):
    """Follows the waiting for `number`, starting it if no process is waiting, until it ends;
    exits as it does."""
    log, pid_file = files(number)
    print(f"land.py: waiting in the background; log {log} (land.py --wait {number} follows it again)", flush=True)
    if not alive(pid_file):
        if os.path.exists(log) and EXIT in open(log).read():
            # A waiting that ended before: its outcome stands unless the pull request moved on.
            os.rename(log, log + ".old")
        detach(number, log, pid_file)
    position, idle = 0, 0
    while True:
        text = open(log).read() if os.path.exists(log) else ""
        new, position = text[position:], len(text)
        for line in new.splitlines():
            if line.startswith(EXIT):
                sys.exit(int(line[len(EXIT):]))
            print(line, flush=True)
        if not new and not alive(pid_file):
            idle += 1
            if idle >= 3:
                sys.exit(f"land.py: the waiting process ended without an outcome; see {log}, then land.py --wait {number}")
        else:
            idle = 0
        time.sleep(1)


def main():
    if sys.argv[1:2] == ["--waiter"]:
        return wait(int(sys.argv[2]))
    if sys.argv[1:2] == ["--wait"]:
        if len(sys.argv) > 2:
            return follow(int(sys.argv[2]))
        branch = run("git", "rev-parse", "--abbrev-ref", "HEAD")
        found = json.loads(run("gh", "pr", "list", "--repo", REPO, "--head", branch, "--state", "all",
                               "--limit", "1", "--json", "number"))
        if not found:
            sys.exit(f"land.py: {branch} has no pull request")
        return follow(found[0]["number"])
    args = [a for a in sys.argv[1:] if a != "--no-wait"]
    if len(args) != 1 or args[0].startswith("-"):
        sys.exit(__doc__)
    what, wait_for_it = args[0], "--no-wait" not in sys.argv
    branch = run("git", "rev-parse", "--abbrev-ref", "HEAD")
    if branch in ("HEAD", "main"):
        sys.exit("land.py: check out the lane's branch first; main is moved by the merge queue alone")
    if run("git", "status", "--porcelain", "--untracked-files=no"):
        sys.exit("land.py: commit or drop the changes in the worktree first; the queue lands only what is committed")
    run("git", "fetch", "-q", "origin", "main")
    if not run("git", "rev-list", "origin/main..HEAD"):
        sys.exit(f"land.py: {branch} has nothing that main does not have")

    pushed = subprocess.run(["git", "push", "-q", "-u", "origin", f"HEAD:refs/heads/{branch}"], capture_output=True, text=True)
    if pushed.returncode:
        sys.exit(f"land.py: origin's {branch} has commits this one does not (git pull, or push it yourself):\n{pushed.stderr.strip()}")

    found = json.loads(run("gh", "pr", "list", "--repo", REPO, "--head", branch, "--state", "open", "--json", "number,url"))
    if found:
        number, url = found[0]["number"], found[0]["url"]
    else:
        body = run("git", "log", "--no-merges", "--format=- %s", "origin/main..HEAD")
        url = run("gh", "pr", "create", "--repo", REPO, "--base", "main", "--head", branch,
                  "--title", f"Merge {branch}: {what}", "--body", body)
        number = int(url.rstrip("/").rsplit("/", 1)[1])
    print(f"{url}")
    queued = subprocess.run(["gh", "pr", "merge", str(number), "--repo", REPO, "--merge", "--auto"],
                            capture_output=True, text=True)
    # Run again on a pull request already in the queue, `gh pr merge` may refuse; the queue has it.
    try:
        in_queue = state(number)["isInMergeQueue"]
    except GhFailed:
        in_queue = False
    if queued.returncode and not in_queue:
        sys.exit(f"land.py: gh pr merge failed:\n{queued.stderr.strip()}")
    if not wait_for_it:
        print("queued; land.py does not wait (--no-wait)")
        return
    follow(number)


if __name__ == "__main__":
    main()
