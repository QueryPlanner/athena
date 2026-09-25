-- Tokens and cost per model per day, from Athena's own runs table.
-- Cost needs a row per model in analytics/prices.csv (USD per million
-- tokens); a model without one shows NULL cost rather than a guessed price.
ATTACH '{{ATHENA_DB}}' AS athena (TYPE sqlite, READ_ONLY);

WITH prices AS (
    SELECT * FROM read_csv('{{PRICES}}', header = true, columns = {
        'model': 'VARCHAR',
        'input_usd_per_mtok': 'DOUBLE',
        'output_usd_per_mtok': 'DOUBLE',
        'cached_input_usd_per_mtok': 'DOUBLE'
    })
)
SELECT
    CAST(to_timestamp(r.started_at / 1000) AS DATE) AS day,
    r.model,
    count(*) AS runs,
    count(*) FILTER (WHERE r.status <> 'ok') AS failed_runs,
    sum(r.model_calls) AS model_calls,
    sum(r.input_tokens) AS input_tokens,
    sum(r.output_tokens) AS output_tokens,
    sum(r.cached_input_tokens) AS cached_input_tokens,
    round(sum(
        (r.input_tokens - r.cached_input_tokens) * p.input_usd_per_mtok
        + r.cached_input_tokens * coalesce(p.cached_input_usd_per_mtok, p.input_usd_per_mtok)
        + r.output_tokens * p.output_usd_per_mtok
    ) / 1e6, 4) AS cost_usd
FROM athena.runs AS r
LEFT JOIN prices AS p USING (model)
GROUP BY ALL
ORDER BY day DESC, cost_usd DESC NULLS LAST, runs DESC;
