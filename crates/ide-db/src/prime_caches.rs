//! rust-analyzer is lazy and doesn't compute anything unless asked. This
//! sometimes is counter productive when, for example, the first goto definition
//! request takes longer to compute. This module implements prepopulation of
//! various caches, it's not really advanced at the moment.
use std::panic::AssertUnwindSafe;

use hir::import_map::ImportMap;
use rustc_hash::{FxHashMap, FxHashSet};
use salsa::{Cancelled, Database};

use crate::{RootDatabase, base_db::Crate, symbol_index::SymbolIndex};

/// Warm caches for `scope`.
///
/// `scope` must be closed under transitive dependencies: the scheduler only
/// follows reverse-dep edges within `scope`. Out-of-scope crates can still be
/// primed by salsa on demand when a scope crate's queries reach into them.
/// Callers that want to prime everything pass `&all_crates(db)`.
pub fn parallel_prime_caches(
    db: &RootDatabase,
    scope: &[Crate],
    num_worker_threads: usize,
) {
    if scope.is_empty() {
        return;
    }
    let _p = tracing::info_span!("parallel_prime_caches", scope_size = scope.len()).entered();

    enum ParallelPrimeCacheWorkerProgress {
        EndCrateDefMap { crate_id: Crate },
        EndCrateImportMap,
        EndSema,
        EndModuleSymbols,
        Cancelled(Cancelled),
    }

    // The setup here is a bit complicated. We try to make best use of compute resources.
    // The idea is that if we have a def map available to compute, we should do that first.
    // This is because def map is a dependency of both import map and symbols. So if we have
    // e.g. a def map and a symbols, if we compute the def map we can, after it completes,
    // compute the def maps of dependencies, the existing symbols and the symbols of the
    // new crate, all in parallel. But if we compute the symbols, after that we will only
    // have the def map to compute, and the rest of the CPU cores will rest, which is not
    // good.
    // However, it's better to compute symbols/import map than to compute a def map that
    // isn't ready yet, because one of its dependencies hasn't yet completed its def map.
    // Such def map will just block on the dependency, which is just wasted time. So better
    // to compute the symbols/import map of an already computed def map in that time.

    let scope_set: FxHashSet<Crate> = scope.iter().copied().collect();

    let (reverse_deps, mut to_be_done_deps) = {
        // Only count in-scope deps — otherwise an out-of-scope dep would
        // leave the scheduler waiting on a crate it never enqueued.
        let to_be_done_deps = scope
            .iter()
            .map(|&krate| {
                let count = krate
                    .data(db)
                    .dependencies
                    .iter()
                    .filter(|dep| scope_set.contains(&dep.crate_id))
                    .count() as u32;
                (krate, count)
            })
            .collect::<FxHashMap<_, _>>();
        let mut reverse_deps =
            scope.iter().map(|&krate| (krate, Vec::new())).collect::<FxHashMap<_, _>>();
        for &krate in scope {
            for dep in &krate.data(db).dependencies {
                if let Some(rev) = reverse_deps.get_mut(&dep.crate_id) {
                    rev.push(krate);
                }
            }
        }
        (reverse_deps, to_be_done_deps)
    };

    // Every worker reads `db` by reference, so they are joined before this returns; they stop
    // once the work senders below drop.
    std::thread::scope(|workers| {
        let (
            def_map_work_sender,
            import_map_work_sender,
            symbols_work_sender,
            sema_work_sender,
            progress_receiver,
        ) = {
            let (progress_sender, progress_receiver) = crossbeam_channel::unbounded();
            let (def_map_work_sender, def_map_work_receiver) = crossbeam_channel::unbounded();
            let (import_map_work_sender, import_map_work_receiver) = crossbeam_channel::unbounded();
            let (sema_work_sender, sema_work_receiver) = crossbeam_channel::unbounded();
            let (symbols_work_sender, symbols_work_receiver) = crossbeam_channel::unbounded();
            let prime_caches_worker = move |db: &RootDatabase| {
                let handle_def_map = |crate_id| {
                    let cancelled = Cancelled::catch(|| {
                        _ = hir::crate_def_map(db, crate_id);
                    });

                    match cancelled {
                        Ok(()) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::EndCrateDefMap { crate_id })?,
                        Err(cancelled) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::Cancelled(cancelled))?,
                    }

                    Ok::<_, crossbeam_channel::SendError<_>>(())
                };
                let handle_sema = |crate_id| {
                    let cancelled = Cancelled::catch(|| {
                        hir::attach_db(db, || {
                            // method resolution is likely to hit all trait impls at some point
                            // we pre-populate it here as this will hit a lot of parses ...
                            // This also computes the lang items, which is what we want as the work for them is also highly recursive and will be trigger by the module symbols query
                            // slowing down leaf crate analysis tremendously as we go back to being blocked on a single thread
                            _ = hir::TraitImpls::for_crate(db, crate_id);
                        })
                    });

                    match cancelled {
                        Ok(()) => progress_sender.send(ParallelPrimeCacheWorkerProgress::EndSema)?,
                        Err(cancelled) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::Cancelled(cancelled))?,
                    }

                    Ok::<_, crossbeam_channel::SendError<_>>(())
                };
                let handle_import_map = |crate_id| {
                    let cancelled = Cancelled::catch(|| _ = ImportMap::of(db, crate_id));

                    match cancelled {
                        Ok(()) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::EndCrateImportMap)?,
                        Err(cancelled) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::Cancelled(cancelled))?,
                    }

                    Ok::<_, crossbeam_channel::SendError<_>>(())
                };
                let handle_symbols = |module: hir::Module| {
                    let cancelled = Cancelled::catch(AssertUnwindSafe(|| {
                        _ = SymbolIndex::module_symbols(db, module)
                    }));

                    match cancelled {
                        Ok(()) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::EndModuleSymbols)?,
                        Err(cancelled) => progress_sender
                            .send(ParallelPrimeCacheWorkerProgress::Cancelled(cancelled))?,
                    }

                    Ok::<_, crossbeam_channel::SendError<_>>(())
                };

                loop {
                    db.unwind_if_revision_cancelled();

                    // Biased because we want to prefer def maps.
                    crossbeam_channel::select_biased! {
                        recv(def_map_work_receiver) -> work => {
                            let Ok(crate_id) = work else { break };
                            handle_def_map(crate_id)?;
                        }
                        recv(sema_work_receiver) -> work => {
                            let Ok(crate_id) = work else { break };
                            handle_sema(crate_id)?;
                        }
                        recv(import_map_work_receiver) -> work => {
                            let Ok(crate_id) = work else { break };
                            handle_import_map(crate_id)?;
                        }
                        recv(symbols_work_receiver) -> work => {
                            let Ok(module) = work else { break };
                            handle_symbols(module)?;
                        }
                    }
                }
                Ok::<_, crossbeam_channel::SendError<_>>(())
            };

            for id in 0..num_worker_threads {
                let worker = prime_caches_worker.clone();
                std::thread::Builder::new()
                    .name(format!("PrimeCaches#{id}"))
                    .stack_size(stdx::thread::DEFAULT_STACK_SIZE)
                    .spawn_scoped(workers, move || worker(db))
                    .expect("failed to spawn thread");
            }

            (
                def_map_work_sender,
                import_map_work_sender,
                symbols_work_sender,
                sema_work_sender,
                progress_receiver,
            )
        };

        let crate_def_maps_total = scope.len();
        let mut crate_def_maps_done = 0;
        let (mut crate_import_maps_total, mut crate_import_maps_done) = (0usize, 0usize);
        let (mut module_symbols_total, mut module_symbols_done) = (0usize, 0usize);
        let (mut sema_total, mut sema_done) = (0usize, 0usize);

        for (&krate, &to_be_done_deps) in &to_be_done_deps {
            if to_be_done_deps != 0 {
                continue;
            }

            def_map_work_sender.send(krate).ok();
        }

        while crate_def_maps_done < crate_def_maps_total
            || crate_import_maps_done < crate_import_maps_total
            || module_symbols_done < module_symbols_total
            || sema_done < sema_total
        {
            db.unwind_if_revision_cancelled();

            // Every worker has exited.
            let Ok(progress) = progress_receiver.recv() else { return };

            match progress {
                ParallelPrimeCacheWorkerProgress::EndCrateDefMap { crate_id } => {
                    crate_def_maps_done += 1;

                    // Fire ready dependencies.
                    for &dep in &reverse_deps[&crate_id] {
                        let to_be_done = to_be_done_deps.get_mut(&dep).unwrap();
                        *to_be_done -= 1;
                        if *to_be_done == 0 {
                            def_map_work_sender.send(dep).ok();
                        }
                    }

                    sema_work_sender.send(crate_id).ok();
                    sema_total += 1;
                    let origin = &crate_id.data(db).origin;
                    if origin.is_lang() {
                        crate_import_maps_total += 1;
                        import_map_work_sender.send(crate_id).ok();
                    } else if origin.is_local() {
                        // Compute the symbol search index.
                        // This primes the cache for `ide_db::symbol_index::world_symbols()`.
                        //
                        // We do this for workspace crates only (members of local_roots), because doing it
                        // for all dependencies could be *very* unnecessarily slow in a large project.
                        //
                        // FIXME: We should do it unconditionally if the configuration is set to default to
                        // searching dependencies (rust-analyzer.workspace.symbol.search.scope), but we
                        // would need to pipe that configuration information down here.
                        let modules = hir::Crate::from(crate_id).modules(db);
                        module_symbols_total += modules.len();
                        for module in modules {
                            symbols_work_sender.send(module).ok();
                        }
                    }
                }
                ParallelPrimeCacheWorkerProgress::EndCrateImportMap => crate_import_maps_done += 1,
                ParallelPrimeCacheWorkerProgress::EndModuleSymbols => module_symbols_done += 1,
                ParallelPrimeCacheWorkerProgress::EndSema => sema_done += 1,
                ParallelPrimeCacheWorkerProgress::Cancelled(cancelled) => {
                    // Cancelled::throw should probably be public
                    std::panic::resume_unwind(Box::new(cancelled));
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use test_fixture::WithFixture;

    use crate::{RootDatabase, base_db::all_crates};

    #[test]
    fn primes_a_dependency_chain_and_returns() {
        let db = RootDatabase::with_files(
            "//- /a.rs crate:a\npub fn f() {}\n//- /b.rs crate:b deps:a\npub fn g() { a::f() }\n",
        );
        let all = all_crates(&db);
        super::parallel_prime_caches(&db, &all, 2);
        super::parallel_prime_caches(&db, &[], 2);
    }
}
