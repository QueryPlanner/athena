-- Schema 11 fixture: v10_user_settings.sql opened by main at a01f30e
-- (schema 11), which applied shipped migration 11 (workout_sessions,
-- workout_sets, rowing_results). That build's Store::workout_log then logged
-- two workouts for telegram:111111 (user 2): a push day on 2026-10-08 with a
-- warm-up and two exercises, and a VO2 day on 2026-10-09 with a 2000 m row.
-- Workout text and request keys are made up; every other row is
-- v10_user_settings.sql unchanged. Dumped with `sqlite3 .dump`, which omits
-- user_version, so it is set on the last line. Never edit this file.
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
CREATE TABLE users (
         id          INTEGER PRIMARY KEY,
         transport   TEXT NOT NULL,
         external_id TEXT NOT NULL,
         created_at  INTEGER NOT NULL,
         UNIQUE (transport, external_id)
     );
INSERT INTO users VALUES(1,'cli','local',1790297476232);
INSERT INTO users VALUES(2,'telegram','111111',1790297509324);
INSERT INTO users VALUES(3,'telegram','222222',1790297514521);
CREATE TABLE sessions (
         id         TEXT NOT NULL PRIMARY KEY,
         user_id    INTEGER NOT NULL REFERENCES users (id),
         name       TEXT NOT NULL,
         created_at INTEGER NOT NULL,
         UNIQUE (user_id, name)
     );
