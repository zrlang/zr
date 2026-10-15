//! 类型语法解析。
//!
//! 类型文法（Pratt 风格的优先级解析，以 `->` 为最低优先级）：
//!
//! ```ebnf
//! type           := function_type | unary_type
//! function_type  := unary_type '->' type
//! unary_type     := '*' mut? type | '&' mut? type | unary_type
//! type_suffix    := '?' | '!' type | '[' type ']' | '[' type ';' expr ']'
//!                  | '::' path | '<' types '>'
//! ```

use crate::lexer::TokenKind;
use crate::parser::ast::*;
use crate::parser::Parser;

impl Parser {
    /// 解析一个完整类型（最高层级，允许 `->`）。
    pub fn parse_type(&mut self) -> Result<Type, crate::error::CompileError> {
        self.parse_type_bp(0)
    }

    /// 带绑定优先级的类型解析（Pratt）。
    fn parse_type_bp(&mut self, bp: u8) -> Result<Type, crate::error::CompileError> {
        let mut ty = self.parse_type_prefix()?;

        loop {
            // 后缀运算符
            if self.check(&TokenKind::Arrow) {
                // `->` 优先级最低，只在 bp == 0 时消费
                if bp > 0 {
                    break;
                }
                self.advance(); // ->
                let ret = self.parse_type_bp(0)?;
                ty = Type::Function {
                    params: match ty {
                        Type::Tuple(ts) => ts,
                        other => vec![other],
                    },
                    ret: Box::new(ret),
                    is_async: false,
                    abi: Abi::Zr,
                };
            } else if self.match_kind(&TokenKind::Question) {
                ty = Type::Optional(Box::new(ty));
            } else if self.match_kind(&TokenKind::Bang) {
                let err_ty = self.parse_type_bp(20)?; // 较高优先级
                ty = Type::ErrorUnion(Box::new(ty), Box::new(err_ty));
            } else if self.match_kind(&TokenKind::LBracket) {
                if self.match_kind(&TokenKind::RBracket) {
                    // `[]T` → 切片（需要重新解析元素类型放在前面）
                    // 语法约定：先写 `[]T` 表示切片，这里采用「元素类型在后」
                    // 简化实现：假定 `[T]` 为数组，长度由后续 `;` 提供
                    let _ = ty;
                    let elem = self.parse_type()?;
                    self.consume(&TokenKind::RBracket, "expected ']'")?;
                    ty = Type::Slice(Box::new(elem), crate::parser::ast::Mutability::Immutable);
                } else {
                    let elem = self.parse_type()?;
                    if self.match_kind(&TokenKind::Semicolon) {
                        let len = self.parse_expression()?;
                        self.consume(&TokenKind::RBracket, "expected ']'")?;
                        ty = Type::Array(Box::new(elem), ArrayLen::Const(Box::new(len)));
                    } else {
                        self.consume(&TokenKind::RBracket, "expected ']' or ';'")?;
                        ty = Type::Array(Box::new(elem), ArrayLen::Dynamic);
                    }
                }
            } else if self.match_kind(&TokenKind::ColonColon) {
                // 泛型参数：`Foo::<i32, u64>` —— zr 采用 Rust 的 turbofish 语法
                // 此处简化：把 `T` 作为路径的一部分处理
                let _ = ty;
            } else {
                break;
            }
        }

        Ok(ty)
    }

