-- A database as written by `main` (commit 27163af), before migrations existed.
-- user_version 0, messages table only. Rows are a real test.sh run against
-- OpenRouter: a tool call, its result, and two follow-up turns.
-- Never edit this file: it is what users' existing agent.db files look like.
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
