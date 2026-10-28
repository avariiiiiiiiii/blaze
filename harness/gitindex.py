#!/usr/bin/env python3
"""Fetch a local git-index registry with Cargo and Blaze, including offline cache exchange.

There is no crates.io download: two .crate files and their index are made here. The fetched
sources, archive bytes, directory names and index cache bytes must be Cargo's. `--fetch`
uses blaze-registry's fetch example instead of the CLI (fetchdiff.py's driver).
"""
import functools, hashlib, http.server, io, json, os, re, shutil, subprocess, sys, tarfile, tempfile, threading
from pathlib import Path

from blaze_cli import base_env
from checktargets import write
from fetchdiff import compare


def main(fetch=None):
    if fetch is None and "--fetch" in sys.argv:
        fetch = sys.argv[sys.argv.index("--fetch") + 1]
    work = Path(tempfile.mkdtemp(prefix="git-index-", dir="/tmp" if os.name != "nt" else None))
    print(f"work: {work}", flush=True)
    repo, downloads, ws = (work / it for it in ("index", "downloads", "workspace"))
    repo.mkdir(); downloads.mkdir()
    home = {tool: work / (tool + "-home") for tool in ("cargo", "blaze")}
    base = {tool: dict(base_env(str(work / (tool + "-daemon"))), CARGO_HOME=str(at)) for tool, at in home.items()}
    for at in home.values():
        at.mkdir()
    git_env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@localhost",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@localhost")

    def git(*args):
        return subprocess.run(["git", "-c", "commit.gpgsign=false", *args], cwd=repo,
                              env=git_env, check=True, capture_output=True, text=True).stdout.strip()

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def do_GET(self):
            if self.headers.get("authorization") != "git-index-fixture-token":
                self.send_error(401)
                return
            super().do_GET()

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=str(downloads)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    write(repo, "config.json", json.dumps({"dl": f"http://127.0.0.1:{port}/{{crate}}-{{version}}.crate",
                                           "auth-required": True}))
    for name, deps in (("beta", []), ("alpha", [{"name": "beta", "req": "^1", "features": [],
                        "optional": False, "default_features": True, "target": None, "kind": "normal"}])):
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w:gz", format=tarfile.USTAR_FORMAT) as tar:
            manifest = f'[package]\nname = "{name}"\nversion = "1.0.0"\nedition = "2021"\n'
            if deps:
                manifest += '[dependencies]\nbeta = "1"\n'
            for path, text in (("Cargo.toml", manifest), ("src/lib.rs", "pub fn value() -> u32 { 7 }\n")):
                body = text.encode(); info = tarfile.TarInfo(f"{name}-1.0.0/{path}")
                info.size = len(body); info.mode = 0o644; info.mtime = 1000000000
                tar.addfile(info, io.BytesIO(body))
        data = archive.getvalue()
        (downloads / f"{name}-1.0.0.crate").write_bytes(data)
        entry = {"name": name, "vers": "1.0.0", "deps": deps, "cksum": hashlib.sha256(data).hexdigest(),
                 "features": {}, "yanked": False}
        write(repo, f"{name[:2]}/{name[2:4]}/{name}", json.dumps(entry) + "\n")
    git("init", "-q"); git("add", "."); git("commit", "-q", "-m", "index")
    url = repo.as_uri()
    config = f'[registries.fixture]\nindex = "{url}"\n[registry]\nglobal-credential-providers = ["cargo:token"]\n[net]\ngit-fetch-with-cli = true\n'
    for at in home.values():
        (at / "config.toml").write_text(config)
        (at / "credentials.toml").write_text('[registries.fixture]\ntoken = "git-index-fixture-token"\n')
    write(ws, "Cargo.toml", '[package]\nname = "uses-git-index"\nversion = "0.0.0"\nedition = "2021"\n'
          '[dependencies]\nalpha = { version = "1", registry = "fixture" }\n')
    write(ws, "src/lib.rs", "pub fn value() -> u32 { alpha::value() }\n")

    def run(tool, *args, use_home=None):
        at = use_home or tool
        command = [tool, *args]
        if tool == "blaze" and fetch:
            command = [fetch, str(ws / "Cargo.lock"), str(home[at]), "--config", str(home[at] / "config.toml")]
            if "--offline" in args:
                command += ["--offline"]
        lock = ws / "Cargo.lock"
        original = lock.read_text() if lock.exists() else None
        # The fetch example resolves no manifest. An old lock without checksums makes it
        # query each package's index too, rather than only downloading known archives.
        if tool == "blaze" and fetch:
            lock.write_text(re.sub(r'^checksum = .*\n', '', original, flags=re.M))
        try:
            ran = subprocess.run(command, cwd=ws, env=base[at], capture_output=True, text=True, timeout=600)
        finally:
            if tool == "blaze" and fetch:
                lock.write_text(original)
        assert ran.returncode == 0, f"{command}: exit {ran.returncode}\n{ran.stdout}\n{ran.stderr}"
        return ran

    passed = False
    try:
        run("cargo", "generate-lockfile")
        cargo_lock = (ws / "Cargo.lock").read_text()
        for tool in home:
            if tool == "blaze" and not fetch:
                # A locked fetch can use checksums without querying each index entry.
                # Resolve from nothing here to exercise the git index through metadata too.
                (ws / "Cargo.lock").unlink()
                run(tool, "metadata", "--format-version=1")
                assert (ws / "Cargo.lock").read_text() == cargo_lock, "Blaze's lock differs from Cargo's"
                run(tool, "fetch", "--locked")
            else:
                run(tool, "fetch", "--locked")
        total, unequal, dirs = compare(str(home["cargo"]), str(home["blaze"]))
        assert total == 2 and not unequal and all(a == b for a, b in dirs.values()), (total, unequal, dirs)
        paths = list((home["cargo"] / "registry/index").glob("*/.cache/*/*/*"))
        assert len(paths) == 2, paths
        for path in paths:
            relative = path.relative_to(home["cargo"])
            assert path.read_bytes() == (home["blaze"] / relative).read_bytes(), relative
        print("  2 packages: sources, archives, paths and index cache bytes equal Cargo's", flush=True)
        # A cached git tree, with no working-tree index files, must suffice for the other
        # implementation too. The remote and download service are no longer available.
        server.shutdown(); server.server_close()
        shutil.rmtree(repo)
        for tool in home:
            other = "blaze" if tool == "cargo" else "cargo"
            run(tool, "fetch", "--locked", "--offline", use_home=other)
            print(f"  {tool} reads {other}'s cache offline", flush=True)
        passed = True
    finally:
        server.server_close()
        if passed:
            shutil.rmtree(work)
    print("PASS: git-index registry and shared offline caches", flush=True)


if __name__ == "__main__":
    main()
