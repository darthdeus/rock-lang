use super::*;

pub fn emit_incremental(
    ws: &Workspace,
    cache: &mut SkullEmitCache,
    reinferred: &[(ModuleId, BodyOwner)],
) -> Result<MirEmit, CompilerError> {
    // Shader registrations first: they are graph roots, so the cache update must
    // see this edit's set.
    let shader_changed = cache.update_shader_registrations(ws, reinferred)?;
    let (_affected, update, added_errors) = cache.update_chunk_cache(ws, reinferred, &[])?;
    if let Some(err) = SkullEmitCache::combine_errors(added_errors) {
        return Err(err);
    }
    if shader_changed || cache.shader_delta_affects_view(&update) {
        cache.refresh_shader_registrations(ws)?;
    }

    let mut built_specs: Vec<(Instance, Chunk)> = Vec::new();
    let (mut emit, emitted, unit_reads) = assemble(
        ws,
        cache,
        &mut built_specs,
        CacheLifetime::Retained,
        None,
        None,
    )?;
    cache.retain_assembly(ws, &emit, emitted, unit_reads, built_specs);
    emit.compile_counts.chunks = cache.chunks.len();
    // A codegen rejection is recorded on the shared `CodegenCtx`, so it must
    // be drained here or it would surface at a later, unrelated emit.
    if let Some(err) = cache.cg.take_rejection() {
        return Err(err);
    }
    Ok(emit)
}
