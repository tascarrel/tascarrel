CREATE TABLE chats (
    chat_id TEXT PRIMARY KEY NOT NULL CHECK (length(chat_id) > 0),
    pod_id TEXT NOT NULL CHECK (length(pod_id) > 0),
    title TEXT NOT NULL CHECK (length(title) > 0),
    harness_jsonb BLOB NOT NULL CHECK (json_valid(harness_jsonb, 8)),
    model_jsonb BLOB CHECK (model_jsonb IS NULL OR json_valid(model_jsonb, 8)),
    resume_cursor_jsonb BLOB CHECK (
        resume_cursor_jsonb IS NULL OR json_valid(resume_cursor_jsonb, 8)
    ),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    updated_at TEXT NOT NULL CHECK (length(updated_at) > 0)
) STRICT;

CREATE INDEX chats_by_archival_and_updated_at
ON chats (archived, updated_at DESC, chat_id);

CREATE TABLE chat_turns (
    chat_id TEXT NOT NULL REFERENCES chats(chat_id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL CHECK (length(turn_id) > 0),
    turn_index INTEGER NOT NULL CHECK (turn_index >= 0),
    turn_jsonb BLOB NOT NULL CHECK (json_valid(turn_jsonb, 8)),
    PRIMARY KEY (chat_id, turn_id),
    UNIQUE (chat_id, turn_index)
) STRICT;

CREATE TABLE chat_timeline_entries (
    chat_id TEXT NOT NULL REFERENCES chats(chat_id) ON DELETE CASCADE,
    entry_id TEXT NOT NULL CHECK (length(entry_id) > 0),
    entry_index INTEGER NOT NULL CHECK (entry_index >= 0),
    entry_kind TEXT NOT NULL CHECK (entry_kind IN ('item', 'request', 'activity')),
    entry_jsonb BLOB NOT NULL CHECK (json_valid(entry_jsonb, 8)),
    PRIMARY KEY (chat_id, entry_id),
    UNIQUE (chat_id, entry_index)
) STRICT;

CREATE TABLE chat_attachments (
    chat_id TEXT NOT NULL REFERENCES chats(chat_id) ON DELETE CASCADE,
    attachment_id TEXT NOT NULL CHECK (length(attachment_id) > 0),
    attachment_jsonb BLOB NOT NULL CHECK (json_valid(attachment_jsonb, 8)),
    PRIMARY KEY (chat_id, attachment_id)
) STRICT;
