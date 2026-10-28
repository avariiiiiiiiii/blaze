//! A macro to implement `HirDatabase` for any (`Sized`) type implementing `SourceDatabase`.

#[macro_export]
macro_rules! impl_hir_database {
    ($ty:ty) => {
        const _: () = {
            use $crate::{
                __private::{hir_def::ModuleId, salsa},
                InferBodyId,
                db::{HirDatabase, InternedClosureId, InternedCoroutineId, InternedOpaqueTyId},
                display::{DisplayTarget, HirDisplay},
                mir,
                mir::{MirBody, MirEvalError, MirLowerError, MirOptLevel},
                next_solver::{Allocation, GenericArgs, StoredGenericArgs, Ty},
                traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
            };

            #[salsa::db]
            impl $crate::db::HirDatabase for $ty {
                fn as_dyn(&self) -> &dyn HirDatabase {
                    self
                }

                fn mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::mir_body_query(self, def).map_err(|err| err.clone())
                }

                fn mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::mir_body_for_closure_query(self, def).map_err(|err| err.clone())
                }

                fn monomorphized_mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_mir_body_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_mir_body_for_closure_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn mir_body_for_coroutine<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::mir_body_for_coroutine_query(self, def).map_err(|err| err.clone())
                }

                fn mir_body_for_coroutine_by_move<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::mir_body_for_coroutine_by_move_query(self, def).map_err(|err| err.clone())
                }

                fn monomorphized_mir_body_for_coroutine_by_move<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_mir_body_for_coroutine_by_move_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_mir_body_for_coroutine<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_mir_body_for_coroutine_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn runtime_mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::runtime_mir_body_query(self, def).map_err(|err| err.clone())
                }

                fn runtime_mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::runtime_mir_body_for_closure_query(self, def).map_err(|err| err.clone())
                }

                fn runtime_mir_body_for_coroutine<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::runtime_mir_body_for_coroutine_query(self, def).map_err(|err| err.clone())
                }

                fn runtime_mir_body_for_coroutine_by_move<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::runtime_mir_body_for_coroutine_by_move_query(self, def)
                        .map_err(|err| err.clone())
                }

                fn optimized_mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                    level: MirOptLevel,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::optimized_mir_body_query(self, def, level).map_err(|err| err.clone())
                }

                fn optimized_mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                    level: MirOptLevel,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::optimized_mir_body_for_closure_query(self, def, level)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_optimized_mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                    level: MirOptLevel,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_optimized_mir_body_query(self, def, level, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_optimized_mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                    level: MirOptLevel,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_optimized_mir_body_for_closure_query(
                        self, def, level, subst, env,
                    )
                    .map_err(|err| err.clone())
                }

                fn monomorphized_runtime_mir_body<'db>(
                    &'db self,
                    def: InferBodyId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_runtime_mir_body_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_runtime_mir_body_for_closure<'db>(
                    &'db self,
                    def: InternedClosureId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_runtime_mir_body_for_closure_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_runtime_mir_body_for_coroutine<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_runtime_mir_body_for_coroutine_query(self, def, subst, env)
                        .map_err(|err| err.clone())
                }

                fn monomorphized_runtime_mir_body_for_coroutine_by_move<'db>(
                    &'db self,
                    def: InternedCoroutineId<'db>,
                    subst: StoredGenericArgs,
                    env: StoredParamEnvAndCrate,
                ) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
                    mir::monomorphized_runtime_mir_body_for_coroutine_by_move_query(
                        self, def, subst, env,
                    )
                    .map_err(|err| err.clone())
                }

                fn interpret_mir<'db>(
                    &'db self,
                    body: &'db MirBody<'db>,
                    assert_placeholder_ty_is_unused: bool,
                    trait_env: Option<ParamEnvAndCrate<'db>>,
                ) -> Result<Allocation<'db>, MirEvalError<'db>> {
                    mir::interpret_mir(self, body, assert_placeholder_ty_is_unused, trait_env)
                        .and_then(|(value, _)| value)
                }

                fn opaque_of_impl<'db>(
                    &'db self,
                    env: ParamEnvAndCrate<'db>,
                    opaque: InternedOpaqueTyId<'db>,
                    args: GenericArgs<'db>,
                ) -> Option<Ty<'db>> {
                    mir::opaque_of_impl(self, env, opaque, args)
                }

                fn type_name<'db>(&'db self, ty: Ty<'db>, module: ModuleId) -> String {
                    match ty.display_source_code(self, module, true) {
                        Ok(ty_name) => ty_name,
                        // Fallback to human readable display in case of `Err`. Ideally we want to use `display_source_code` to
                        // render full paths.
                        Err(_) => {
                            let krate = module.krate(self);
                            ty.display(self, DisplayTarget::from_crate(self, krate)).to_string()
                        }
                    }
                }
            }
        };
    };
}
