" Minimal synchronous lexical highlighting for Rock.
"
" All rich highlighting (identifiers, types, params, properties) comes from
" the LSP's semantic tokens, which override syntax via higher extmark
" priority. This file only covers the lexical layer the server is slow to
" (re)send on a debounce -- comments, strings, numbers, keywords -- so a
" line greys/colors the instant you type it instead of flickering as code
" until the semantic-token response lands. Deliberately no PascalCase /
" ALL_CAPS / call-shape guessing: that produces false positives and fights
" the LSP once its tokens arrive.

if exists("b:current_syntax")
  finish
endif

syntax sync minlines=200

" Triple-quoted (inline-lua / raw) strings first so // and " inside them
" don't start a comment or single string.
syn region rockTripleString start=+"""+ end=+"""+ keepend

syn region rockCommentLine start=+//+ end=+$+ contains=@Spell
syn region rockCommentBlock start=+/\*+ end=+\*/+ contains=rockCommentBlock,@Spell

syn region rockString start=+"+ skip=+\\.+ end=+"+ contains=rockStringInterp
syn region rockStringInterp start=+{+ end=+}+ contained
syn match rockCharLit +'\(\\.\|[^']\)'+

syn match rockNumber +\<\d\+\(\.\d\+\)\=\([eE][-+]\=\d\+\)\=\>+
syn match rockNumber +\<0x[0-9a-fA-F]\+\>+
syn match rockNumber +\<0b[01]\+\>+

syn keyword rockKeyword fn let return global const if else for in while match loop break continue co yield
syn keyword rockKeyword struct enum interface type extern value macro static co
syn keyword rockBoolean true false
syn keyword rockNull null

hi def link rockCommentLine   Comment
hi def link rockCommentBlock  Comment
hi def link rockString        String
hi def link rockTripleString  String
hi def link rockCharLit       Character
hi def link rockStringInterp  Special
hi def link rockNumber        Number
hi def link rockKeyword       Keyword
hi def link rockBoolean       Boolean
hi def link rockNull          Constant

let b:current_syntax = "rock"
