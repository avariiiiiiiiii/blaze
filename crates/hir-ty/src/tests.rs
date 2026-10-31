//! Tests of hir-ty as a whole: what its queries allocate.

mod adjusted_ty;
mod budget;
mod const_args;
mod const_bounds;
mod const_param_ty;
mod const_valtree;
mod elided_lifetime_args;
mod fn_cache;
mod infer_vars;
mod inhabitedness;
mod layout_errors;
mod lowering_errors;
mod probe_errors;
mod qualifier_resolutions;
mod ref_mut_bindings;
mod sizedness;
mod struct_paths;
mod trait_item_imports;
