ALTER TABLE chats ADD COLUMN attention_required INTEGER NOT NULL DEFAULT 0
CHECK (attention_required IN (0, 1));
