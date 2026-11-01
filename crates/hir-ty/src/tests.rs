//! Tests of hir-ty as a whole: what its queries allocate.

mod adjusted_ty;
mod budget;
mod coercion_lub;
mod const_args;
mod const_bounds;
mod const_param_ty;
mod elided_lifetime_args;
mod fn_cache;
mod hir_eval_shifts;
mod impossible_items;
mod index_places;
mod infer_vars;
mod inhabitedness;
mod layout_errors;
mod lowering_errors;
mod method_resolutions;
mod probe_errors;
mod qualifier_resolutions;
mod query_span_names;
mod ref_mut_bindings;
mod sizedness;
mod struct_paths;
mod trait_item_imports;
