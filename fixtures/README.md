# Fixtures

- `grand-rapids.osm.json.gz`: `patinate fetch-osm` at the `config.toml` center and radius, gzipped, tracked in git LFS.
- `activities.json`: fully synthetic. Seeded random walks over the basemap's road graph, starts stratified across the map. Regenerate with
  `cargo run --release --example synth_activities > fixtures/activities.json`.
- `config.toml`: a public landmark as home and a public salt, accepted only together. Never copy the salt.
