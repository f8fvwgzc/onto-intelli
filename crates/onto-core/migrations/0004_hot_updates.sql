-- Leave room on every page of entities, so an update that changes no
-- indexed column (a state change, a property that is not searchable) is a
-- HOT update: no new entries in any index, the HNSW index included. Pages
-- written before this keep their old fill until they are rewritten.

ALTER TABLE {schema}.entities SET (fillfactor = 85);
