# Requests of speed-backend-llvm

1. To `codegen-adapter` (in `v2/requests/codegen-adapter.md`, "From speed-backend-llvm"):
   `v2/backend.rs:496` `prepared: &Prepared` becomes `prepared: &mut Prepared`, as the trait
   already says.
