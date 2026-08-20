-- Ingest credentials. One token authenticates one source; a source may have several, so a
-- compromised sender can be revoked without stopping the others.
--
-- The plaintext token is never stored. What is stored is HMAC-SHA256 of the token, keyed by an
-- application secret: a database dump alone therefore does not yield working credentials, and the
-- lookup is a primary-key hit on a MAC rather than a comparison in application code.
CREATE TABLE ingest_token (
    token_hash bytea       PRIMARY KEY,
    source     text        NOT NULL,
    -- Who or what holds this token, for revocation: "sap-prod outbox", "laptop of the integrator".
    label      text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    -- Revocation keeps the row: knowing that a credential existed and when it stopped working is
    -- worth more than the space.
    revoked_at timestamptz,

    CONSTRAINT ingest_token_hash_is_sha256
        CHECK (octet_length(token_hash) = 32),
    CONSTRAINT ingest_token_label_is_not_empty
        CHECK (label <> '')
);

CREATE INDEX ingest_token_source_idx ON ingest_token (source) WHERE revoked_at IS NULL;
