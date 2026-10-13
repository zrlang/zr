# Reduced Syntax for `zr0`

## Status: ⬜ Not started

## Goal
Define the smallest possible grammar that is still useful and can be compiled
to C. Target ~15 keywords.

## Grammar (subset)
```
program := item*
item    := fn_decl | struct_decl | extern_decl
fn_decl := 'fn' ident '(' params ')' type block
block   := '{' stmt* '}'
stmt    := let_stmt | if_stmt | while_stmt | return_stmt | expr_stmt
expr    := lit | var | unary_op expr | expr bin_op expr
         | expr '(' args ')' | expr '.' field
type    := 'i32' | 'bool' | 'void' | type '*' | type '[' expr ']'
```

## Excluded (for now)
- generics
- traits
- async/await
- match
- closures
- modules

These are re-introduced in Stage 1.
