//! A persistent per-body codegen cache.
//!
//! A fresh compile of an unchanged project repeats the whole per-body
//! pipeline (lower, inline, cleanup, regalloc, stringify) for every function.
//! This cache stores each emitted [`Chunk`] on disk, keyed by what its text
//! depends on, so a later process reads the chunk instead of rebuilding it.
//!
//! The key is in two parts. The ENVIRONMENT hash covers everything a body's
//! emission can read other than function bodies: the compiler identity, the
//! emit options, every module's source text with its function body blocks cut
//! out (declarations, types, globals, decorators and macro definitions stay),
//! the build inputs, the by-reference fn set and the codegen context's slot
//! and constant tables. Any change there misses every body, which is also
//! how the in-process incremental cache treats an interface edit. The BODY
//! hash covers one function: its item text, its position, and every span of
//! its checked body (so a macro expansion retagged into it is covered too).
//!
//! A record is found by the environment, the declaration and its own body
//! hash, then validated against the facts recorded when it was built: the
//! body hash of every declaration whose body the chunk embeds, calls or
//! depends on for ABI (the chunk's own dependency set, the same set the
//! in-process cache invalidates on), the call-graph component relation of
//! every callee (what the callee binding read), and the build inputs the body
//! baked in. A fact that cannot be computed -- a module whose text the host
//! did not provide, a synthetic function -- disables caching for that body
//! rather than guessing.

use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use xxhash_rust::xxh3::Xxh3;

mod codec;
mod store;

/// Where and under what identity the cache lives, with the text of every
/// module the compile checked.
pub struct BodyCacheConfig {
    /// The cache directory; shards go under `<dir>/<project key>/`.
    pub dir: PathBuf,
    /// The running compiler image's content identity. A rebuilt compiler
    /// never reads chunks another one wrote.
    pub compiler_identity: String,
    /// Groups one project's shards for pruning.
    pub project_key: String,
    /// Source text by module file id. The stdlib's own text is added when
    /// the host did not provide it.
    pub sources: FxHashMap<Ustr, Arc<str>>,
}

impl BodyCacheConfig {
    /// `ROCK_BODY_CACHE`: unset or `1` keeps the default directory, `0`
    /// (or empty) disables the cache, anything else is the directory.
    pub fn from_environment(
        compiler_identity: String,
        project_key: &str,
        sources: FxHashMap<Ustr, Arc<str>>,
    ) -> Option<Self> {
        let dir = Self::dir_for(std::env::var_os("ROCK_BODY_CACHE").as_deref())?;
        Some(Self {
            dir,
            compiler_identity,
            project_key: project_key.to_string(),
            sources,
        })
    }

    /// The directory the `ROCK_BODY_CACHE` value selects, `None` when it
    /// disables the cache.
    pub fn dir_for(switch: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
        match switch {
            None => Self::default_dir(),
            Some(value) if value.is_empty() || value == "0" || value == "off" => None,
            Some(value) if value == "1" => Self::default_dir(),
            Some(value) => Some(PathBuf::from(value)),
        }
    }

    /// The per-user cache directory: `~/Library/Caches` on macOS, else
    /// `$XDG_CACHE_HOME` or `~/.cache`, under `rock-lang/body-cache`.
    pub fn default_dir() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        let base = if cfg!(target_os = "macos") {
            PathBuf::from(&home).join("Library/Caches")
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&home).join(".cache"))
        };
        Some(base.join("rock-lang").join("body-cache"))
    }
}

/// What one emission did with the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BodyCacheStats {
    /// False when the configuration or the compile made the cache inert
    /// (a diagnostic emit mode, or a module without source text).
    pub enabled: bool,
    pub hits: usize,
    pub misses: usize,
    /// Chunks whose key or dependency facts could not be computed.
    pub uncacheable: usize,
    pub stored: usize,
    /// Records and bytes of the shard that was loaded.
    pub shard_records: usize,
    pub shard_bytes: usize,
    pub written_bytes: usize,
}

#[derive(Default)]
struct Counters {
    hits: AtomicUsize,
    misses: AtomicUsize,
    uncacheable: AtomicUsize,
}

/// The per-emission state: the environment, every body's hash and the shard.
struct Prepared {
    env: u128,
    /// By declaration: `Some(hash)` for a function with a Rock body,
    /// `Some(DECLARATION_ONLY)` for one whose whole text is in the
    /// environment (extern, `inline(lua)`), absent for a synthetic one.
    bodies: FxHashMap<FuncRef, u128>,
    shard: store::Shard,
    shard_path: PathBuf,
}

