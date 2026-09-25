-- Shared view: one row per span from the JSONL files Athena writes itself
-- (ATHENA_TELEMETRY_DIR, src/telemetry/jsonl.rs documents the schema).
-- The columns are declared rather than inferred, so every file reads the
-- same way; attributes and resource stay JSON because their keys vary.
CREATE OR REPLACE TEMP VIEW spans AS
SELECT
    json_extract_string(resource, '$."deployment.environment.name"') AS env,
    json_extract_string(resource, '$."service.version"') AS version,
    trace_id,
    span_id,
    parent_span_id,
    name AS span_name,
    to_timestamp(start_unix_nano / 1e9) AS started_at,
    duration_ms,
    status = 'error' AS is_error,
    status_message,
    json_extract_string(attributes, '$."gen_ai.tool.name"') AS tool_name,
    attributes
FROM read_json('{{TELEMETRY_DIR}}/traces-*.jsonl',
               format = 'newline_delimited',
               -- A staging span with content capture on can be large.
               maximum_object_size = 104857600,
               columns = {
                   'trace_id': 'VARCHAR',
                   'span_id': 'VARCHAR',
                   'parent_span_id': 'VARCHAR',
                   'name': 'VARCHAR',
                   'kind': 'VARCHAR',
                   'start_unix_nano': 'UBIGINT',
                   'end_unix_nano': 'UBIGINT',
                   'duration_ms': 'DOUBLE',
                   'status': 'VARCHAR',
                   'status_message': 'VARCHAR',
                   'attributes': 'JSON',
                   'events': 'JSON',
                   'scope': 'VARCHAR',
                   'resource': 'JSON'
               });
