# blaze's changes to the standard library

library/ is the Rust project's standard library at the commit of blaze's nightly
(UPSTREAM), unchanged but for the changes listed here. `python3 harness/library.py check '{}'`
holds it to that: a file that differs from upstream and is not named below fails it.

To change the standard library: edit it under library/, commit, and add an entry below in the
same commit, naming each file it changes, what it changes and why. The daily bump moves
library/ to the next nightly as a merge (`harness/library.py merge`), so the change is kept;
a conflict stops the bump with an issue (docs/toolchain.md, "The daily bump").

None yet.