/// A function with no Rock body of its own has nothing outside the
/// environment hash; any fixed value that a real hash never equals works.
const DECLARATION_ONLY: u128 = 1;

/// The cache handle one emission uses. Lookups and records run from the
/// parallel fan-out; [`Self::flush`] writes what was recorded.
pub struct BodyCache {
    config: BodyCacheConfig,
    prepared: OnceLock<Option<Prepared>>,
    pending: Mutex<Vec<(u128, Vec<u8>)>>,
    counters: Counters,
    written: AtomicUsize,
}

impl BodyCache {
    pub fn open(mut config: BodyCacheConfig) -> Self {
        for (file, text) in skull_parser::stdlib::source_texts() {
            config
                .sources
                .entry(file)
                .or_insert_with(|| Arc::from(text));
        }
        Self {
            config,
            prepared: OnceLock::new(),
            pending: Mutex::new(Vec::new()),
            counters: Counters::default(),
            written: AtomicUsize::new(0),
        }
    }

    pub fn stats(&self) -> BodyCacheStats {
        let prepared = self.prepared.get().and_then(Option::as_ref);
        BodyCacheStats {
            enabled: prepared.is_some(),
            hits: self.counters.hits.load(Ordering::Relaxed),
            misses: self.counters.misses.load(Ordering::Relaxed),
            uncacheable: self.counters.uncacheable.load(Ordering::Relaxed),
            stored: self.pending.lock().expect("body cache records").len(),
            shard_records: prepared.map_or(0, |p| p.shard.len()),
            shard_bytes: prepared.map_or(0, |p| p.shard.file_bytes()),
            written_bytes: self.written.load(Ordering::Relaxed),
        }
    }

    /// Whether this emission can use the cache ([`Self::prepare`] ran and
    /// every input was hashable).
    pub(super) fn is_active(&self) -> bool {
        self.prepared.get().is_some_and(Option::is_some)
    }

    /// Hash the environment and every body, then load this environment's
    /// shard. Runs once per emission, after the setup lanes built the codegen
    /// and scalar contexts it reads.
    pub(super) fn prepare(
        &self,
        ws: &Workspace,
        cg: &CodegenCtx,
        scx: &skull_mir::lower::ScalarCtx,
        build_tag: &str,
    ) {
        self.prepared.get_or_init(|| {
            if crate::allocation_report::enabled() || cg.trace_frames_enabled() {
                return None;
            }
            let modules: Vec<&bone_semantic::hir::Module> = ws.source_modules().collect();
            let hashed = par_map(modules, |module| {
                let text = self.config.sources.get(&module.file).map(Arc::clone);
                hash_module(module, text.as_deref())
            });
            if hashed.iter().any(|m| m.missing) {
                return None;
            }
            let mut env = Xxh3::new();
            env.update(b"rock-body-cache-env-v1\0");
            env.update(self.config.compiler_identity.as_bytes());
            env.update(build_tag.as_bytes());
            env.update(&[u8::from(skull_transform::inline_off_active())]);
            env.update(&[u8::from(ws.identity_storage_enforce_unpackaged)]);
            let mut facts = String::new();
            cg.write_cache_environment(&mut facts);
            env.update(facts.as_bytes());
            for name in scx.taken_by_ref_names() {
                env.update(name.as_bytes());
                env.update(b"\0");
            }
            env.update(comptime_environment(&ws.comptime_inputs).as_bytes());
            env.update(&(hashed.len() as u64).to_le_bytes());
            let mut bodies = FxHashMap::default();
            for hashed in &hashed {
                env.update(&hashed.env.to_le_bytes());
                for (func, hash) in &hashed.bodies {
                    if let Some(hash) = hash {
                        bodies.insert(*func, *hash);
                    }
                }
            }
            let env = env.digest128();
            let shard_path = self
                .config
                .dir
                .join(&self.config.project_key)
                .join(format!("{env:032x}.bin"));
            let shard = store::Shard::load(&shard_path, env).unwrap_or_default();
            Some(Prepared {
                env,
                bodies,
                shard,
                shard_path,
            })
        });
    }

    fn prepared(&self) -> Option<&Prepared> {
        self.prepared.get().and_then(Option::as_ref)
    }

