# lsp-types [![Build Status](https://travis-ci.org/gluon-lang/lsp-types.svg?branch=master)](https://travis-ci.org/gluon-lang/lsp-types) [![Documentation](https://docs.rs/lsp-types/badge.svg)](https://docs.rs/crate/lsp-types)

> **Rock fork.** This crate is a hand-maintained fork of upstream
> [lsp-types](https://github.com/gluon-lang/lsp-types) 0.97.0 with manual serde
> impls; `lsp-serde-test` checks it against upstream. Protocol modules nothing
> in the Rock workspace used were removed (LSIF, notebooks, call and type
> hierarchy, code lens, document links, folding and selection ranges, linked
> editing, monikers, inline values and completions, pull diagnostics and file
> operations), along with their requests, notifications and capability fields.

Types useful for interacting with a [language server](https://code.visualstudio.com/blogs/2016/06/27/common-language-protocol).

Supports Language Server Protocol (LSP) version 3.16.0.

Proposed 3.17 features can be activated using the `proposed` feature flag.
- **NOTE** that these are unstable and may change between releases.

## Contributing

If you are making a change which adds, removes or modifies the LSP API it is highly appreciated if you link to the spec where this change is described. This gives context to whether the change should be an experimental addition and lets the reviewer double check the changes easily against the spec.

## Links

[Stable Protocol reference](https://github.com/microsoft/language-server-protocol/tree/gh-pages/_specifications/lsp/3.17/specification.md)

[Proposed Protocol reference](https://github.com/microsoft/language-server-protocol/blob/gh-pages/_specifications/lsp/3.18/specification.md)
