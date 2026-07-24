CREATE TABLE images (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) BETWEEN 1 AND 64),
    input_sha256 TEXT NOT NULL CHECK (
        length(input_sha256) = 64
        AND input_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    input_modified_at TEXT NOT NULL CHECK (length(input_modified_at) > 0),
    status TEXT NOT NULL CHECK (
        status IN ('generating', 'generated', 'orphaned', 'failed')
    ),
    runtime_generation_id TEXT CHECK (
        runtime_generation_id IS NULL OR length(runtime_generation_id) > 0
    ),
    failure_message TEXT CHECK (
        failure_message IS NULL OR length(failure_message) > 0
    ),
    failed_at TEXT CHECK (
        failed_at IS NULL OR length(failed_at) > 0
    ),
    created_at TEXT NOT NULL CHECK (length(created_at) > 0),
    CHECK (
        (
            status = 'generating'
            AND runtime_generation_id IS NULL
            AND failure_message IS NULL
            AND failed_at IS NULL
        )
        OR (
            status = 'generated'
            AND runtime_generation_id IS NOT NULL
            AND failure_message IS NULL
            AND failed_at IS NULL
        )
        OR (
            status = 'orphaned'
            AND runtime_generation_id IS NOT NULL
            AND failure_message IS NULL
            AND failed_at IS NULL
        )
        OR (
            status = 'failed'
            AND runtime_generation_id IS NULL
            AND failure_message IS NOT NULL
            AND failed_at IS NOT NULL
        )
    )
) STRICT;

CREATE UNIQUE INDEX images_single_generating
ON images (status) WHERE status = 'generating';

CREATE INDEX images_by_created_at ON images (created_at, id);