    /// The record key of `body`, or `None` when its declaration is not
    /// hashable.
    fn key(
        &self,
        prepared: &Prepared,
        interner: &skull_mir::monoty::MonoInterner,
        body: &Instance,
    ) -> Option<u128> {
        let body_hash = *prepared.bodies.get(&body.func)?;
        let mut key = Xxh3::new();
        key.update(if body.args.is_empty() {
            b"body\0"
        } else {
            b"spec\0"
        });
        key.update(&prepared.env.to_le_bytes());
        key.update(&body.func.module.raw().to_le_bytes());
        key.update(&body.func.func.into_raw().into_u32().to_le_bytes());
        key.update(&body_hash.to_le_bytes());
        for arg in body.arg_types(interner) {
            key.update(arg.canonical().as_bytes());
            key.update(b"\0");
        }
        Some(key.digest128())
    }

    /// The chunk stored for `body`, when every fact it was built from still
    /// holds. The binding ranks are refreshed from the current call graph:
    /// the relation each capture decision read is verified, the number only
    /// orders the binding pass.
    pub(super) fn lookup(
        &self,
        ws: &Workspace,
        cg: &CodegenCtx,
        scx: &skull_mir::lower::ScalarCtx,
        body: &Instance,
    ) -> Option<Chunk> {
        let prepared = self.prepared()?;
        let Some(key) = self.key(prepared, scx.interner(), body) else {
            self.counters.uncacheable.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let chunk = self.read_record(ws, cg, scx, prepared, body, key);
        match &chunk {
            Some(_) => self.counters.hits.fetch_add(1, Ordering::Relaxed),
            None => self.counters.misses.fetch_add(1, Ordering::Relaxed),
        };
        chunk
    }

    fn read_record(
        &self,
        ws: &Workspace,
        cg: &CodegenCtx,
        scx: &skull_mir::lower::ScalarCtx,
        prepared: &Prepared,
        body: &Instance,
        key: u128,
    ) -> Option<Chunk> {
        let payload = prepared.shard.get(key)?;
        let mut reader = codec::Reader::new(payload);
        let _symbol = reader.str()?;
        let own_rank = cg.scc_of(body.func);
        if reader.bool()? != own_rank.is_some() {
            return None;
        }
        let deps = reader.len()?;
        for _ in 0..deps {
            let dep = reader.func_ref()?;
            let hash = reader.u128()?;
            if prepared.bodies.get(&dep) != Some(&hash) {
                return None;
            }
        }
        let facts = reader.len()?;
        for _ in 0..facts {
            let callee = reader.func_ref()?;
            let bindable = reader.bool()?;
            if bindable_callee(cg, own_rank, callee) != bindable {
                return None;
            }
        }
        let mut chunk = reader.chunk(scx.interner())?;
        if !reader.at_end() {
            return None;
        }
        let mut file_hashes = FxHashMap::default();
        if comptime_reads_stale(ws, &chunk.comptime_reads, &mut file_hashes) {
            return None;
        }
        for plan in &mut chunk.binds {
            plan.rank = own_rank.unwrap_or(0);
        }
        Some(chunk)
    }

    /// Store `chunk` as `body`'s emission. A body whose emit recorded a
    /// rejection is not stored: the rejection is part of its output.
    pub(super) fn record(
        &self,
        ws: &Workspace,
        cg: &CodegenCtx,
        scx: &skull_mir::lower::ScalarCtx,
        body: &Instance,
        chunk: &Chunk,
        rejected: bool,
    ) {
        let Some(prepared) = self.prepared() else {
            return;
        };
        // The lookup already counted a body without a key.
        if !prepared.bodies.contains_key(&body.func) {
            return;
        }
        let encoded = (!rejected)
            .then(|| self.encode(ws, cg, scx, prepared, body, chunk))
            .flatten();
        match encoded {
            Some(record) => self
                .pending
                .lock()
                .expect("body cache records")
                .push(record),
            None => {
                self.counters.uncacheable.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn encode(
        &self,
        ws: &Workspace,
        cg: &CodegenCtx,
        scx: &skull_mir::lower::ScalarCtx,
        prepared: &Prepared,
        body: &Instance,
        chunk: &Chunk,
    ) -> Option<(u128, Vec<u8>)> {
        let key = self.key(prepared, scx.interner(), body)?;
        let mut deps: Vec<FuncRef> = chunk.deps.iter().chain(&chunk.calls).copied().collect();
        deps.sort_by_key(|f| skull_mir::instance::DeclKey(*f).raw());
        deps.dedup();
        let own_rank = cg.scc_of(body.func);
        let mut writer = codec::Writer::new();
        writer.str(&body.symbol(ws, scx.interner()));
        writer.bool(own_rank.is_some());
        writer.len(deps.len());
        for dep in &deps {
            writer.func_ref(*dep);
            writer.u128(*prepared.bodies.get(dep)?);
        }
        writer.len(deps.len());
        for dep in &deps {
            writer.func_ref(*dep);
            writer.bool(bindable_callee(cg, own_rank, *dep));
        }
        writer.chunk(scx.interner(), chunk)?;
        Some((key, writer.out))
    }

    /// Append every recorded chunk to the shard and prune old shards.
    pub fn flush(&self) {
        let Some(prepared) = self.prepared() else {
            return;
        };
        let records = std::mem::take(&mut *self.pending.lock().expect("body cache records"));
        if records.is_empty() {
            return;
        }
        let stored = records.len();
        match prepared
            .shard
            .append(&prepared.shard_path, prepared.env, &records)
        {
            Ok(bytes) => {
                self.written.store(bytes, Ordering::Relaxed);
                if let Some(dir) = prepared.shard_path.parent() {
                    store::prune(dir, &prepared.shard_path);
                    store::prune_projects(&self.config.dir, dir);
                }
            }
            Err(error) => {
                eprintln!(
                    "warning: body cache: cannot write {} ({stored} chunks): {error}",
                    prepared.shard_path.display()
                );
            }
        }
        // The count survives the take so the stats still report it.
        self.pending
            .lock()
            .expect("body cache records")
            .extend(records.into_iter().map(|(key, _)| (key, Vec::new())));
    }
}

/// Whether `callee`'s component sits strictly below the caller's, which is
/// when [`skull_codegen_lua::callee_binding_plan`] captures it.
fn bindable_callee(cg: &CodegenCtx, own_rank: Option<u32>, callee: FuncRef) -> bool {
    matches!((own_rank, cg.scc_of(callee)), (Some(ours), Some(theirs)) if theirs < ours)
}

struct ModuleHashes {
    env: u128,
    bodies: Vec<(FuncRef, Option<u128>)>,
    missing: bool,
}

/// The environment contribution of one module and the hash of each of its
/// function bodies.
fn hash_module(module: &bone_semantic::hir::Module, text: Option<&str>) -> ModuleHashes {
    use bone_semantic::hir::Function;
    let Some(text) = text else {
        return ModuleHashes {
            env: 0,
            bodies: Vec::new(),
            missing: true,
        };
    };
    let body_of = |fid: Idx<Function>| module.bodies.get(&BodyOwner::Function(fid));
    // A Rock body block is cut out of the environment text; its text is the
    // body's own input. The signature around it stays.
    let mut holes: Vec<(usize, usize)> = module
        .functions
        .iter()
        .filter(|(_, f)| f.inline_body.is_none() && !f.is_extern)
        .filter_map(|(fid, _)| {
            let body = body_of(fid)?;
            let span = body.expr_spans.get(body.root?)?;
            let (start, end) = span.offset_range;
            (span.file == module.file && start <= end && end <= text.len()).then_some((start, end))
        })
        .collect();
    holes.sort_unstable();
    if holes.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        holes.clear();
    }
    let mut env = Xxh3::new();
    env.update(module.file.as_bytes());
    env.update(format!("{:?}|{:?}\0", module.package, module.identity_storage).as_bytes());
    let mut previous = 0;
    for (start, end) in &holes {
        env.update(&text.as_bytes()[previous..*start]);
        env.update(b"\0body\0");
        previous = *end;
    }
    env.update(&text.as_bytes()[previous..]);
    let bodies = module
        .functions
        .iter()
        .map(|(fid, f)| {
            let func = FuncRef {
                module: module.id,
                func: fid,
            };
            if f.is_synthetic {
                return (func, None);
            }
            let Some(body) = body_of(fid).filter(|body| body.root.is_some()) else {
                return (func, Some(DECLARATION_ONLY));
            };
            let mut hash = Xxh3::new();
            hash.update(b"fn\0");
            hash.update(f.name.as_bytes());
            hash_span(&mut hash, &f.span);
            let (start, end) = f.span.offset_range;
            if f.span.file == module.file && start <= end && end <= text.len() {
                hash.update(&text.as_bytes()[start..end]);
            }
            hash.update(&(body.expr_spans.iter().count() as u64).to_le_bytes());
            for (_, span) in body.expr_spans.iter() {
                hash_span(&mut hash, span);
            }
            hash.update(&(body.statement_spans.iter().count() as u64).to_le_bytes());
            for (_, span) in body.statement_spans.iter() {
                hash_span(&mut hash, span);
            }
            (func, Some(hash.digest128()))
        })
        .collect();
    ModuleHashes {
        env: env.digest128(),
        bodies,
        missing: false,
    }
}

fn hash_span(hash: &mut Xxh3, span: &bone_semantic::prelude::Span) {
    hash.update(span.file.as_bytes());
    for value in [
        span.line_range.0,
        span.line_range.1,
        span.col_range.0,
        span.col_range.1,
        span.offset_range.0,
        span.offset_range.1,
    ] {
        hash.update(&(value as u64).to_le_bytes());
    }
}

/// The build inputs in a canonical rendering. A volatile var (a build
/// stamp) and a var whose value is a fingerprint of files change without
/// the environment changing; a body that reads one records the value it
/// baked in, and the lookup checks that record instead.
fn comptime_environment(inputs: &bone_semantic::inference::ComptimeInputs) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut vars: Vec<_> = inputs
        .vars
        .iter()
        .filter(|(name, _)| {
            !inputs.volatile_vars.contains(*name) && !inputs.source_fingerprints.contains_key(*name)
        })
        .map(|(name, var)| (name.as_str(), var.value.as_str(), var.secret))
        .collect();
    vars.sort_unstable();
    for (name, value, secret) in vars {
        let _ = writeln!(out, "var {name}={value} secret={secret}");
    }
    let mut volatile: Vec<&str> = inputs.volatile_vars.iter().map(Ustr::as_str).collect();
    volatile.sort_unstable();
    let _ = writeln!(out, "volatile {volatile:?}");
    let _ = writeln!(
        out,
        "channel={} modules={} embed_root={:?}",
        inputs.channel, inputs.modules_mode, inputs.embed_root
    );
    let mut fingerprints: Vec<_> = inputs
        .source_fingerprints
        .iter()
        .map(|(name, fp)| {
            (
                name.as_str(),
                format!("{} {:?} {:?} {:?}", fp.secret, fp.roots, fp.files, fp.vars),
            )
        })
        .collect();
    fingerprints.sort_unstable();
    for (name, fp) in fingerprints {
        let _ = writeln!(out, "fingerprint {name}: {fp}");
    }
    let mut files: Vec<_> = inputs
        .source_fingerprint_files
        .iter()
        .map(|(name, hash)| (name.as_str(), *hash))
        .collect();
    files.sort_unstable();
    for (name, hash) in files {
        let _ = writeln!(out, "fingerprint-file {name}={hash:016x}");
    }
    out
}

/// A project's shard directory name: the hash of the path that identifies
/// it, so the directory tree never spells the path.
pub fn project_key(identity: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(identity.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_round_trips_records_and_drops_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p").join("shard.bin");
        let env = 7u128;
        let empty = store::Shard::load(&path, env).unwrap();
        assert_eq!(empty.len(), 0);
        empty
            .append(&path, env, &[(1, b"one".to_vec()), (2, b"two".to_vec())])
            .unwrap();
        let loaded = store::Shard::load(&path, env).unwrap();
        assert_eq!(loaded.get(1), Some(&b"one"[..]));
        assert_eq!(loaded.get(2), Some(&b"two"[..]));
        // A newer record for a key wins.
        loaded.append(&path, env, &[(1, b"uno".to_vec())]).unwrap();
        let loaded = store::Shard::load(&path, env).unwrap();
        assert_eq!(loaded.get(1), Some(&b"uno"[..]));
        assert_eq!(loaded.len(), 2);
        // A truncated tail record is skipped, the rest still reads.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 1);
        std::fs::write(&path, bytes).unwrap();
        let loaded = store::Shard::load(&path, env).unwrap();
        assert_eq!(loaded.get(1), Some(&b"one"[..]));
        assert_eq!(loaded.get(2), Some(&b"two"[..]));
        // Another environment's shard reads as empty.
        assert_eq!(store::Shard::load(&path, 8).unwrap().len(), 0);
    }

    #[test]
    fn environment_switch_selects_the_directory() {
        let dir = |value: Option<&str>| BodyCacheConfig::dir_for(value.map(std::ffi::OsStr::new));
        assert!(dir(Some("0")).is_none());
        assert!(dir(Some("")).is_none());
        assert!(dir(Some("off")).is_none());
        assert_eq!(dir(Some("/tmp/x")), Some(PathBuf::from("/tmp/x")));
        assert_eq!(dir(Some("1")), BodyCacheConfig::default_dir());
    }
}
