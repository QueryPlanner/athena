-- p50/p95 latency and error rate per tool, per environment, over the retained JSONL (30 days).
-- rig names tool spans execute_tool and sets gen_ai.tool.name.
-- needs: spans.sql (scripts/analytics.sh prepends it)

SELECT
    env,
    coalesce(tool_name, span_name) AS tool,
    count(*) AS calls,
    round(quantile_cont(duration_ms, 0.5), 1) AS p50_ms,
    round(quantile_cont(duration_ms, 0.95), 1) AS p95_ms,
    round(avg(CASE WHEN is_error THEN 1 ELSE 0 END), 4) AS error_rate
FROM spans
WHERE span_name LIKE 'execute_tool%'
GROUP BY ALL
ORDER BY env, calls DESC;
