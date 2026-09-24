-- A database as written by main at 74095ac: schema version 2, the messages
-- and runs tables. Made by loading v0_main.sql and running that build's
-- binary against OpenRouter three times:
--   testsess  a tool call (add 2 and 3), its result, and a reply carrying an
--             encrypted reasoning block, appended after the 8 v0 rows
--   research  a second session, one plain exchange
--   broken    AGENT_MODEL=no-such/model-e2e: a failed run and no messages
-- The OpenRouter account id in the failed run's error text was replaced by
-- user_REDACTED; nothing else was changed. .dump omits user_version, so it
-- is set on the last line. Never edit this file.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
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
INSERT INTO messages VALUES('testsess',8,'{"role":"user","content":[{"type":"text","text":"Use the add tool to add 2 and 3. Reply with just the number."}]}');
INSERT INTO messages VALUES('testsess',9,'{"role":"assistant","id":null,"content":[{"type":"toolcall","id":"call_yKEprld4S9SbfUWKFyR1kWIB","provider":{"call_id":"call_yKEprld4S9SbfUWKFyR1kWIB"},"function":{"name":"add","arguments":{"a":2,"b":3}},"signature":null,"additional_params":null}]}');
INSERT INTO messages VALUES('testsess',10,'{"role":"user","content":[{"type":"toolresult","call":"call_yKEprld4S9SbfUWKFyR1kWIB","provider":{"call_id":"call_yKEprld4S9SbfUWKFyR1kWIB"},"name":"add","content":[{"type":"json","value":5.0}]}]}');
INSERT INTO messages VALUES('testsess',11,'{"role":"assistant","id":null,"content":[{"type":"reasoning","id":"rs_00b2166273180da2016ab54834e0d487d0b7caa9a4d7f10c76","content":[{"type":"encrypted","content":"gAAAAABqtUg1uQ3B4ROTNSkLRNDUG6-fXTEGWIsXxfLm3qVeojkHXs9QbFX92CpyBSC80HKhIlXVtq4PiHpSJRskmwouf-S1TUUYW5PSMW1NJSckTchlFY9feMYzLtS3s-qOlYNyoP6PoeTlkzu3zUpguZwyo6uw2vFcjBPJePfbPbK_JA7uJ4nVh9dJUVTElZNg9p2IZSvPTuLOedYSyP9OL6eJHRBePy3tKZ-OesOnz7d7e79q9SMPueMivhcdlIirDxxxq8VF5-5JW0UFmP3ZQyQSTNYxnqLZI1CMKqm3eOhlR2sQ_cGcxy3rIoh21cwn_vHWItYJKa-V3MX7ozq9Ld7TuVyVJVmwbK0SM93WTMF4u7KiV-6t70cdtQhWohAnwX1akq5mZxB3dW26SONCaDHnvLzQ4qsqceVozcMBcY3Fo0Eb6A56Yr0HErh4NHAfpKQyy5tjPA_FlvJTQx5cuaHMcWzo2VDWGjMkzra7jzIIN2mNgW5BlE5mmpFXI7_tC5USeOuZmGjO4U1YkHnGAfFNXL1FAZ_fH5L8ZjpkofBLuzhQt58_54aDTd32vsvmLr7zL-z5PAyFL1gBbyDwPW6--yftUjSuIbZmcTEy3XRyjxu9tPJdzj8AjZakzwx8hOIRmjpcxE8WV-67u9D2XP3rxzx57AznNEYm-ixfqfj3d5rU3WxxETKrzRYTW4rXxhICNc2NB21z27nzeJVtJ_wblxi1G5BlnpaTIhMqs7DO8IQzuFXCZbQe5tDrx6zxTWUvm-s5SHDor6xNmOx0BpMeg_aUM-ivnKiB-DQQ935JYP7nWgUCHQ21F9QmKXU6QrvGdN0m6Ss61SBDm-Hv51KHxhOrGvXvwAsm9r-xJ22PRHbX797wH6vrbFU2kOy05JtvRwS2nrqlYPa01J_-OxmQ3hcoyOiYxUZjziHhRz9MPMPn3PqbnUzCC8oUKI4ORLQcrgMzCAynOCTrkg-JOgkHhBhIjRJ7nm012J5P4GPQ-pTbb4YQRW-OTXQtCVBeZyKPtqHonNkfhbDpM1Jissw96PHCulhzzx94EgoSwaZEn1ZjB7TzmxZvdwUSLmmA7L1_oI8QTx-gsCvrrVWrXxtXqCVHem42PaQbrf3wDdhDs36xTkz9wOJzvg4hRbXzP-qo2qjQ6F36nEBOMbfdrXjZjtHBZiTRmnmdLt_6sC5jwOzz65MMyR2C2DUgIzlmMZgOKjyCW2nkQaTqzTll3-M55GXpNnhJSNcF1A4mBmrX82UI12g1hKqX-hySUzRbnhk8fT9a-JqHT5nloGO7N7BmyMURu3xe_-eMZEOwSUCiU1p-j3At5GGIqNibB0FfZX9Gqd41Wg-kRFU2H8MUfFIRWT0HSPtFyrxxh-B0SfI72usieafFnqXmx0QZ20r3Tc-JQbAQ.eyJlbmRwb2ludF9zbHVnIjoib3BlbmFpL2dwdC01LjYtbHVuYS0yMDI2MDcwOXxvcGVuYWkifQ"}]},{"type":"text","text":"5"}]}');
INSERT INTO messages VALUES('research',0,'{"role":"user","content":[{"type":"text","text":"Say hi in one word."}]}');
INSERT INTO messages VALUES('research',1,'{"role":"assistant","id":null,"content":[{"type":"text","text":"Hi"}]}');
CREATE TABLE runs (
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
INSERT INTO runs VALUES('5b93ad79-4b74-4f49-b945-837147e8ad6d','testsess',1790265390532,1790265397695,'openai/gpt-5.6-luna','ok',NULL,8,11,488,46,534,0,0,18,0,2,'[{"call_index":0,"finish_reason":"tool_calls","raw":{"choices":[{"finish_reason":"tool_calls","index":0,"message":{"role":"assistant","tool_calls":[{"function":{"arguments":"{\"a\":2,\"b\":3}","name":"add"},"id":"call_yKEprld4S9SbfUWKFyR1kWIB","type":"function"}]},"native_finish_reason":"completed"}],"created":1790265391,"id":"gen-1790265391-ZVhUC1KCyLNpE8fKfI0G","model":"openai/gpt-5.6-luna","object":"chat.completion","provider":"OpenAI","service_tier":"default","system_fingerprint":null,"usage":{"completion_tokens":21,"completion_tokens_details":{"reasoning_tokens":0},"cost":0.0,"prompt_tokens":227,"prompt_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"total_tokens":248}},"response_id":"gen-1790265391-ZVhUC1KCyLNpE8fKfI0G","usage":{"cache_creation_input_tokens":0,"cached_input_tokens":0,"input_tokens":227,"output_tokens":21,"reasoning_tokens":0,"tool_use_prompt_tokens":0,"total_tokens":248}},{"call_index":1,"finish_reason":"stop","raw":{"choices":[{"finish_reason":"stop","index":0,"message":{"content":[{"text":"5","type":"text"}],"reasoning_details":[{"data":"gAAAAABqtUg1uQ3B4ROTNSkLRNDUG6-fXTEGWIsXxfLm3qVeojkHXs9QbFX92CpyBSC80HKhIlXVtq4PiHpSJRskmwouf-S1TUUYW5PSMW1NJSckTchlFY9feMYzLtS3s-qOlYNyoP6PoeTlkzu3zUpguZwyo6uw2vFcjBPJePfbPbK_JA7uJ4nVh9dJUVTElZNg9p2IZSvPTuLOedYSyP9OL6eJHRBePy3tKZ-OesOnz7d7e79q9SMPueMivhcdlIirDxxxq8VF5-5JW0UFmP3ZQyQSTNYxnqLZI1CMKqm3eOhlR2sQ_cGcxy3rIoh21cwn_vHWItYJKa-V3MX7ozq9Ld7TuVyVJVmwbK0SM93WTMF4u7KiV-6t70cdtQhWohAnwX1akq5mZxB3dW26SONCaDHnvLzQ4qsqceVozcMBcY3Fo0Eb6A56Yr0HErh4NHAfpKQyy5tjPA_FlvJTQx5cuaHMcWzo2VDWGjMkzra7jzIIN2mNgW5BlE5mmpFXI7_tC5USeOuZmGjO4U1YkHnGAfFNXL1FAZ_fH5L8ZjpkofBLuzhQt58_54aDTd32vsvmLr7zL-z5PAyFL1gBbyDwPW6--yftUjSuIbZmcTEy3XRyjxu9tPJdzj8AjZakzwx8hOIRmjpcxE8WV-67u9D2XP3rxzx57AznNEYm-ixfqfj3d5rU3WxxETKrzRYTW4rXxhICNc2NB21z27nzeJVtJ_wblxi1G5BlnpaTIhMqs7DO8IQzuFXCZbQe5tDrx6zxTWUvm-s5SHDor6xNmOx0BpMeg_aUM-ivnKiB-DQQ935JYP7nWgUCHQ21F9QmKXU6QrvGdN0m6Ss61SBDm-Hv51KHxhOrGvXvwAsm9r-xJ22PRHbX797wH6vrbFU2kOy05JtvRwS2nrqlYPa01J_-OxmQ3hcoyOiYxUZjziHhRz9MPMPn3PqbnUzCC8oUKI4ORLQcrgMzCAynOCTrkg-JOgkHhBhIjRJ7nm012J5P4GPQ-pTbb4YQRW-OTXQtCVBeZyKPtqHonNkfhbDpM1Jissw96PHCulhzzx94EgoSwaZEn1ZjB7TzmxZvdwUSLmmA7L1_oI8QTx-gsCvrrVWrXxtXqCVHem42PaQbrf3wDdhDs36xTkz9wOJzvg4hRbXzP-qo2qjQ6F36nEBOMbfdrXjZjtHBZiTRmnmdLt_6sC5jwOzz65MMyR2C2DUgIzlmMZgOKjyCW2nkQaTqzTll3-M55GXpNnhJSNcF1A4mBmrX82UI12g1hKqX-hySUzRbnhk8fT9a-JqHT5nloGO7N7BmyMURu3xe_-eMZEOwSUCiU1p-j3At5GGIqNibB0FfZX9Gqd41Wg-kRFU2H8MUfFIRWT0HSPtFyrxxh-B0SfI72usieafFnqXmx0QZ20r3Tc-JQbAQ.eyJlbmRwb2ludF9zbHVnIjoib3BlbmFpL2dwdC01LjYtbHVuYS0yMDI2MDcwOXxvcGVuYWkifQ","format":"openai-responses-v1","id":"rs_00b2166273180da2016ab54834e0d487d0b7caa9a4d7f10c76","index":0,"type":"reasoning.encrypted"}],"role":"assistant"},"native_finish_reason":"completed"}],"created":1790265394,"id":"gen-1790265394-2VQfinO74ZzzAlz8BPCl","model":"openai/gpt-5.6-luna","object":"chat.completion","provider":"OpenAI","service_tier":"default","system_fingerprint":null,"usage":{"completion_tokens":25,"completion_tokens_details":{"reasoning_tokens":18},"cost":0.0,"prompt_tokens":261,"prompt_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"total_tokens":286}},"response_id":"gen-1790265394-2VQfinO74ZzzAlz8BPCl","usage":{"cache_creation_input_tokens":0,"cached_input_tokens":0,"input_tokens":261,"output_tokens":25,"reasoning_tokens":18,"tool_use_prompt_tokens":0,"total_tokens":286}}]');
INSERT INTO runs VALUES('c0698f7b-9917-47a6-959c-f4e6ccf3b209','research',1790265397719,1790265399789,'openai/gpt-5.6-luna','ok',NULL,0,1,99,5,104,0,0,0,0,1,'[{"call_index":0,"finish_reason":"stop","raw":{"choices":[{"finish_reason":"stop","index":0,"message":{"content":[{"text":"Hi","type":"text"}],"role":"assistant"},"native_finish_reason":"completed"}],"created":1790265397,"id":"gen-1790265397-V0bRT5TXJ1RdbNd69c8Z","model":"openai/gpt-5.6-luna","object":"chat.completion","provider":"OpenAI","service_tier":"default","system_fingerprint":null,"usage":{"completion_tokens":5,"completion_tokens_details":{"reasoning_tokens":0},"cost":0.0,"prompt_tokens":99,"prompt_tokens_details":{"cache_write_tokens":0,"cached_tokens":0},"total_tokens":104}},"response_id":"gen-1790265397-V0bRT5TXJ1RdbNd69c8Z","usage":{"cache_creation_input_tokens":0,"cached_input_tokens":0,"input_tokens":99,"output_tokens":5,"reasoning_tokens":0,"tool_use_prompt_tokens":0,"total_tokens":104}}]');
INSERT INTO runs VALUES('f8be8c10-75d9-404a-8040-2424278f3d92','broken',1790265399813,1790265400029,'no-such/model-e2e','error','CompletionError: HttpError: Invalid status code 400 Bad Request with message: {"error":{"message":"no-such/model-e2e is not a valid model ID","code":400},"user_id":"user_REDACTED"}',0,-1,0,0,0,0,0,0,0,0,'[]');
CREATE INDEX runs_by_session ON runs (session_id, started_at);
COMMIT;
PRAGMA user_version=2;
