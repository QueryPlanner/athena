"""Check complete GitHub job evidence for one candidate release run."""

import json
import sys


def deployment_mode(value):
    mode = value or "staged"
    if mode not in ("staged", "direct"):
        raise ValueError("ATHENA_DEPLOY_MODE must be staged or direct")
    return mode


def verify_jobs(mode, pages):
    mode = deployment_mode(mode)
    if not isinstance(pages, list) or not pages:
        raise ValueError("expected paginated job evidence for one run")
    jobs = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get("jobs"), list):
            raise ValueError("invalid jobs page")
        jobs.extend(page["jobs"])
    total = pages[0].get("total_count")
    if type(total) is not int or total < 1 or any(
        page.get("total_count") != total for page in pages
    ) or len(jobs) != total:
        raise ValueError("incomplete or inconsistent job evidence")
    conclusions = {}
    for job in jobs:
        if not isinstance(job, dict) or not isinstance(job.get("name"), str):
            raise ValueError("invalid job")
        name = job["name"]
        if name in conclusions:
            raise ValueError(f"duplicate job {name}")
        conclusions[name] = job.get("conclusion")
    required = ["policy", "infra-checks", "unit", "integration", "build", "sandbox-image"]
    if mode == "staged":
        required += ["deploy-staging", "bench-staging", "smoke-staging"]
    missing = [name for name in required if conclusions.get(name) != "success"]
    if missing:
        raise ValueError("required jobs did not succeed: " + ", ".join(missing))


def main(args):
    try:
        if len(args) != 2 or args[0] not in ("mode", "verify"):
            raise ValueError("usage: release_policy.py <mode|verify> MODE")
        mode = deployment_mode(args[1])
        if args[0] == "verify":
            verify_jobs(mode, json.load(sys.stdin))
        print(mode)
        return 0
    except (ValueError, TypeError) as error:
        print(f"release policy: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
