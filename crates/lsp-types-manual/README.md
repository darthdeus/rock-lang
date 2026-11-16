# lsp-types-manual

The subset of [lsp-types](https://github.com/gluon-lang/lsp-types) 0.97.0 that
`rock-lsp` and `bone-query` exchange with editors, with hand-written serde
impls in place of the derive macros.

Only the types, fields and variants the server sends or reads are kept. Unknown
JSON fields are ignored on input, so a client sending more of the protocol than
is modelled here is still understood. The server's capabilities are a JSON
literal in `rock-lsp`, not types in this crate.

`lsp-serde-test` round-trips generated messages through this crate and upstream
and reports any difference in output.
