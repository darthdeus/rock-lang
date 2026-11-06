# JSON conformance corpus

`cases/` is the `test_parsing/` set from [nst/JSONTestSuite][jts], vendored
verbatim at 318 files (~354KB). MIT licensed; see `LICENSE`.

Case names carry their expectation as a prefix: `y_` must be accepted, `n_`
must be rejected, `i_` is implementation-defined. `manifest.txt` restates that
as an explicit list and is the denominator `tests/json_conformance.rock`
asserts against -- the harness reads it rather than listing the directory, so
a case that disappears fails the run instead of shrinking it.

Cases are read from disk rather than embedded as Rock literals because
several carry invalid UTF-8, lone surrogates, or raw control bytes that would
not survive escaping.

Known divergences and the two cases that abort the process are pinned as
sentinels in the harness itself, with the reasons.

[jts]: https://github.com/nst/JSONTestSuite
