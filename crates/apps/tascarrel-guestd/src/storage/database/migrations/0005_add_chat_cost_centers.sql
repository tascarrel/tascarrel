ALTER TABLE chats
ADD COLUMN cost_center_id TEXT
CHECK (
    cost_center_id IS NULL
    OR (
        length(cost_center_id) BETWEEN 1 AND 64
        AND cost_center_id NOT GLOB '*[^A-Za-z0-9_-]*'
    )
);

CREATE INDEX chats_by_cost_center
ON chats (cost_center_id, chat_id);
