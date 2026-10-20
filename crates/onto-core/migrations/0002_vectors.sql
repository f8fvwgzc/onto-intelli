-- Entity vectors in pgvector: the hashed word + trigram embedding of each
-- entity's searchable text, 512 components in -127..127 (exact in
-- halfvec), searched by cosine distance through an HNSW index.
-- {vector} is the schema the vector extension lives in.

ALTER TABLE {schema}.entities
    ADD COLUMN IF NOT EXISTS embedding {vector}.halfvec(512),
    -- false until a vector was computed (bulk imports, rows from before this
    -- migration); the engine backfills them when it opens the tenant.
    ADD COLUMN IF NOT EXISTS embedded BOOLEAN NOT NULL DEFAULT false;

CREATE INDEX IF NOT EXISTS entities_embedding
    ON {schema}.entities USING hnsw (embedding {vector}.halfvec_cosine_ops);
CREATE INDEX IF NOT EXISTS entities_unembedded ON {schema}.entities (id) WHERE NOT embedded;
