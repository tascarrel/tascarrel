ALTER TABLE chats
ADD COLUMN purpose_jsonb BLOB
CHECK (purpose_jsonb IS NULL OR json_valid(purpose_jsonb, 8));
