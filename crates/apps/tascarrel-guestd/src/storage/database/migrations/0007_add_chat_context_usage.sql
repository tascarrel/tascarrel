ALTER TABLE chats ADD COLUMN context_usage_jsonb BLOB
CHECK (context_usage_jsonb IS NULL OR json_valid(context_usage_jsonb, 8));
