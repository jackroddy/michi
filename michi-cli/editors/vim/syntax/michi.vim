" Vim syntax file for the .michi pipeline format.
" Command text inside a step body is highlighted as sh and ends at `;`.

if exists('b:current_syntax')
  finish
endif

" the stock sh syntax, included as a cluster for command text
unlet! b:current_syntax
syntax include @michiShell syntax/sh.vim
unlet! b:current_syntax

syn match michiShebang /\%^#!.*/
syn match michiComment /\/\/.*$/

" a sweep variable or data scalar, in michi text and in command text
syn match michiVar /\$\%({\h\w*\%(\.\h\w*\)\=}\|\h\w*\)/

" strings, and raw strings that carry as many # as they opened with
syn region michiString start=/"/ skip=/\\./ end=/"/ contains=michiVar
syn region michiRaw start=/r\z(#*\)"/ end=/"\z1/

syn match michiNumber /-\=\<\d\+\%(\.\d\+\)\=\%(ms\|s\|m\|h\)\=\>/
syn match michiOp /\.\.=\=\|:\*\=\ze\d/
syn match michiDelim /[{}()\[\],=;]/
syn match michiReserved /\<\%(let\|def\|fn\|for\|in\|if\|else\|use\|import\|include\|true\|false\)\>/
syn match michiKeyword /\<\%(data\|pipeline\)\>/

" attributes: #[name(args), flag]
syn region michiAttr matchgroup=michiAttrBr start=/#\[/ end=/\]/
      \ contains=michiAttrName,michiAttrArgs,michiComment
syn match michiAttrName /\<\h\w*\>/ contained
syn region michiAttrArgs matchgroup=michiDelim start=/(/ end=/)/ contained
      \ contains=michiAttrArgs,michiAttrKey,michiString,michiVar,michiNumber,michiOp,michiDelim,michiReserved,michiComment
syn match michiAttrKey /\<\h\w*\>\ze\s*=/ contained

" parameter lists: <N = [1, 2]>, <(N, L) = a.B>, at a line start or on a step header
syn region michiParams matchgroup=michiParamBr start=/^\s*\zs</ end=/>/ contains=@michiParamItems
syn region michiParams matchgroup=michiParamBr start=/</ end=/>/ contained contains=@michiParamItems
syn match michiParamName /\<\h\w*\>/ contained
syn cluster michiParamItems contains=michiParamName,michiReserved,michiNumber,michiOp,michiDelim,michiString,michiComment

" step header, then its body of attributes, sweeps, comments and commands
syn region michiStep matchgroup=michiKeyword start=/\<step\>/ end=/\ze{/
      \ contains=michiParams,michiStepName,michiComment nextgroup=michiBody skipwhite skipnl
syn match michiStepName /\<\h\w*\>/ contained
syn region michiBody matchgroup=michiDelim start=/{/ end=/}/ contained
      \ contains=@michiBodyItems
syn cluster michiBodyItems contains=michiComment,michiAttr,michiParams,michiRaw,michiCmd,michiDelim,michiGroup
syn region michiGroup matchgroup=michiDelim start=/{/ end=/}/ contained transparent
      \ contains=@michiBodyItems

" a command starts at a line start or after `;` or `{`, unless it is another construct
syn region michiCmd start=/\%(^\s*\|[;{]\s*\)\@<=\%(#\[\|<\|}\|\/\/\|r#*"\|;\)\@!\ze\S/ end=/\ze;/ contained keepend
      \ skip=/\\.\|'[^']*'\|"\%(\\.\|\_[^"\\]\)*"\|{\_[^}]*}\|(\_[^)]*)/
      \ contains=@michiShell,michiVar

" a $NAME inside sh double quotes and command substitutions
syn cluster shDblQuoteList add=michiVar
syn cluster shCommandSubList add=michiVar
syn cluster shEchoList add=michiVar

syn cluster michiTop contains=michiComment,michiAttr,michiKeyword,michiReserved,michiStep,
      \michiParams,michiNumber,michiOp,michiString,michiRaw,michiVar,michiDelim,michiBlock,michiShebang
syn region michiBlock matchgroup=michiDelim start=/{/ end=/}/ transparent contains=@michiTop

hi def link michiShebang Comment
hi def link michiComment Comment
hi def link michiKeyword Keyword
hi def link michiReserved Error
hi def link michiAttrBr PreProc
hi def link michiAttrName PreProc
hi def link michiAttrKey Identifier
hi def link michiParamBr Special
hi def link michiParamName Identifier
hi def link michiStepName Function
hi def link michiVar Identifier
hi def link michiString String
hi def link michiRaw String
hi def link michiNumber Number
hi def link michiOp Operator
hi def link michiDelim Delimiter

let b:current_syntax = 'michi'
