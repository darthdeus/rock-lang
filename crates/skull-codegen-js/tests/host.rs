//! The host seam: every generated-JS touch of the outside world (stdout,
//! stderr, environment, RNG seed) goes through the embedder's `__rock_host`.
//! A browser Worker has no `process`, so each check here runs the runtime in a
//! node `vm` context that has none, and the node default is pinned separately.
