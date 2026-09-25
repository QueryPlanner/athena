-- Shared view: one row per span from the collector's JSONL files
-- (deploy/otel/config.yaml, file/traces exporter, OTLP/JSON encoding).
-- Plain read_json rather than the otlp community extension, so it works
-- offline and does not depend on that extension's column names.
CREATE OR REPLACE TEMP VIEW spans AS
WITH files AS (
    SELECT unnest(resourceSpans) AS rs
    FROM read_json('{{OTEL_DIR}}/traces*.jsonl', format = 'newline_delimited',
                   maximum_object_size = 104857600, union_by_name = true)
),
scoped AS (
    SELECT rs.resource.attributes AS resource_attrs, unnest(rs.scopeSpans) AS ss FROM files
),
flat AS (
    SELECT resource_attrs, unnest(ss.spans) AS s FROM scoped
)
SELECT
    list_extract(list_filter(resource_attrs, a -> a.key = 'deployment.environment.name'), 1).value.stringValue AS env,
    s.traceId AS trace_id,
    s.spanId AS span_id,
    s.name AS span_name,
    to_timestamp(CAST(s.startTimeUnixNano AS HUGEINT) / 1e9) AS started_at,
    (CAST(s.endTimeUnixNano AS HUGEINT) - CAST(s.startTimeUnixNano AS HUGEINT)) / 1e6 AS duration_ms,
    coalesce(s.status.code, 0) = 2 AS is_error,
    s.status.message AS status_message,
    list_extract(list_filter(s.attributes, a -> a.key = 'gen_ai.tool.name'), 1).value.stringValue AS tool_name
FROM flat;
