-- Sandbox and browser failures per day, with the most common error messages.
-- Athena's own child spans are named sandbox.exec and browser.action.
-- needs: spans.sql (scripts/analytics.sh prepends it)

SELECT
    CAST(started_at AS DATE) AS day,
    env,
    span_name,
    count(*) AS calls,
    count(*) FILTER (WHERE is_error) AS failures,
    round(count(*) FILTER (WHERE is_error) / count(*), 4) AS failure_rate,
    list(DISTINCT status_message) FILTER (WHERE is_error AND status_message IS NOT NULL)[1:5] AS sample_errors
FROM spans
WHERE span_name IN ('sandbox.exec', 'browser.action')
GROUP BY ALL
ORDER BY day DESC, failures DESC;
