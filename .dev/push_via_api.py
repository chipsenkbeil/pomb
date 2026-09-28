#!/usr/bin/env python3
"""Push local commits to GitHub via the Git Data API (raw token never leaves vault).

Usage: push_via_api.py <repo_dir> <owner/repo> <local_base> <local_head> <remote_branch> <commit_message>
"""
import base64, json, os, subprocess, sys, urllib.request

sys.path.insert(0, "/opt/hatch/skills/skill-creator/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response, DynamicCredentialError

REPO_DIR, OWNER_REPO, BASE, HEAD, BRANCH, MESSAGE = sys.argv[1:7]
API = "https://api.github.com"
ALLOWED = ["api.github.com"]
UA = {"User-Agent": "dale-github-skill/1.0", "Accept": "application/vnd.github+json"}


def api(method, path, data=None):
    req = urllib.request.Request(API + path, method=method, headers=dict(UA))
    body = None
    if data is not None:
        body = json.dumps(data).encode()
        req.add_header("Content-Type", "application/json")
    add_surrogate_to_request(req, "custom.github", allowed_hosts=ALLOWED)
    try:
        with urllib.request.urlopen(req, data=body, timeout=120) as resp:
            return resp.status, read_json_response(resp)
    except urllib.error.HTTPError as e:
        raise DynamicCredentialError(f"GitHub API {e.code} on {method} {path}: {e.read().decode()[:300]}")


def git(*args):
    return subprocess.run(["git", *args], cwd=REPO_DIR, capture_output=True, text=True, check=True).stdout


# 1. Remote ref's tree must match local BASE's tree
_, refs = api("GET", f"/repos/{OWNER_REPO}/git/matching-refs/heads/{BRANCH}")
remote_sha = refs[0]["object"]["sha"]
_, commit_obj = api("GET", f"/repos/{OWNER_REPO}/git/commits/{remote_sha}")
base_tree = commit_obj["tree"]["sha"]
local_tree = git("rev-parse", BASE + "^{tree}").strip()
assert base_tree == local_tree, f"remote tree {base_tree} != local tree {local_tree}"

# 2. Diff file list (--name-status -z emits status\0path\0 pairs)
raw = git("diff", BASE, HEAD, "--name-status", "-z").split("\0")
fields = [f for f in raw if f]
entries = []
for i in range(0, len(fields), 2):
    status, path = fields[i][0], fields[i + 1]
    full = os.path.join(REPO_DIR, path)
    if status == "D":
        entries.append({"path": path, "mode": "100644", "type": "blob", "sha": None})
    else:
        with open(full, "rb") as f:
            content_b64 = base64.b64encode(f.read()).decode()
        _, blob = api("POST", f"/repos/{OWNER_REPO}/git/blobs",
                      {"content": content_b64, "encoding": "base64"})
        entries.append({"path": path, "mode": "100644", "type": "blob", "sha": blob["sha"]})
    print(f"  {status} {path}", flush=True)

# 3. New tree
_, tree_obj = api("POST", f"/repos/{OWNER_REPO}/git/trees",
                  {"base_tree": base_tree, "tree": entries})
print("tree:", tree_obj["sha"], flush=True)

# 4. New commit
_, new_commit = api("POST", f"/repos/{OWNER_REPO}/git/commits",
                    {"message": MESSAGE, "tree": tree_obj["sha"], "parents": [remote_sha]})
print("commit:", new_commit["sha"], flush=True)

# 5. Update ref
_, ref_obj = api("PATCH", f"/repos/{OWNER_REPO}/git/refs/heads/{BRANCH}",
                 {"sha": new_commit["sha"]})
print("ref now:", ref_obj["object"]["sha"], flush=True)
print("PUSHED OK")
