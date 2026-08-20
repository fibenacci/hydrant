-- One table holds every record of every collection. The service knows nothing about the source
-- domain, so there is nothing to model per entity type: a record is an opaque JSON document
-- addressed by (source, collection, id).

-- `seq` is the load-bearing column: globally monotonic, so `?since=<seq>` is an index scan and a
-- consumer can replicate rather than poll.
--
-- It comes from an explicit sequence rather than GENERATED ALWAYS AS IDENTITY, because an identity
-- column is only assigned on INSERT. A change feed needs a new position on every *change*, and an
-- idempotent upsert writes changes as UPDATEs — with an identity column those would keep their old
-- position and consumers would never see them.
CREATE SEQUENCE record_seq AS bigint MINVALUE 1;

CREATE TABLE record (
    source       text        NOT NULL,
    collection   text        NOT NULL,
    id           text        NOT NULL,
    seq          bigint      NOT NULL DEFAULT nextval('record_seq'),
    payload      jsonb       NOT NULL,
    content_hash bytea       NOT NULL,
    ingested_at  timestamptz NOT NULL DEFAULT now(),

    -- Deletes are tombstones, not row removals: a consumer replicating from a cursor has to be
    -- able to observe the deletion. Compaction happens on a schedule longer than the maximum
    -- supported consumer lag.
    deleted_at   timestamptz,

    PRIMARY KEY (source, collection, id),

    -- SHA-256, always. A hash of another width means something wrote a different canonical form.
    CONSTRAINT record_content_hash_is_sha256
        CHECK (octet_length(content_hash) = 32),

    -- A tombstone carries no payload. Enforced here rather than only in application code: this is
    -- the invariant that keeps a deleted record from serving its last known state.
    CONSTRAINT record_tombstone_has_no_payload
        CHECK (deleted_at IS NULL OR payload = '{}'::jsonb)
);

ALTER SEQUENCE record_seq OWNED BY record.seq;

-- The feed reads by position; the unique index also guarantees no two records share one.
CREATE UNIQUE INDEX record_seq_key ON record (seq);
CREATE INDEX record_feed_idx ON record (source, collection, seq);
