"""Release admission tests using GitHub's paginated jobs response shape."""

import copy
import io
import json
import unittest
from unittest.mock import patch

import release_policy as policy


def evidence(staged=False):
    names = ["policy", "infra-checks", "unit", "integration", "build", "sandbox-image"]
    if staged:
        names += ["deploy-staging", "bench-staging", "smoke-staging"]
    return [{"total_count": len(names), "jobs": [
        {"name": name, "conclusion": "success"} for name in names
    ]}]


class ReleasePolicyTests(unittest.TestCase):
    def test_modes_default_and_reject_typos(self):
        for value, expected in [("", "staged"), ("staged", "staged"), ("direct", "direct")]:
            self.assertEqual(policy.deployment_mode(value), expected)
        for value in ["DIRECT", "false", " staged"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                policy.deployment_mode(value)

    def test_direct_accepts_ci_without_staging(self):
        policy.verify_jobs("direct", evidence())

    def test_staged_requires_real_staging_jobs(self):
        with self.assertRaisesRegex(ValueError, "deploy-staging"):
            policy.verify_jobs("staged", evidence())
        policy.verify_jobs("staged", evidence(True))

    def test_every_required_job_must_succeed(self):
        for index in range(9):
            for conclusion in ["failure", "skipped", "cancelled", None]:
                pages = evidence(True)
                pages[0]["jobs"][index]["conclusion"] = conclusion
                with self.subTest(index=index, conclusion=conclusion), self.assertRaises(ValueError):
                    policy.verify_jobs("staged", pages)

    def test_missing_jobs_are_rejected(self):
        for index in range(6):
            pages = evidence()
            del pages[0]["jobs"][index]
            pages[0]["total_count"] -= 1
            with self.subTest(index=index), self.assertRaises(ValueError):
                policy.verify_jobs("direct", pages)

    def test_advisory_eval_failure_does_not_block_release(self):
        pages = evidence(True)
        pages[0]["total_count"] += 1
        pages[0]["jobs"].append({"name": "eval-staging", "conclusion": "failure"})
        policy.verify_jobs("staged", pages)

    def test_pagination_and_truncated_evidence(self):
        page = evidence(True)[0]
        pages = [{"total_count": 9, "jobs": page["jobs"][:5]},
                 {"total_count": 9, "jobs": page["jobs"][5:]}]
        policy.verify_jobs("staged", pages)
        with self.assertRaises(ValueError):
            policy.verify_jobs("staged", pages[:1])
        pages[1]["total_count"] = 10
        with self.assertRaises(ValueError):
            policy.verify_jobs("staged", pages)

    def test_multiple_runs_cannot_be_combined(self):
        first = evidence()
        second = copy.deepcopy(first)
        first[0]["jobs"][0]["conclusion"] = "failure"
        second[0]["jobs"][1]["conclusion"] = "failure"
        for pages in [first, second, first + second]:
            with self.assertRaises(ValueError):
                policy.verify_jobs("direct", pages)

    def test_malformed_evidence_fails_closed(self):
        for pages in [None, {}, [], [{}], [{"jobs": []}],
                      [{"total_count": 1, "jobs": [None]}],
                      [{"total_count": 1, "jobs": [{"conclusion": "success"}]}]]:
            with self.subTest(pages=pages), self.assertRaises(ValueError):
                policy.verify_jobs("direct", pages)

    def test_duplicate_names_do_not_count_as_complete_evidence(self):
        pages = evidence()
        pages[0]["jobs"][1] = pages[0]["jobs"][0].copy()
        with self.assertRaisesRegex(ValueError, "duplicate job"):
            policy.verify_jobs("direct", pages)

    def test_command_reports_success_and_errors(self):
        with patch("sys.stdout", new_callable=io.StringIO) as output:
            self.assertEqual(policy.main(["mode", ""]), 0)
            self.assertEqual(output.getvalue(), "staged\n")
        with patch("sys.stdin", io.StringIO(json.dumps(evidence()))), patch("sys.stdout", new_callable=io.StringIO) as output:
            self.assertEqual(policy.main(["verify", "direct"]), 0)
            self.assertEqual(output.getvalue(), "direct\n")
        with patch("sys.stdin", io.StringIO("not json")), patch("sys.stderr", new_callable=io.StringIO):
            self.assertEqual(policy.main(["verify", "direct"]), 1)
            self.assertEqual(policy.main([]), 1)


if __name__ == "__main__":
    unittest.main()