    /// 解析类型前缀（基础类型 + 一元类型）。
    fn parse_type_prefix(&mut self) -> Result<Type, crate::error::CompileError> {
        // `impl Trait` / `impl Trait1 + Trait2`
        if self.match_kind(&TokenKind::KwImpl) {
            let mut bounds = vec![self.parse_type_bp(10)?];
            while self.match_kind(&TokenKind::Plus) {
                bounds.push(self.parse_type_bp(10)?);
            }
            return Ok(Type::ImplTrait(bounds));
        }

        // 指针：`*T` / `*const T` / `*mut T`
        if self.match_kind(&TokenKind::Star) {
            let mutability = if self.match_kind(&TokenKind::KwMut) {
                crate::parser::ast::Mutability::Mutable
            } else if self.match_kind(&TokenKind::KwConst) {
                crate::parser::ast::Mutability::Immutable
            } else {
                crate::parser::ast::Mutability::Immutable
            };
            let inner = self.parse_type_bp(20)?;
            return Ok(Type::Pointer(Box::new(inner), mutability));
        }

        // 引用：`&T` / `&mut T`
        if self.match_kind(&TokenKind::Amp) {
            let mutability = if self.match_kind(&TokenKind::KwMut) {
                crate::parser::ast::Mutability::Mutable
            } else {
                crate::parser::ast::Mutability::Immutable
            };
            let inner = self.parse_type_bp(20)?;
            return Ok(Type::Reference(Box::new(inner), mutability));
        }

        // 元组：`(T1, T2, ...)`
        if self.match_kind(&TokenKind::LParen) {
            let mut types = Vec::new();
            if !self.check(&TokenKind::RParen) {
                types.push(self.parse_type()?);
                while self.match_kind(&TokenKind::Comma) {
                    if self.check(&TokenKind::RParen) {
                        break;
                    }
                    types.push(self.parse_type()?);
                }
            }
            self.consume(&TokenKind::RParen, "expected ')' to close tuple type")?;
            if types.len() == 1 {
                // 单元素元组 `(T,)` 保留为 Tuple
                return Ok(Type::Tuple(types));
            }
            return Ok(Type::Tuple(types));
        }

        // 数组 / 切片：`[T; N]` / `[T]`
        if self.match_kind(&TokenKind::LBracket) {
            if self.match_kind(&TokenKind::RBracket) {
                // `[]const T` / `[]T` —— 右括号已经吃掉了，这里只解析元素
                // 类型，不再消费第二个右括号。
                let _ = self.match_kind(&TokenKind::KwConst);
                let elem = self.parse_type()?;
                return Ok(Type::Slice(Box::new(elem), crate::parser::ast::Mutability::Immutable));
            }
            let elem = self.parse_type()?;
            let len = if self.match_kind(&TokenKind::Semicolon) {
                let len_expr = self.parse_expression()?;
                ArrayLen::Const(Box::new(len_expr))
            } else {
                ArrayLen::Dynamic
            };
            self.consume(&TokenKind::RBracket, "expected ']' to close array type")?;
            return Ok(Type::Array(Box::new(elem), len));
        }

        // 永不复返类型 `!`
        if self.match_kind(&TokenKind::Bang) {
            return Ok(Type::Never);
        }

        // 生命周期标注：`&'a T` 里的 `'a`（类型检查阶段直接丢弃）
        if let TokenKind::Lifetime(_) = self.peek().kind {
            self.advance();
        }

        // 基础类型
        let kind = self.peek().kind.clone();
        match &kind {
            TokenKind::TyI(w) => { self.advance(); Ok(Type::I(*w)) }
            TokenKind::TyU(w) => { self.advance(); Ok(Type::U(*w)) }
            TokenKind::TyF(w) => { self.advance(); Ok(Type::F(*w)) }
            TokenKind::TyBool => { self.advance(); Ok(Type::Bool) }
            TokenKind::TyStr => { self.advance(); Ok(Type::Str) }
            TokenKind::TyChar => { self.advance(); Ok(Type::Char) }
            TokenKind::TyNever => { self.advance(); Ok(Type::Never) }
            TokenKind::KwVoid => { self.advance(); Ok(Type::Void) }
            TokenKind::KwAnytype => { self.advance(); Ok(Type::Anytype) }
            TokenKind::KwUpperSelf => {
                self.advance();
                let mut path = vec!["Self".to_string()];
                while self.match_kind(&TokenKind::ColonColon) {
                    path.push(self.consume_ident("expected path segment")?);
                }
                let generics = if self.match_kind(&TokenKind::Lt) {
                    self.parse_generic_args()?
                } else {
                    vec![]
                };
                Ok(Type::Named { path, generics })
            }
            TokenKind::KwFn => self.parse_function_type(),
            TokenKind::Ident(_) => {
                let first = self.consume_ident("expected type name")?;
                let mut path = vec![first];
                while self.match_kind(&TokenKind::ColonColon) {
                    path.push(self.consume_ident("expected path segment")?);
                }
                let generics = if self.match_kind(&TokenKind::Lt) {
                    self.parse_generic_args()?
                } else {
                    vec![]
                };
                Ok(Type::Named { path, generics })
            }
            _ => Err(crate::error::CompileError::type_error(format!(
                "expected type, found {:?}", kind
            ))),
        }
    }

    /// 解析函数类型 `fn(params) -> ret`。
    fn parse_function_type(&mut self) -> Result<Type, crate::error::CompileError> {
        self.advance(); // fn
        let is_async = self.match_kind(&TokenKind::KwAsync);
        self.consume(&TokenKind::LParen, "expected '(' after 'fn'")?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            params.push(self.parse_type()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::RParen) {
                    break;
                }
                params.push(self.parse_type()?);
            }
        }
        self.consume(&TokenKind::RParen, "expected ')'")?;
        let ret = if self.match_kind(&TokenKind::Arrow) {
            Box::new(self.parse_type()?)
        } else {
            Box::new(Type::Void)
        };
        Ok(Type::Function { params, ret, is_async, abi: Abi::Zr })
    }

    /// 解析泛型实参 `<T, U, 42>`。
    pub(crate) fn parse_generic_args(&mut self) -> Result<Vec<Type>, crate::error::CompileError> {
        let mut args = Vec::new();
        if !self.check(&TokenKind::Gt) {
            args.push(self.parse_type()?);
            while self.match_kind(&TokenKind::Comma) {
                if self.check(&TokenKind::Gt) {
                    break;
                }
                args.push(self.parse_type()?);
            }
        }
        self.consume(&TokenKind::Gt, "expected '>' to close generic arguments")?;
        Ok(args)
    }
}