INSERT INTO sessions VALUES('broken',1,'broken',1790297476232);
INSERT INTO sessions VALUES('research',1,'research',1790297476232);
INSERT INTO sessions VALUES('testsess',1,'testsess',1790297476232);
INSERT INTO sessions VALUES('f53d1b64-0693-4d8a-8acd-b4e85a775f3e',2,'default',1790297509325);
INSERT INTO sessions VALUES('8e81cfc9-e0af-4b10-bb29-0daa0897de9d',2,'notes',1790297512036);
INSERT INTO sessions VALUES('a91c353d-694e-47ff-9322-4c44c7757714',3,'default',1790297514522);
CREATE TABLE IF NOT EXISTS "runs" (
         run_id     TEXT NOT NULL PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES sessions (id),
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
CREATE TABLE selected_sessions (
         user_id     INTEGER NOT NULL PRIMARY KEY REFERENCES users (id),
         session_id  TEXT NOT NULL REFERENCES sessions (id),
         selected_at INTEGER NOT NULL
     );
INSERT INTO selected_sessions VALUES(2,'8e81cfc9-e0af-4b10-bb29-0daa0897de9d',1790000000000);
CREATE TABLE sandboxes (
         session_id    TEXT NOT NULL PRIMARY KEY REFERENCES sessions (id),
         sandbox_id    TEXT NOT NULL,
         bash_session  TEXT,
         code_language TEXT,
         code_context  TEXT,
         created_at    INTEGER NOT NULL,
         expires_at    INTEGER NOT NULL
     );
INSERT INTO sandboxes VALUES('8e81cfc9-e0af-4b10-bb29-0daa0897de9d','sbx-fixture-1','bash-fixture-1','python','ctx-fixture-1',1790000000000,1790001800000);
CREATE TABLE browser_links (
         token      TEXT NOT NULL PRIMARY KEY,
         session_id TEXT NOT NULL REFERENCES sessions (id),
         url        TEXT NOT NULL,
         expires_at INTEGER NOT NULL
     );
INSERT INTO browser_links VALUES('fixturetoken0000000000000000000000000001','8e81cfc9-e0af-4b10-bb29-0daa0897de9d','https://example.com/login',1790003600000);
CREATE TABLE browser_states (
         user_id  INTEGER NOT NULL PRIMARY KEY REFERENCES users (id),
         state    BLOB NOT NULL,
         saved_at INTEGER NOT NULL
     );
INSERT INTO browser_states VALUES(2,x'7b22636f6f6b696573223a5b7b226e616d65223a2273657373696f6e222c2276616c7565223a22666978747572652d636f6f6b69652d76616c7565222c22646f6d61696e223a226578616d706c652e636f6d222c2270617468223a222f227d5d2c226f726967696e73223a5b5d7d',1790000600000);
CREATE TABLE compactions (
         session_id    TEXT NOT NULL REFERENCES sessions (id),
         through_seq   INTEGER NOT NULL,
         summary       TEXT NOT NULL,
         model         TEXT NOT NULL,
         input_tokens  INTEGER NOT NULL DEFAULT 0,
         output_tokens INTEGER NOT NULL DEFAULT 0,
         created_at    INTEGER NOT NULL,
         PRIMARY KEY (session_id, through_seq)
     );
INSERT INTO compactions VALUES('testsess',3,'fixture summary','fixture-model',10,2,1790000600000);
CREATE TABLE user_identities (
         transport   TEXT NOT NULL,
         external_id TEXT NOT NULL,
         user_id     INTEGER NOT NULL REFERENCES users (id),
         created_at  INTEGER NOT NULL,
         PRIMARY KEY (transport, external_id)
     );
INSERT INTO user_identities VALUES('cli','local',1,1790297476232);
INSERT INTO user_identities VALUES('telegram','111111',2,1790297509324);
INSERT INTO user_identities VALUES('telegram','222222',3,1790297514521);
INSERT INTO user_identities VALUES('http','linked-fixture',2,123456);
CREATE TABLE calorie_logs (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         user_id INTEGER NOT NULL REFERENCES users (id),
         request_key TEXT NOT NULL,
         request_hash TEXT NOT NULL,
         description TEXT NOT NULL,
         consumed_date TEXT NOT NULL,
         calories REAL CHECK (calories BETWEEN 0 AND 1000000),
         protein_g REAL CHECK (protein_g BETWEEN 0 AND 1000000),
         carbs_g REAL CHECK (carbs_g BETWEEN 0 AND 1000000),
         fat_g REAL CHECK (fat_g BETWEEN 0 AND 1000000),
         meal_type TEXT,
         source TEXT NOT NULL CHECK (source IN ('user', 'estimated')),
         version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL,
         deleted_at INTEGER,
         UNIQUE (user_id, request_key)
     );
INSERT INTO calorie_logs VALUES(1,2,'fixture-key-1','c00d85077dd17a7ec5a97d93703141bd87dacc4280143b9eefa7932723ee0bee','rice and eggs','2026-10-05',450.0,20.0,60.0,NULL,'lunch','estimated',2,1791563353568,1791563353569,NULL);
INSERT INTO calorie_logs VALUES(2,2,'fixture-key-2','7bbac0da9d3ad5595c294eb0c67c930a5fcad934ddfe1f7720089d399bca9e90','banana','2026-10-06',NULL,20.0,NULL,NULL,'lunch','estimated',2,1791563353568,1791563353569,1791563353569);
INSERT INTO calorie_logs VALUES(3,1,'fixture-key-1','40bb12443d10382ec854e776f83bf7f224a18f3d1c90ecd7d6854c23d92ae807','toast','2026-10-06',150.0,20.0,NULL,NULL,'lunch','estimated',1,1791563353568,1791563353568,NULL);
CREATE TABLE user_settings (
         user_id    INTEGER NOT NULL PRIMARY KEY REFERENCES users (id),
         timezone   TEXT NOT NULL,
         updated_at INTEGER NOT NULL
     );
INSERT INTO user_settings VALUES(2,'Europe/London',1791617420428);
CREATE TABLE workout_sessions (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         user_id INTEGER NOT NULL REFERENCES users (id),
         request_key TEXT NOT NULL,
         request_hash TEXT NOT NULL,
         session_date TEXT NOT NULL,
         day_type TEXT NOT NULL,
         notes TEXT,
         version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL,
         deleted_at INTEGER,
         UNIQUE (user_id, request_key)
     );
INSERT INTO workout_sessions VALUES(1,2,'fixture-push-1','c57278c2639d8d8185121ebae59d488935d3c444dd27e9f22524abc7232cc36d','2026-10-08','push','felt strong',1,1791623804422,1791623804422,NULL);
INSERT INTO workout_sessions VALUES(2,2,'fixture-vo2-1','6eb417510f9a6961a125b90962bdf59bad2239aa5f35d5fc2197b1319a2d595b','2026-10-09','vo2',NULL,1,1791623804423,1791623804423,NULL);
CREATE TABLE workout_sets (
         session_id INTEGER NOT NULL REFERENCES workout_sessions (id),
         exercise_index INTEGER NOT NULL CHECK (exercise_index > 0),
         set_index INTEGER NOT NULL CHECK (set_index > 0),
         exercise TEXT NOT NULL,
         exercise_key TEXT NOT NULL,
         reps INTEGER NOT NULL CHECK (reps BETWEEN 0 AND 1000),
         weight_kg REAL NOT NULL CHECK (weight_kg BETWEEN 0 AND 1000),
         target_reps INTEGER CHECK (target_reps BETWEEN 1 AND 1000),
         is_warmup INTEGER NOT NULL CHECK (is_warmup IN (0, 1)),
         notes TEXT,
         PRIMARY KEY (session_id, exercise_index, set_index)
     );
INSERT INTO workout_sets VALUES(1,1,1,'Bench Press','bench press',10,40.0,NULL,1,NULL);
INSERT INTO workout_sets VALUES(1,1,2,'Bench Press','bench press',8,80.0,8,0,NULL);
INSERT INTO workout_sets VALUES(1,1,3,'Bench Press','bench press',7,80.0,8,0,NULL);
INSERT INTO workout_sets VALUES(1,2,1,'Overhead Press','overhead press',8,45.5,NULL,0,NULL);
CREATE TABLE rowing_results (
         session_id INTEGER NOT NULL PRIMARY KEY REFERENCES workout_sessions (id),
         distance_m INTEGER NOT NULL DEFAULT 2000 CHECK (distance_m BETWEEN 100 AND 100000),
         time_ms INTEGER NOT NULL CHECK (time_ms > 0)
     );
INSERT INTO rowing_results VALUES(2,2000,465300);
PRAGMA writable_schema=ON;
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('calorie_logs',3);
INSERT INTO sqlite_sequence VALUES('workout_sessions',2);
CREATE TRIGGER messages_need_a_session BEFORE INSERT ON messages
     WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id = NEW.session_id)
     BEGIN SELECT RAISE(ABORT, 'no such session'); END;
CREATE TRIGGER messages_keep_a_session BEFORE UPDATE OF session_id ON messages
     WHEN NOT EXISTS (SELECT 1 FROM sessions WHERE id = NEW.session_id)
     BEGIN SELECT RAISE(ABORT, 'no such session'); END;
CREATE INDEX runs_by_session ON runs (session_id, started_at);
CREATE INDEX calories_by_owner_date ON calorie_logs (user_id, consumed_date, id);
CREATE INDEX workouts_by_owner_type_date
         ON workout_sessions (user_id, day_type, session_date, id);
CREATE INDEX workout_sets_by_exercise ON workout_sets (exercise_key, session_id);
PRAGMA writable_schema=OFF;
COMMIT;
PRAGMA user_version=11;
