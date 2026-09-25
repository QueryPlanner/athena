-- Eval pass rate per git sha and environment, and per case, from the
-- JSONL rows `athena eval run --out` writes (plan section 9):
-- eval_run_id, git_sha, env, case_id, sample, pass, scores, run_id,
-- trace_id, tokens, latency_ms.
CREATE OR REPLACE TEMP VIEW results AS
SELECT * FROM read_json('{{EVAL_RESULTS}}', format = 'newline_delimited', union_by_name = true);

SELECT
    git_sha,
    env,
    count(DISTINCT case_id) AS cases,
    count(*) AS samples,
    round(avg(CASE WHEN pass THEN 1 ELSE 0 END), 4) AS pass_rate,
    round(quantile_cont(latency_ms, 0.95), 0) AS p95_latency_ms
FROM results
GROUP BY ALL
ORDER BY git_sha, env;

-- pass^k per case: a case counts only if every sample passed.
SELECT
    env,
    case_id,
    count(*) AS samples,
    bool_and(pass) AS pass_all,
    round(avg(CASE WHEN pass THEN 1 ELSE 0 END), 4) AS pass_rate
FROM results
GROUP BY ALL
ORDER BY pass_all, pass_rate, case_id;
