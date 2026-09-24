-- A database as written by feat/run-observability at 7e6ddc5, before migrations
-- existed: main's messages table plus runs, still at user_version 0.
-- The runs DDL is copied verbatim from that commit. Never edit this file.
CREATE TABLE messages (
             session_id TEXT NOT NULL,
             seq        INTEGER NOT NULL,
             json       TEXT NOT NULL,
             PRIMARY KEY (session_id, seq)
         );
INSERT INTO messages VALUES('testsess',0,'{"role":"user","content":[{"type":"text","text":"Use the add tool to add 21 and 21. Reply with just the number."}]}');
INSERT INTO messages VALUES('testsess',1,'{"role":"assistant","id":null,"content":[{"type":"toolcall","id":"call_7njgCgdDJJkx6hvWbhJd0LVz","provider":{"call_id":"call_7njgCgdDJJkx6hvWbhJd0LVz"},"function":{"name":"add","arguments":{"a":21,"b":21}},"signature":null,"additional_params":null}]}');
INSERT INTO messages VALUES('testsess',2,'{"role":"user","content":[{"type":"toolresult","call":"call_7njgCgdDJJkx6hvWbhJd0LVz","provider":{"call_id":"call_7njgCgdDJJkx6hvWbhJd0LVz"},"name":"add","content":[{"type":"json","value":42.0}]}]}');
INSERT INTO messages VALUES('testsess',3,'{"role":"assistant","id":null,"content":[{"type":"text","text":"42"}]}');
INSERT INTO messages VALUES('testsess',4,'{"role":"user","content":[{"type":"text","text":"What two numbers did I just ask you to add? Answer from our conversation, do not use a tool."}]}');
INSERT INTO messages VALUES('testsess',5,'{"role":"assistant","id":null,"content":[{"type":"text","text":"21 and 21"}]}');
INSERT INTO messages VALUES('testsess',6,'{"role":"user","content":[{"type":"text","text":"and what was the result?"}]}');
INSERT INTO messages VALUES('testsess',7,'{"role":"assistant","id":null,"content":[{"type":"text","text":"42"}]}');
CREATE TABLE IF NOT EXISTS runs (
             run_id     TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             started_at INTEGER NOT NULL,
             ended_at   INTEGER NOT NULL,
             model      TEXT NOT NULL,
             status     TEXT NOT NULL,
             error      TEXT,
             first_seq  INTEGER NOT NULL,
             last_seq   INTEGER NOT NULL,
             input_tokens                INTEGER NOT NULL DEFAULT 0,
             output_tokens               INTEGER NOT NULL DEFAULT 0,
             total_tokens                INTEGER NOT NULL DEFAULT 0,
             cached_input_tokens         INTEGER NOT NULL DEFAULT 0,
             cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
             reasoning_tokens            INTEGER NOT NULL DEFAULT 0,
             tool_use_prompt_tokens      INTEGER NOT NULL DEFAULT 0,
             model_calls INTEGER NOT NULL DEFAULT 0,
             calls_json  TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS runs_by_session ON runs (session_id, started_at);
INSERT INTO runs VALUES('run-before-migrations','testsess',1000,2000,'openai/gpt-5.6-luna','ok',NULL,0,3,256,26,282,0,0,0,0,2,'[]');
