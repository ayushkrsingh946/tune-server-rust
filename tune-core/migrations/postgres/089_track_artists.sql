-- Chantier « artistes multiples » (docs/chantier-artistes-multiples.md).
-- Jumelle de la migration SQLite 125.
--
-- La liaison piste <-> artistes, À CÔTÉ de `tracks.artist_id`, qui reste
-- l'artiste principal. Le scan la remplit quand il découpe un crédit ARTIST
-- (« The Beatles & Genesis ») ; une piste à un seul artiste n'y a aucune
-- ligne. La table naît vide ; le prochain scan la remplit.
BEGIN;
CREATE TABLE IF NOT EXISTS track_artists (
    track_id BIGINT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    artist_id BIGINT NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
    rang INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, artist_id)
);
CREATE INDEX IF NOT EXISTS idx_track_artists_artist ON track_artists(artist_id);
INSERT INTO schema_version (version, name) VALUES (89, 'track_artists')
    ON CONFLICT (version) DO NOTHING;

COMMIT;
