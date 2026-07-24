CREATE TABLE pods (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) BETWEEN 1 AND 64),
    title TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 256),
    status TEXT NOT NULL CHECK (
        status IN ('creating', 'ready', 'destroying', 'destroyed')
    ),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    image_id TEXT NOT NULL CHECK (length(image_id) > 0),
    ephemeral INTEGER NOT NULL CHECK (ephemeral IN (0, 1)),
    identity_slot INTEGER NOT NULL CHECK (
        identity_slot >= 0 AND identity_slot <= 4294967295
    ),
    archived_at TEXT,
    CHECK (
        (status = 'destroyed' AND archived_at IS NOT NULL AND length(archived_at) > 0)
        OR (status <> 'destroyed' AND archived_at IS NULL)
    )
) STRICT;

CREATE INDEX pods_active_by_created_at ON pods (created_at, id) WHERE status <> 'destroyed';
CREATE INDEX pods_archived_by_archived_at ON pods (archived_at, id) WHERE status = 'destroyed';
CREATE UNIQUE INDEX pods_active_identity_slot ON pods (identity_slot) WHERE status <> 'destroyed';
